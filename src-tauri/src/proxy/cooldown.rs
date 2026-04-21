//! Provider 短冷却跟踪
//!
//! 作用：一个 key 刚刚发生过"瞬时失败"（429 / 5xx / 超时 / 连接错误等可重试错误），
//! 在选路阶段**立刻跳过**它一小段时间，让流量自然轮到其他 key，而不是在同一个坏 key 上
//! 反复重试。
//!
//! 设计原则：
//! - 只判断"可重试失败"，不区分具体错误类别——任何不是权限/参数错误的瞬时问题都冷却
//! - 冷却时长优先使用上游 `Retry-After` 头；没有就按连续失败次数做指数退避
//! - 抖动 50~100% 防止多个客户端同步重试风暴
//! - 与通用熔断器解耦：熔断器负责"长期健康"，本模块负责"短期跳过"

use http::HeaderMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

/// 指数退避下限（无 Retry-After 提示时的首跳冷却时长）
const DEFAULT_BASE_COOLDOWN_MS: u64 = 1_000;
/// 指数退避上限
const DEFAULT_MAX_COOLDOWN_MS: u64 = 60_000;
/// 服务端 `Retry-After` 允许兑现的最大时长（防止被恶意过长值卡死）
const HINT_MAX_MS: u64 = 5 * 60 * 1_000;

#[derive(Debug, Clone)]
struct CooldownState {
    cooldown_until: Instant,
    consecutive_failures: u32,
}

/// 按 `app_type:provider_id` 粒度跟踪每个 key 的短冷却
#[derive(Default)]
pub struct CooldownTracker {
    states: RwLock<HashMap<String, CooldownState>>,
}

impl CooldownTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            states: RwLock::new(HashMap::new()),
        })
    }

    /// 是否仍在冷却期（选路阶段用来过滤候选）
    pub async fn is_cooling_down(&self, app_type: &str, provider_id: &str) -> bool {
        let key = make_key(app_type, provider_id);
        let states = self.states.read().await;
        match states.get(&key) {
            Some(s) => Instant::now() < s.cooldown_until,
            None => false,
        }
    }

    /// 记录一次瞬时失败，返回本次冷却时长（ms）
    ///
    /// `hint_ms` 来自上游响应头（如 `Retry-After`）；没有就按连续失败次数退避。
    pub async fn record_failure(
        &self,
        app_type: &str,
        provider_id: &str,
        hint_ms: Option<u64>,
    ) -> u64 {
        let key = make_key(app_type, provider_id);
        let mut states = self.states.write().await;
        let entry = states.entry(key).or_insert_with(|| CooldownState {
            cooldown_until: Instant::now(),
            consecutive_failures: 0,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        let cooldown_ms = compute_cooldown_ms(entry.consecutive_failures, hint_ms);
        entry.cooldown_until = Instant::now() + Duration::from_millis(cooldown_ms);
        cooldown_ms
    }

    /// 成功一次：清除冷却和连续失败计数
    pub async fn clear(&self, app_type: &str, provider_id: &str) {
        let key = make_key(app_type, provider_id);
        let mut states = self.states.write().await;
        states.remove(&key);
    }

    /// 冷却剩余时间（毫秒），无冷却返回 0
    #[allow(dead_code)]
    pub async fn remaining_ms(&self, app_type: &str, provider_id: &str) -> u64 {
        let key = make_key(app_type, provider_id);
        let states = self.states.read().await;
        states
            .get(&key)
            .map(|s| {
                s.cooldown_until
                    .saturating_duration_since(Instant::now())
                    .as_millis() as u64
            })
            .unwrap_or(0)
    }
}

fn make_key(app_type: &str, provider_id: &str) -> String {
    format!("{app_type}:{provider_id}")
}

/// 计算冷却时长
///
/// - 优先使用 `hint_ms`（服务器冷却建议），截断到 [base, HINT_MAX]
/// - 否则按连续失败次数做指数退避：`base * 2^(n-1)`，上限 max
/// - 叠加 50~100% 抖动避免多客户端/多请求同步重试
fn compute_cooldown_ms(consecutive_failures: u32, hint_ms: Option<u64>) -> u64 {
    if let Some(ms) = hint_ms {
        return ms.clamp(DEFAULT_BASE_COOLDOWN_MS, HINT_MAX_MS);
    }
    let exp = consecutive_failures.saturating_sub(1).min(10);
    let ideal = DEFAULT_BASE_COOLDOWN_MS
        .saturating_mul(1u64 << exp)
        .min(DEFAULT_MAX_COOLDOWN_MS);
    let jitter = 0.5 + 0.5 * jitter_unit_f64();
    ((ideal as f64) * jitter)
        .clamp(DEFAULT_BASE_COOLDOWN_MS as f64, DEFAULT_MAX_COOLDOWN_MS as f64) as u64
}

/// 解析 `Retry-After` 头，支持整数秒或 HTTP-date 格式
pub fn parse_retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get(http::header::RETRY_AFTER)?.to_str().ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(secs) = trimmed.parse::<f64>() {
        if secs.is_finite() && secs >= 0.0 {
            return Some((secs * 1000.0) as u64);
        }
    }
    if let Ok(target) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as i128;
        let target_ms = target.timestamp_millis() as i128;
        let diff = target_ms - now_ms;
        if diff > 0 {
            return Some(diff.min(HINT_MAX_MS as i128) as u64);
        }
        return Some(0);
    }
    None
}

/// 基于系统纳秒时间的 [0, 1) 伪随机值，用于退避抖动
fn jitter_unit_f64() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1_000_000) as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hint_is_honored() {
        let tracker = CooldownTracker::new();
        tracker.record_failure("claude", "p1", Some(2_000)).await;
        assert!(tracker.is_cooling_down("claude", "p1").await);
        let remain = tracker.remaining_ms("claude", "p1").await;
        assert!(remain > 1_500 && remain <= 2_000);
    }

    #[tokio::test]
    async fn exponential_backoff_without_hint() {
        let tracker = CooldownTracker::new();
        let first = tracker.record_failure("claude", "p1", None).await;
        let second = tracker.record_failure("claude", "p1", None).await;
        assert!(first >= DEFAULT_BASE_COOLDOWN_MS / 2);
        assert!(second >= first / 2);
    }

    #[tokio::test]
    async fn clear_removes_cooldown() {
        let tracker = CooldownTracker::new();
        tracker.record_failure("claude", "p1", Some(5_000)).await;
        assert!(tracker.is_cooling_down("claude", "p1").await);
        tracker.clear("claude", "p1").await;
        assert!(!tracker.is_cooling_down("claude", "p1").await);
    }

    #[test]
    fn parse_retry_after_seconds() {
        let mut h = HeaderMap::new();
        h.insert(http::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(parse_retry_after_ms(&h).unwrap(), 3_000);
    }

    #[test]
    fn parse_retry_after_missing() {
        assert!(parse_retry_after_ms(&HeaderMap::new()).is_none());
    }

    #[test]
    fn hint_caps_at_safe_max() {
        let ms = compute_cooldown_ms(1, Some(u64::MAX));
        assert!(ms <= HINT_MAX_MS);
    }
}
