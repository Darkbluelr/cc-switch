//! Provider 健康度指标命令
//!
//! 对外暴露 `get_provider_health_metrics`，前端用于展示每个 provider 的
//! 缓存命中率、假 200 率、成功率、TTFT 等运行态指标。

use crate::database::ProviderHealthMetrics;
use crate::store::AppState;

/// 默认样本窗口：最近 30 分钟
const DEFAULT_WINDOW_SECONDS: u64 = 1800;
/// 窗口上限：不允许一次性扫过 7 天的日志，防止误用导致大表全扫
const MAX_WINDOW_SECONDS: u64 = 7 * 24 * 3600;
/// 窗口下限：至少 1 分钟，避免抽样过稀
const MIN_WINDOW_SECONDS: u64 = 60;

/// 拉取指定应用下 per-provider 的健康度指标
///
/// # 参数
/// - `app_type`: "claude" / "codex" / "gemini"
/// - `window_seconds`: 统计窗口（秒）。省略时用默认 30 分钟。会被 clamp 到 [60, 7d]。
///
/// # 返回
/// 每个"窗口内有样本的"provider 一条记录。调用方应以自己的 provider 列表为准
/// 与返回结果左合并（无样本 = 无数据，不是"全部失败"）。
#[tauri::command]
pub async fn get_provider_health_metrics(
    state: tauri::State<'_, AppState>,
    app_type: String,
    window_seconds: Option<u64>,
) -> Result<Vec<ProviderHealthMetricsView>, String> {
    let window = window_seconds
        .unwrap_or(DEFAULT_WINDOW_SECONDS)
        .clamp(MIN_WINDOW_SECONDS, MAX_WINDOW_SECONDS);

    let raw = state
        .db
        .get_provider_health_metrics(&app_type, window)
        .map_err(|e| e.to_string())?;

    Ok(raw.into_iter().map(ProviderHealthMetricsView::from).collect())
}

/// 视图对象：在 DAO 原始计数之上加了派生比例，方便前端直接展示
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderHealthMetricsView {
    pub provider_id: String,
    pub app_type: String,
    pub window_seconds: u64,
    pub total_requests: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub fake_200_count: u64,
    pub streaming_200_count: u64,
    pub cache_read_tokens: u64,
    pub prompt_total_tokens: u64,
    /// 成功率 [0.0, 1.0]，无样本时为 null
    pub success_rate: Option<f64>,
    /// 假 200 率 [0.0, 1.0]，无流式 200 样本时为 null
    pub fake_200_rate: Option<f64>,
    /// 缓存命中率 [0.0, 1.0]，无 prompt token 时为 null
    pub cache_hit_rate: Option<f64>,
    pub avg_first_token_ms: Option<u64>,
    pub max_first_token_ms: Option<u64>,
}

impl From<ProviderHealthMetrics> for ProviderHealthMetricsView {
    fn from(m: ProviderHealthMetrics) -> Self {
        let success_rate = m.success_rate();
        let fake_200_rate = m.fake_200_rate();
        let cache_hit_rate = m.cache_hit_rate();
        Self {
            provider_id: m.provider_id,
            app_type: m.app_type,
            window_seconds: m.window_seconds,
            total_requests: m.total_requests,
            success_count: m.success_count,
            error_count: m.error_count,
            fake_200_count: m.fake_200_count,
            streaming_200_count: m.streaming_200_count,
            cache_read_tokens: m.cache_read_tokens,
            prompt_total_tokens: m.prompt_total_tokens,
            success_rate,
            fake_200_rate,
            cache_hit_rate,
            avg_first_token_ms: m.avg_first_token_ms,
            max_first_token_ms: m.max_first_token_ms,
        }
    }
}
