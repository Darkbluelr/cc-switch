use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("服务器已在运行")]
    AlreadyRunning,

    #[error("服务器未运行")]
    NotRunning,

    #[error("地址绑定失败: {0}")]
    BindFailed(String),

    #[error("停止超时")]
    StopTimeout,

    #[error("停止失败: {0}")]
    StopFailed(String),

    #[error("请求转发失败: {0}")]
    ForwardFailed(String),

    #[error("无可用的Provider")]
    NoAvailableProvider,

    #[error("所有供应商暂不可用（全部处在冷却/熔断中）")]
    AllProvidersCircuitOpen {
        /// 最早解冻剩余时间（毫秒）。用于给客户端下发 `Retry-After` 头，
        /// 避免客户端在冷却窗口内徒劳重试把自己的 retry budget 打光。
        retry_after_ms: Option<u64>,
    },

    #[error("未配置供应商")]
    NoProvidersConfigured,

    #[allow(dead_code)]
    #[error("Provider不健康: {0}")]
    ProviderUnhealthy(String),

    #[error("上游错误 (状态码 {status}): {body:?}")]
    UpstreamError {
        status: u16,
        body: Option<String>,
        /// 上游 `Retry-After` 头（毫秒）。仅做"冷却时长建议"，与状态码无关 ——
        /// 上游通常在 429/503 上发送，但路由层处理逻辑对所有可重试错误一致。
        retry_after_ms: Option<u64>,
    },

    #[error("超过最大重试次数")]
    MaxRetriesExceeded,

    #[error("数据库错误: {0}")]
    DatabaseError(String),

    #[error("配置错误: {0}")]
    ConfigError(String),

    #[allow(dead_code)]
    #[error("格式转换错误: {0}")]
    TransformError(String),

    #[allow(dead_code)]
    #[error("无效的请求: {0}")]
    InvalidRequest(String),

    #[error("超时: {0}")]
    Timeout(String),

    /// 流式响应空闲超时
    #[allow(dead_code)]
    #[error("流式响应空闲超时: {0}秒无数据")]
    StreamIdleTimeout(u64),

    /// 认证错误
    #[error("认证失败: {0}")]
    AuthError(String),

    #[allow(dead_code)]
    #[error("内部错误: {0}")]
    Internal(String),
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        // AllProvidersCircuitOpen 也可能携带 Retry-After（来自路由器对冷却队列的汇总）
        if let ProxyError::AllProvidersCircuitOpen { retry_after_ms } = &self {
            let error_body = json!({
                "error": {
                    "message": self.to_string(),
                    "type": "proxy_error",
                }
            });
            let mut response = (StatusCode::SERVICE_UNAVAILABLE, Json(error_body)).into_response();
            if let Some(ms) = retry_after_ms {
                let secs = (*ms as f64 / 1000.0).ceil().max(1.0) as u64;
                if let Ok(val) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                    response
                        .headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, val);
                }
            }
            return response;
        }

        let (status, body, retry_after_ms) = match &self {
            ProxyError::UpstreamError {
                status: upstream_status,
                body: upstream_body,
                retry_after_ms,
            } => {
                let http_status =
                    StatusCode::from_u16(*upstream_status).unwrap_or(StatusCode::BAD_GATEWAY);

                // 尝试解析上游响应体为 JSON，如果失败则包装为字符串
                let error_body = if let Some(body_str) = upstream_body {
                    if let Ok(json_body) = serde_json::from_str::<serde_json::Value>(body_str) {
                        // 上游返回的是 JSON，直接透传
                        json_body
                    } else {
                        // 上游返回的不是 JSON，包装为错误消息
                        json!({
                            "error": {
                                "message": body_str,
                                "type": "upstream_error",
                            }
                        })
                    }
                } else {
                    json!({
                        "error": {
                            "message": format!("Upstream error (status {})", upstream_status),
                            "type": "upstream_error",
                        }
                    })
                };

                (http_status, error_body, *retry_after_ms)
            }
            _ => {
                let (http_status, message) = match &self {
                    ProxyError::AlreadyRunning => (StatusCode::CONFLICT, self.to_string()),
                    ProxyError::NotRunning => (StatusCode::SERVICE_UNAVAILABLE, self.to_string()),
                    ProxyError::BindFailed(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::StopTimeout => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::StopFailed(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::ForwardFailed(_) => (StatusCode::BAD_GATEWAY, self.to_string()),
                    ProxyError::NoAvailableProvider => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::AllProvidersCircuitOpen { .. } => unreachable!(),
                    ProxyError::NoProvidersConfigured => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::ProviderUnhealthy(_) => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::MaxRetriesExceeded => {
                        (StatusCode::SERVICE_UNAVAILABLE, self.to_string())
                    }
                    ProxyError::DatabaseError(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::ConfigError(_) => (StatusCode::BAD_REQUEST, self.to_string()),
                    ProxyError::TransformError(_) => {
                        (StatusCode::UNPROCESSABLE_ENTITY, self.to_string())
                    }
                    ProxyError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
                    ProxyError::Timeout(_) => (StatusCode::GATEWAY_TIMEOUT, self.to_string()),
                    ProxyError::StreamIdleTimeout(_) => {
                        (StatusCode::GATEWAY_TIMEOUT, self.to_string())
                    }
                    ProxyError::AuthError(_) => (StatusCode::UNAUTHORIZED, self.to_string()),
                    ProxyError::Internal(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
                    }
                    ProxyError::UpstreamError { .. } => unreachable!(),
                };

                let error_body = json!({
                    "error": {
                        "message": message,
                        "type": "proxy_error",
                    }
                });

                (http_status, error_body, None)
            }
        };

        let mut response = (status, Json(body)).into_response();
        // 透传 Retry-After 头：上游给了冷却建议就还给客户端
        if let Some(ms) = retry_after_ms {
            let secs = (ms as f64 / 1000.0).ceil() as u64;
            if let Ok(val) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                response
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, val);
            }
        }
        response
    }
}

/// 错误分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// 可重试错误（网络问题、5xx）
    Retryable, // 网络超时、5xx 错误
    /// 不可重试错误（4xx、认证失败）
    NonRetryable, // 认证失败、参数错误、4xx 错误
    #[allow(dead_code)]
    ClientAbort, // 客户端主动中断
}

/// 判断错误是否可重试
#[allow(dead_code)]
pub fn categorize_error(error: &reqwest::Error) -> ErrorCategory {
    if error.is_timeout() || error.is_connect() {
        return ErrorCategory::Retryable;
    }

    if let Some(status) = error.status() {
        if status.is_server_error() {
            ErrorCategory::Retryable
        } else if status.is_client_error() {
            ErrorCategory::NonRetryable
        } else {
            ErrorCategory::Retryable
        }
    } else {
        ErrorCategory::Retryable
    }
}
