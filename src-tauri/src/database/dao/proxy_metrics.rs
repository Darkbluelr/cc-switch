//! Provider 健康度指标 DAO
//!
//! 在最近 N 秒的 `proxy_request_logs` 上做 per-provider 聚合，返回足够
//! 上层做决策的原始计数和派生比例（缓存命中率、假 200 率、成功率、TTFT）。
//!
//! ## 缓存命中率口径
//!
//! cc-switch 按 API 类型存储 `input_tokens` 的语义不同：
//! - Codex 流式（Responses API，`parser::from_codex_response`）：`input_tokens`
//!   为**总 prompt（已包含 cached）**；命中率 = `cache_read / input`
//! - Codex 非流式：`input_tokens` 已减去 cached；命中率 = `cache_read / (input + cache_read)`
//! - Claude：`input_tokens` 为净 uncached 输入，`cache_read` 与
//!   `cache_creation` 分别独立；命中率 = `cache_read / (input + cache_read + cache_creation)`
//! - 其他：按 Codex 流式口径处理（保守选择）
//!
//! 因此 SQL 里对"命中率分母"做 CASE 分支，保证不同 app_type 的统计都正确。

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use serde::{Deserialize, Serialize};

/// Provider 健康度指标（视图对象，直接序列化给前端）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderHealthMetrics {
    pub provider_id: String,
    pub app_type: String,
    /// 样本窗口（秒）
    pub window_seconds: u64,
    /// 窗口内总请求数
    pub total_requests: u64,
    /// 2xx 请求数
    pub success_count: u64,
    /// 非 2xx 请求数
    pub error_count: u64,
    /// 假 200：状态 200 + 流式 + 三类 token 全 0（通常是上游中途关流）
    pub fake_200_count: u64,
    /// 流式 200 总数（假 200 率的分母）
    pub streaming_200_count: u64,
    /// 缓存命中 token（分子）
    pub cache_read_tokens: u64,
    /// 按 app_type 语义修正后的"总 prompt"（分母）
    pub prompt_total_tokens: u64,
    /// first_token_ms 的均值（仅统计 first_token_ms 非 NULL 的样本）
    pub avg_first_token_ms: Option<u64>,
    /// first_token_ms 的最大值（反映最差首字节延迟）
    pub max_first_token_ms: Option<u64>,
}

impl ProviderHealthMetrics {
    /// 成功率（2xx 占比，0.0 ~ 1.0）
    pub fn success_rate(&self) -> Option<f64> {
        if self.total_requests == 0 {
            None
        } else {
            Some(self.success_count as f64 / self.total_requests as f64)
        }
    }

    /// 假 200 率（在流式 200 中的占比，0.0 ~ 1.0）
    pub fn fake_200_rate(&self) -> Option<f64> {
        if self.streaming_200_count == 0 {
            None
        } else {
            Some(self.fake_200_count as f64 / self.streaming_200_count as f64)
        }
    }

    /// 缓存命中率（cache_read / app_type 对应的"总 prompt"，0.0 ~ 1.0）
    pub fn cache_hit_rate(&self) -> Option<f64> {
        if self.prompt_total_tokens == 0 {
            None
        } else {
            Some(self.cache_read_tokens as f64 / self.prompt_total_tokens as f64)
        }
    }
}

impl Database {
    /// 拉取指定 app_type 在最近 `window_seconds` 内的 per-provider 指标
    ///
    /// 窗口边界以 `proxy_request_logs.created_at`（秒级 Unix 时间戳）与当前 wall-clock 比较。
    /// 没有样本的 provider 不会出现在结果里 —— 调用方应以自己的 provider 列表为主键合并。
    pub fn get_provider_health_metrics(
        &self,
        app_type: &str,
        window_seconds: u64,
    ) -> Result<Vec<ProviderHealthMetrics>, AppError> {
        // SQLite CASE WHEN 需要读到 app_type，这里绑定两次（?1 用于 WHERE，?3 用于 CASE）
        // 也可以只绑一次用 ?1，两处都引用；我们采用后者。
        let sql = "
            SELECT
                provider_id,
                COUNT(*) AS total,
                SUM(CASE WHEN status_code BETWEEN 200 AND 299 THEN 1 ELSE 0 END) AS success,
                SUM(CASE WHEN status_code < 200 OR status_code >= 300 THEN 1 ELSE 0 END) AS errors,
                SUM(CASE
                    WHEN status_code = 200 AND is_streaming = 1
                      AND (input_tokens + cache_read_tokens + output_tokens) = 0
                    THEN 1 ELSE 0
                END) AS fake_200,
                SUM(CASE
                    WHEN status_code = 200 AND is_streaming = 1 THEN 1 ELSE 0
                END) AS streaming_200,
                SUM(cache_read_tokens) AS cache_read,
                SUM(
                    CASE
                        WHEN ?1 = 'codex' AND is_streaming = 1 THEN input_tokens
                        WHEN ?1 = 'codex' THEN input_tokens + cache_read_tokens
                        WHEN ?1 = 'claude' THEN
                            input_tokens + cache_read_tokens + cache_creation_tokens
                        ELSE input_tokens
                    END
                ) AS prompt_total,
                SUM(COALESCE(first_token_ms, 0)) AS sum_ttft,
                SUM(CASE WHEN first_token_ms IS NOT NULL THEN 1 ELSE 0 END) AS cnt_ttft,
                MAX(first_token_ms) AS max_ttft
            FROM proxy_request_logs
            WHERE app_type = ?1
              AND created_at > ?2
              -- 只算真实走过代理的请求，排除本地 JSONL 日志同步（session_log）
              -- 那些记录 provider_id 是占位符（_codex_session 等），不代表失败
              AND data_source = 'proxy'
            GROUP BY provider_id
        ";

        let cutoff = current_unix_seconds().saturating_sub(window_seconds as i64);

        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(rusqlite::params![app_type, cutoff], |row| {
                let provider_id: String = row.get(0)?;
                let total: i64 = row.get(1)?;
                let success: i64 = row.get(2)?;
                let errors: i64 = row.get(3)?;
                let fake_200: i64 = row.get(4)?;
                let streaming_200: i64 = row.get(5)?;
                let cache_read: i64 = row.get(6)?;
                let prompt_total: i64 = row.get(7)?;
                let sum_ttft: i64 = row.get(8)?;
                let cnt_ttft: i64 = row.get(9)?;
                let max_ttft: Option<i64> = row.get(10)?;

                let avg_ttft = if cnt_ttft > 0 {
                    Some((sum_ttft / cnt_ttft) as u64)
                } else {
                    None
                };

                Ok(ProviderHealthMetrics {
                    provider_id,
                    app_type: app_type.to_string(),
                    window_seconds,
                    total_requests: total.max(0) as u64,
                    success_count: success.max(0) as u64,
                    error_count: errors.max(0) as u64,
                    fake_200_count: fake_200.max(0) as u64,
                    streaming_200_count: streaming_200.max(0) as u64,
                    cache_read_tokens: cache_read.max(0) as u64,
                    prompt_total_tokens: prompt_total.max(0) as u64,
                    avg_first_token_ms: avg_ttft,
                    max_first_token_ms: max_ttft.filter(|v| *v >= 0).map(|v| v as u64),
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut result = Vec::new();
        for row in rows {
            match row {
                Ok(m) => result.push(m),
                Err(e) => {
                    log::warn!("[proxy_metrics] 解析单行失败（已跳过）: {e}");
                }
            }
        }
        Ok(result)
    }
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_log(
        conn: &rusqlite::Connection,
        provider_id: &str,
        app_type: &str,
        status_code: i32,
        is_streaming: bool,
        input_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        output_tokens: u64,
        first_token_ms: Option<u64>,
        created_at: i64,
    ) {
        insert_log_with_source(
            conn,
            provider_id,
            app_type,
            status_code,
            is_streaming,
            input_tokens,
            cache_read,
            cache_creation,
            output_tokens,
            first_token_ms,
            created_at,
            "proxy",
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_log_with_source(
        conn: &rusqlite::Connection,
        provider_id: &str,
        app_type: &str,
        status_code: i32,
        is_streaming: bool,
        input_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        output_tokens: u64,
        first_token_ms: Option<u64>,
        created_at: i64,
        data_source: &str,
    ) {
        conn.execute(
            "INSERT INTO proxy_request_logs (
                request_id, provider_id, app_type, model,
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd,
                total_cost_usd, latency_ms, first_token_ms, status_code,
                is_streaming, cost_multiplier, created_at, data_source
            ) VALUES (?1, ?2, ?3, 'gpt-5.2', ?4, ?5, ?6, ?7, '0', '0', '0', '0', '0', 100, ?8, ?9, ?10, '1', ?11, ?12)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                provider_id,
                app_type,
                input_tokens,
                output_tokens,
                cache_read,
                cache_creation,
                first_token_ms,
                status_code,
                if is_streaming { 1 } else { 0 },
                created_at,
                data_source,
            ],
        )
        .expect("insert log");
    }

    fn now() -> i64 {
        current_unix_seconds()
    }

    #[test]
    fn codex_streaming_cache_hit_rate_uses_input_as_denominator() {
        let db = Database::memory().unwrap();
        let t = now();
        {
            let conn = db.conn.lock().unwrap();
            // Codex 流式：input=1000（已含 cached），cache_read=970 → 命中率 97%
            insert_log(
                &conn,
                "p1",
                "codex",
                200,
                true,
                1000,
                970,
                0,
                50,
                Some(500),
                t,
            );
            insert_log(
                &conn,
                "p1",
                "codex",
                200,
                true,
                2000,
                1940,
                0,
                100,
                Some(600),
                t,
            );
        }

        let metrics = db.get_provider_health_metrics("codex", 300).unwrap();
        let m = metrics.iter().find(|m| m.provider_id == "p1").unwrap();

        assert_eq!(m.total_requests, 2);
        assert_eq!(m.success_count, 2);
        assert_eq!(m.cache_read_tokens, 970 + 1940);
        assert_eq!(m.prompt_total_tokens, 1000 + 2000);
        let rate = m.cache_hit_rate().unwrap();
        assert!(rate > 0.96 && rate < 0.98, "expected ~97%, got {rate}");
    }

    #[test]
    fn claude_cache_hit_rate_includes_cache_creation_in_denominator() {
        let db = Database::memory().unwrap();
        let t = now();
        {
            let conn = db.conn.lock().unwrap();
            // Claude：input=100（净 uncached），cache_read=900，cache_creation=200
            //   → 分母 = 100+900+200 = 1200，命中率 = 900/1200 = 75%
            insert_log(
                &conn,
                "p1",
                "claude",
                200,
                true,
                100,
                900,
                200,
                50,
                Some(400),
                t,
            );
        }

        let metrics = db.get_provider_health_metrics("claude", 300).unwrap();
        let m = metrics.iter().find(|m| m.provider_id == "p1").unwrap();

        assert_eq!(m.cache_read_tokens, 900);
        assert_eq!(m.prompt_total_tokens, 100 + 900 + 200);
        let rate = m.cache_hit_rate().unwrap();
        assert!((rate - 0.75).abs() < 0.01, "expected ~75%, got {rate}");
    }

    #[test]
    fn fake_200_detection_only_streaming_with_zero_tokens() {
        let db = Database::memory().unwrap();
        let t = now();
        {
            let conn = db.conn.lock().unwrap();
            // 正常 200 流式
            insert_log(
                &conn,
                "p1",
                "codex",
                200,
                true,
                1000,
                900,
                0,
                100,
                Some(500),
                t,
            );
            // 假 200（所有 token 都是 0）
            insert_log(&conn, "p1", "codex", 200, true, 0, 0, 0, 0, Some(30000), t);
            // 非流式 200 即便 0 token 也不算假 200
            insert_log(&conn, "p1", "codex", 200, false, 0, 0, 0, 0, None, t);
            // 非 200 不算
            insert_log(&conn, "p1", "codex", 429, true, 0, 0, 0, 0, None, t);
        }

        let metrics = db.get_provider_health_metrics("codex", 300).unwrap();
        let m = metrics.iter().find(|m| m.provider_id == "p1").unwrap();

        assert_eq!(m.total_requests, 4);
        assert_eq!(m.success_count, 3);
        assert_eq!(m.error_count, 1);
        assert_eq!(m.streaming_200_count, 2);
        assert_eq!(m.fake_200_count, 1);
        let rate = m.fake_200_rate().unwrap();
        assert!((rate - 0.5).abs() < 0.001, "expected 50%, got {rate}");
    }

    #[test]
    fn window_filter_excludes_old_rows() {
        let db = Database::memory().unwrap();
        let t = now();
        {
            let conn = db.conn.lock().unwrap();
            insert_log(
                &conn,
                "p1",
                "codex",
                200,
                true,
                100,
                90,
                0,
                10,
                Some(100),
                t,
            );
            // 2 小时前（窗口 30min 应该看不到）
            insert_log(
                &conn,
                "p1",
                "codex",
                200,
                true,
                100,
                90,
                0,
                10,
                Some(100),
                t - 7200,
            );
        }

        let m = db
            .get_provider_health_metrics("codex", 1800)
            .unwrap()
            .into_iter()
            .find(|m| m.provider_id == "p1")
            .unwrap();
        assert_eq!(m.total_requests, 1);
    }

    #[test]
    fn no_samples_returns_empty_vec() {
        let db = Database::memory().unwrap();
        let metrics = db.get_provider_health_metrics("codex", 300).unwrap();
        assert!(metrics.is_empty());
    }

    #[test]
    fn session_log_records_are_excluded_from_metrics() {
        let db = Database::memory().unwrap();
        let t = now();
        {
            let conn = db.conn.lock().unwrap();
            // 真实代理记录
            insert_log(
                &conn,
                "p1",
                "codex",
                200,
                true,
                1000,
                900,
                0,
                100,
                Some(500),
                t,
            );
            // session_log 同一请求的"镜像"记录（占位 provider_id，0 tokens 是常见情况）
            insert_log_with_source(
                &conn,
                "_codex_session",
                "codex",
                200,
                true,
                0,
                0,
                0,
                0,
                None,
                t,
                "session_log",
            );
            insert_log_with_source(
                &conn,
                "p1",
                "codex",
                200,
                true,
                500,
                400,
                0,
                50,
                Some(300),
                t,
                "session_log",
            );
        }

        let metrics = db.get_provider_health_metrics("codex", 300).unwrap();
        // 仅应出现 p1，来自 proxy；_codex_session / p1(session_log) 均被过滤掉
        assert_eq!(metrics.len(), 1);
        let m = &metrics[0];
        assert_eq!(m.provider_id, "p1");
        assert_eq!(m.total_requests, 1);
        assert_eq!(m.cache_read_tokens, 900);
        assert_eq!(m.prompt_total_tokens, 1000);
    }

    #[test]
    fn ttft_aggregates_avg_and_max() {
        let db = Database::memory().unwrap();
        let t = now();
        {
            let conn = db.conn.lock().unwrap();
            insert_log(
                &conn,
                "p1",
                "codex",
                200,
                true,
                100,
                90,
                0,
                10,
                Some(1000),
                t,
            );
            insert_log(
                &conn,
                "p1",
                "codex",
                200,
                true,
                100,
                90,
                0,
                10,
                Some(3000),
                t,
            );
            insert_log(&conn, "p1", "codex", 200, true, 100, 90, 0, 10, None, t);
        }

        let m = db
            .get_provider_health_metrics("codex", 300)
            .unwrap()
            .into_iter()
            .find(|m| m.provider_id == "p1")
            .unwrap();
        // 有 TTFT 的两条样本，均值 = (1000+3000)/2 = 2000
        assert_eq!(m.avg_first_token_ms, Some(2000));
        assert_eq!(m.max_first_token_ms, Some(3000));
    }
}
