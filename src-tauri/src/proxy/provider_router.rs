//! 供应商路由器模块
//!
//! 负责选择和管理代理目标供应商，实现智能故障转移

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::circuit_breaker::{AllowResult, CircuitBreaker, CircuitBreakerConfig};
use crate::proxy::cooldown::CooldownTracker;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// 把字符串稳定映射到 [0, len) 的偏移量（session 亲和用）
fn hash_to_offset(key: &str, len: usize) -> usize {
    if len <= 1 {
        return 0;
    }
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % len
}

/// 供应商路由器
pub struct ProviderRouter {
    /// 数据库连接
    db: Arc<Database>,
    /// 熔断器管理器 - key 格式: "app_type:provider_id"
    circuit_breakers: Arc<RwLock<HashMap<String, Arc<CircuitBreaker>>>>,
    /// 瞬时失败短冷却跟踪器：任何可重试错误都短暂跳过该 key，与长期熔断解耦
    cooldown: Arc<CooldownTracker>,
    /// Round-Robin 游标 - 每个 app_type 一个，作用于"可用候选列表"的起点
    ///
    /// 当候选数 > 1 且启用故障转移时，在选路阶段按请求轮转起点，
    /// 使流量在多个等价 key 之间天然均摊，避免"永远先砸 P1"。
    round_robin_cursors: Arc<RwLock<HashMap<String, Arc<AtomicUsize>>>>,
}

impl ProviderRouter {
    /// 创建新的供应商路由器
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            circuit_breakers: Arc::new(RwLock::new(HashMap::new())),
            cooldown: CooldownTracker::new(),
            round_robin_cursors: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 获取冷却跟踪器（供 forwarder 在瞬时失败时直接记录）
    #[allow(dead_code)]
    pub fn cooldown(&self) -> Arc<CooldownTracker> {
        self.cooldown.clone()
    }

    /// 记录一次瞬时失败，返回本次冷却时长（ms）
    ///
    /// 调用方应对"可重试"错误统一调用此方法（429/5xx/超时/连接失败等），
    /// 它只影响短期选路，不污染熔断器健康统计；熔断器由独立的 `record_result`
    /// 按"真失败"口径累积。
    pub async fn record_transient_failure(
        &self,
        provider_id: &str,
        app_type: &str,
        hint_ms: Option<u64>,
    ) -> u64 {
        self.cooldown
            .record_failure(app_type, provider_id, hint_ms)
            .await
    }

    /// 请求成功：清空该 provider 的短冷却状态
    pub async fn clear_transient_failure(&self, provider_id: &str, app_type: &str) {
        self.cooldown.clear(app_type, provider_id).await;
    }

    /// 当 failover 队列内全部 provider 都在冷却时，返回"最早解冻剩余时间"（毫秒）
    ///
    /// 用于把"所有供应商暂不可用"的 503 返回附带合理的 `Retry-After`，
    /// 告诉客户端不用立刻重试，减少 CLI 端 retry budget 的无效消耗。
    ///
    /// - `Some(ms)`：队列非空且全部处在冷却中，`ms` 是最快解冻剩余时间
    /// - `None`：队列为空 / 有可用候选 / 数据库查询失败 —— 不下发 Retry-After
    pub async fn earliest_cooldown_remaining_ms(&self, app_type: &str) -> Option<u64> {
        let ids: Vec<String> = self
            .db
            .get_failover_queue(app_type)
            .ok()?
            .into_iter()
            .map(|item| item.provider_id)
            .collect();
        self.cooldown
            .earliest_remaining_ms_all(app_type, &ids)
            .await
    }

    /// 选择可用的供应商（支持故障转移 + 会话亲和 + Round-Robin）
    ///
    /// 返回按优先级排序的可用供应商列表：
    /// - 故障转移关闭时：仅返回当前供应商
    /// - 故障转移开启时：
    ///   - 先按 `failover_tier` 分层（1=最优先），只在同层内做均摊/亲和
    ///   - 只有当前层内**无任何可用候选**时，才降级到下一层
    ///   - 结果列表会按层级拼接：P1 全部候选 → P2 全部候选 → …
    ///     forwarder 会按该顺序逐个尝试，从而实现“同级耗尽再降级”
    ///
    /// 层内起点策略：
    /// 1. `session_key` 有值 → 一致性哈希起点（让同一会话尽量命中同一 key，利好 prompt cache）
    /// 2. 否则 → Round-Robin 游标起点（跨会话均摊流量）
    ///
    /// `session_key` 只应在客户端明确提供了会话标识时传入（比如 Claude 的
    /// `x-claude-code-session-id` 或 Codex 的 `session_id` 头），内部新生成的 UUID
    /// 对缓存命中没帮助，应传 `None` 走 RR。
    pub async fn select_providers(
        &self,
        app_type: &str,
        session_key: Option<&str>,
    ) -> Result<Vec<Provider>, AppError> {
        let mut result = Vec::new();
        let mut total_providers = 0usize;
        let mut circuit_open_count = 0usize;
        let mut rate_limited_count = 0usize;

        // 检查该应用的自动故障转移开关是否开启（从 proxy_config 表读取）
        let auto_failover_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.auto_failover_enabled,
            Err(e) => {
                log::error!("[{app_type}] 读取 proxy_config 失败: {e}，默认禁用故障转移");
                false
            }
        };

        if auto_failover_enabled {
            // 故障转移开启：按 tier 分层筛选候选，同层内轮转起点
            let all_providers = self.db.get_all_providers(app_type)?;

            // 使用 DAO 返回的排序结果，确保和前端展示一致（tier ASC, sort_index ASC）
            let ordered_items = self.db.get_failover_queue(app_type)?;
            total_providers = ordered_items.len();

            // RR：每次请求只自增一次，避免多 tier 时过快推进
            let rr_base = match session_key {
                Some(key) if !key.is_empty() => None,
                _ => Some(
                    self.get_or_create_rr_cursor(app_type)
                        .await
                        .fetch_add(1, Ordering::Relaxed),
                ),
            };

            let mut tiers: Vec<(usize, Vec<Provider>)> = Vec::new();
            let mut last_tier: Option<usize> = None;

            for item in ordered_items {
                let provider_id = item.provider_id;
                let tier = item.failover_tier.max(1);

                let Some(provider) = all_providers.get(&provider_id).cloned() else {
                    continue;
                };

                // 1) 短冷却：跳过刚失败过、仍在退避期的 provider
                if self.cooldown.is_cooling_down(app_type, &provider.id).await {
                    rate_limited_count += 1;
                    continue;
                }

                // 2) 熔断器：跳过 Open 状态的 provider
                let circuit_key = format!("{app_type}:{}", provider.id);
                let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
                if breaker.is_available().await {
                    if last_tier != Some(tier) {
                        tiers.push((tier, Vec::new()));
                        last_tier = Some(tier);
                    }
                    if let Some((_, providers)) = tiers.last_mut() {
                        providers.push(provider);
                    }
                } else {
                    circuit_open_count += 1;
                }
            }

            // 3) 逐层拼接：同层内轮转起点，层与层之间严格按 tier 顺序降级
            for (_tier, mut providers) in tiers {
                if providers.len() > 1 {
                    let offset = match session_key {
                        Some(key) if !key.is_empty() => hash_to_offset(key, providers.len()),
                        _ => rr_base.unwrap_or(0) as usize % providers.len(),
                    };
                    if offset > 0 {
                        providers.rotate_left(offset);
                    }
                }
                result.extend(providers);
            }
        } else {
            // 故障转移关闭：仅使用当前供应商，跳过熔断器检查
            let current_id = AppType::from_str(app_type)
                .ok()
                .and_then(|app_enum| {
                    crate::settings::get_effective_current_provider(&self.db, &app_enum)
                        .ok()
                        .flatten()
                })
                .or_else(|| self.db.get_current_provider(app_type).ok().flatten());

            if let Some(current_id) = current_id {
                if let Some(current) = self.db.get_provider_by_id(&current_id, app_type)? {
                    total_providers = 1;
                    result.push(current);
                }
            }
        }

        if result.is_empty() {
            if total_providers > 0 && (circuit_open_count + rate_limited_count) == total_providers {
                log::warn!(
                    "[{app_type}] [FO-004] 所有供应商暂不可用（熔断 {circuit_open_count}，冷却 {rate_limited_count}）"
                );
                return Err(AppError::AllProvidersCircuitOpen);
            } else {
                log::warn!("[{app_type}] [FO-005] 未配置供应商");
                return Err(AppError::NoProvidersConfigured);
            }
        }

        Ok(result)
    }

    /// 获取或创建指定 app_type 的 Round-Robin 游标
    async fn get_or_create_rr_cursor(&self, app_type: &str) -> Arc<AtomicUsize> {
        {
            let cursors = self.round_robin_cursors.read().await;
            if let Some(c) = cursors.get(app_type) {
                return c.clone();
            }
        }
        let mut cursors = self.round_robin_cursors.write().await;
        if let Some(c) = cursors.get(app_type) {
            return c.clone();
        }
        let cursor = Arc::new(AtomicUsize::new(0));
        cursors.insert(app_type.to_string(), cursor.clone());
        cursor
    }

    /// 请求执行前获取熔断器“放行许可”
    ///
    /// - Closed：直接放行
    /// - Open：超时到达后切到 HalfOpen 并放行一次探测
    /// - HalfOpen：按限流规则放行探测
    ///
    /// 注意：调用方必须在请求结束后通过 `record_result()` 释放 HalfOpen 名额，
    /// 否则会导致该 Provider 长时间无法进入探测状态。
    pub async fn allow_provider_request(&self, provider_id: &str, app_type: &str) -> AllowResult {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.allow_request().await
    }

    /// 记录供应商请求结果
    pub async fn record_result(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
        success: bool,
        error_msg: Option<String>,
    ) -> Result<(), AppError> {
        // 1. 按应用独立获取熔断器配置
        let failure_threshold = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => app_config.circuit_failure_threshold,
            Err(_) => 5, // 默认值
        };

        // 2. 更新熔断器状态
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

        if success {
            breaker.record_success(used_half_open_permit).await;
        } else {
            breaker.record_failure(used_half_open_permit).await;
        }

        // 3. 更新数据库健康状态（使用配置的阈值）
        self.db
            .update_provider_health_with_threshold(
                provider_id,
                app_type,
                success,
                error_msg.clone(),
                failure_threshold,
            )
            .await?;

        Ok(())
    }

    /// 重置熔断器（手动恢复）
    pub async fn reset_circuit_breaker(&self, circuit_key: &str) {
        let breakers = self.circuit_breakers.read().await;
        if let Some(breaker) = breakers.get(circuit_key) {
            breaker.reset().await;
        }
    }

    /// 重置指定供应商的熔断器
    pub async fn reset_provider_breaker(&self, provider_id: &str, app_type: &str) {
        let circuit_key = format!("{app_type}:{provider_id}");
        self.reset_circuit_breaker(&circuit_key).await;
    }

    /// 仅释放 HalfOpen permit，不影响健康统计（neutral 接口）
    ///
    /// 用于整流器等场景：请求结果不应计入 Provider 健康度，
    /// 但仍需释放占用的探测名额，避免 HalfOpen 状态卡死
    pub async fn release_permit_neutral(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
    ) {
        if !used_half_open_permit {
            return;
        }
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.release_half_open_permit();
    }

    /// 更新所有熔断器的配置（热更新）
    pub async fn update_all_configs(&self, config: CircuitBreakerConfig) {
        let breakers = self.circuit_breakers.read().await;
        for breaker in breakers.values() {
            breaker.update_config(config.clone()).await;
        }
    }

    /// 获取熔断器状态
    #[allow(dead_code)]
    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<crate::proxy::circuit_breaker::CircuitBreakerStats> {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breakers = self.circuit_breakers.read().await;

        if let Some(breaker) = breakers.get(&circuit_key) {
            Some(breaker.get_stats().await)
        } else {
            None
        }
    }

    /// 获取或创建熔断器
    async fn get_or_create_circuit_breaker(&self, key: &str) -> Arc<CircuitBreaker> {
        // 先尝试读锁获取
        {
            let breakers = self.circuit_breakers.read().await;
            if let Some(breaker) = breakers.get(key) {
                return breaker.clone();
            }
        }

        // 如果不存在，获取写锁创建
        let mut breakers = self.circuit_breakers.write().await;

        // 双重检查，防止竞争条件
        if let Some(breaker) = breakers.get(key) {
            return breaker.clone();
        }

        // 从 key 中提取 app_type (格式: "app_type:provider_id")
        let app_type = key.split(':').next().unwrap_or("claude");

        // 按应用独立读取熔断器配置
        let config = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => crate::proxy::circuit_breaker::CircuitBreakerConfig {
                failure_threshold: app_config.circuit_failure_threshold,
                success_threshold: app_config.circuit_success_threshold,
                timeout_seconds: app_config.circuit_timeout_seconds as u64,
                error_rate_threshold: app_config.circuit_error_rate_threshold,
                min_requests: app_config.circuit_min_requests,
            },
            Err(_) => crate::proxy::circuit_breaker::CircuitBreakerConfig::default(),
        };

        let breaker = Arc::new(CircuitBreaker::new(config));
        breakers.insert(key.to_string(), breaker.clone());

        breaker
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TempHome {
        #[allow(dead_code)]
        dir: TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("failed to create temp home");
            let original_home = env::var("HOME").ok();
            let original_userprofile = env::var("USERPROFILE").ok();
            let original_test_home = env::var("CC_SWITCH_TEST_HOME").ok();

            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload settings");

            Self {
                dir,
                original_home,
                original_userprofile,
                original_test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.original_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }

            match &self.original_userprofile {
                Some(value) => env::set_var("USERPROFILE", value),
                None => env::remove_var("USERPROFILE"),
            }

            match &self.original_test_home {
                Some(value) => env::set_var("CC_SWITCH_TEST_HOME", value),
                None => env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_router_creation() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);

        let breaker = router.get_or_create_circuit_breaker("claude:test").await;
        assert!(breaker.allow_request().await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_disabled_uses_current_provider() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude", None).await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_order_ignoring_current() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 设置 sort_index 来控制顺序：b=1, a=2
        let mut provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        provider_a.sort_index = Some(2);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        db.add_to_failover_queue("claude", "b").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude", None).await.unwrap();

        assert_eq!(providers.len(), 2);
        // 故障转移开启时：仅按队列顺序选择（忽略当前供应商）
        assert_eq!(providers[0].id, "b");
        assert_eq!(providers[1].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_only_even_if_current_not_in_queue() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        // 只把 b 加入故障转移队列（模拟“当前供应商不在队列里”的常见配置）
        db.add_to_failover_queue("claude", "b").unwrap();

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude", None).await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");
    }

    #[tokio::test]
    #[serial]
    async fn test_select_providers_does_not_consume_half_open_permit() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();

        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        router
            .record_result("b", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        let providers = router.select_providers("claude", None).await.unwrap();
        assert_eq!(providers.len(), 2);

        assert!(router.allow_provider_request("b", "claude").await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_release_permit_neutral_frees_half_open_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 配置熔断器：1 次失败即熔断，0 秒超时立即进入 HalfOpen
        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        db.save_provider("claude", &provider_a).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        // 触发熔断：1 次失败
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        // 第一次请求：获取 HalfOpen 探测名额
        let first = router.allow_provider_request("a", "claude").await;
        assert!(first.allowed);
        assert!(first.used_half_open_permit);

        // 第二次请求应被拒绝（名额已被占用）
        let second = router.allow_provider_request("a", "claude").await;
        assert!(!second.allowed);

        // 使用 release_permit_neutral 释放名额（不影响健康统计）
        router
            .release_permit_neutral("a", "claude", first.used_half_open_permit)
            .await;

        // 第三次请求应被允许（名额已释放）
        let third = router.allow_provider_request("a", "claude").await;
        assert!(third.allowed);
        assert!(third.used_half_open_permit);
    }

    /// Round-Robin：连续调用 select_providers 时起点应轮转
    #[tokio::test]
    #[serial]
    async fn test_select_providers_round_robin_rotates_starting_point() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 三个 provider，sort_index 固定：a=1, b=2, c=3
        for (id, idx) in [("a", 1usize), ("b", 2), ("c", 3)] {
            let mut p =
                Provider::with_id(id.to_string(), format!("Provider {id}"), json!({}), None);
            p.sort_index = Some(idx);
            db.save_provider("claude", &p).unwrap();
            db.add_to_failover_queue("claude", id).unwrap();
        }

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        let first = router.select_providers("claude", None).await.unwrap();
        let second = router.select_providers("claude", None).await.unwrap();
        let third = router.select_providers("claude", None).await.unwrap();

        // 三次调用的"起点"应分别是 a, b, c（RR 轮转）
        assert_eq!(
            first.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(
            second.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
        assert_eq!(
            third.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
    }

    /// 冷却过滤：刚失败过的 provider 在冷却期内应被 select 跳过
    #[tokio::test]
    #[serial]
    async fn test_select_providers_skips_providers_in_cooldown() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let mut provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        provider_a.sort_index = Some(1);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(2);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        // 给 a 一个较长的冷却，确保 select 窗口内它一定被过滤
        router
            .record_transient_failure("a", "claude", Some(10_000))
            .await;

        let providers = router.select_providers("claude", None).await.unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");

        // 显式清除后应重新可选
        router.clear_transient_failure("a", "claude").await;
        let providers = router.select_providers("claude", None).await.unwrap();
        assert_eq!(providers.len(), 2);
    }

    /// 会话亲和：客户端提供 session_key 时，同一 key 稳定映射到同一起点；
    /// 起点 provider 被冷却后，哈希会自然落到剩余候选的另一个
    #[tokio::test]
    #[serial]
    async fn test_select_providers_session_affinity_is_stable_and_falls_back() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        for (id, idx) in [("a", 1usize), ("b", 2), ("c", 3)] {
            let mut p =
                Provider::with_id(id.to_string(), format!("Provider {id}"), json!({}), None);
            p.sort_index = Some(idx);
            db.save_provider("claude", &p).unwrap();
            db.add_to_failover_queue("claude", id).unwrap();
        }

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let key = Some("session-abc-123");

        // 同一 session_key 多次调用应得到相同起点（稳定亲和）
        let first = router.select_providers("claude", key).await.unwrap();
        let second = router.select_providers("claude", key).await.unwrap();
        assert_eq!(first[0].id, second[0].id);

        // 起点 provider 进入冷却后，哈希会落到剩余候选中的一个，不会卡死
        router
            .record_transient_failure(&first[0].id, "claude", Some(10_000))
            .await;
        let third = router.select_providers("claude", key).await.unwrap();
        assert_eq!(third.len(), 2);
        assert_ne!(third[0].id, first[0].id);
    }

    /// 冷却 + 熔断同时用尽时应返回 AllProvidersCircuitOpen
    #[tokio::test]
    #[serial]
    async fn test_select_providers_all_unavailable_errors_out() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        db.save_provider("claude", &provider_a).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        router
            .record_transient_failure("a", "claude", Some(10_000))
            .await;

        let err = router.select_providers("claude", None).await.unwrap_err();
        assert!(matches!(err, AppError::AllProvidersCircuitOpen));
    }
}
