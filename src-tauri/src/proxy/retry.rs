use crate::proxy::config::RetryConfig;
use axum::http::StatusCode;
use thiserror::Error;

/// 代理层统一错误
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("upstream {status}: {body}")]
    Upstream {
        status: u16,
        body: serde_json::Value,
    },
    #[error("circuit breaker open for provider '{0}'")]
    CircuitOpen(String),
    #[error("upstream network error: {0}")]
    Network(String),
    #[error("request timed out: {0}")]
    Timeout(String),
    #[error("no route for model '{0}'")]
    RouteNotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl ProxyError {
    /// 该错误是否值得重试（只对"重试可能成功"的失败重试）
    pub fn retriable(&self, cfg: &RetryConfig) -> bool {
        match self {
            ProxyError::Upstream { status, .. } => cfg.retry_statuses.contains(status),
            ProxyError::Network(_) | ProxyError::Timeout(_) => cfg.retry_on_network_error,
            _ => false,
        }
    }

    /// 映射为对客户端返回的 HTTP 状态
    pub fn http_status(&self) -> StatusCode {
        match self {
            ProxyError::Upstream { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            ProxyError::CircuitOpen(_) => StatusCode::SERVICE_UNAVAILABLE,
            ProxyError::Network(_) | ProxyError::Timeout(_) => StatusCode::BAD_GATEWAY,
            ProxyError::RouteNotFound(_) => StatusCode::NOT_FOUND,
            ProxyError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ProxyError::Unauthorized => StatusCode::UNAUTHORIZED,
            ProxyError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// OpenAI 兼容错误体
    pub fn openai_error(&self) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "message": self.to_string(),
                "type": match self {
                    ProxyError::CircuitOpen(_) => "circuit_open",
                    ProxyError::RouteNotFound(_) => "model_not_found",
                    ProxyError::Upstream { .. } => "upstream_error",
                    ProxyError::Unauthorized => "invalid_api_key",
                    _ => "proxy_error",
                },
                "code": null,
            }
        })
    }
}

/// 指数退避 + 抖动（毫秒）。attempt 从 1 开始。
pub fn backoff_delay(attempt: u32, cfg: &RetryConfig) -> std::time::Duration {
    let base = cfg.base_delay_ms.max(50);
    let exp = base.saturating_mul(1u64 << attempt.saturating_sub(1).min(6));
    let capped = exp.min(cfg.max_delay_ms.max(base));
    // 确定性抖动，避免惊群
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let jitter = (nanos % 120) as u64;
    std::time::Duration::from_millis(capped + jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_semantics() {
        let cfg = RetryConfig::default();
        assert!(ProxyError::Upstream {
            status: 429,
            body: serde_json::json!({})
        }
        .retriable(&cfg));
        assert!(ProxyError::Upstream {
            status: 503,
            body: serde_json::json!({})
        }
        .retriable(&cfg));
        assert!(!ProxyError::Upstream {
            status: 400,
            body: serde_json::json!({})
        }
        .retriable(&cfg));
        assert!(ProxyError::Network("boom".into()).retriable(&cfg));
        assert!(!ProxyError::RouteNotFound("x".into()).retriable(&cfg));
        assert!(!ProxyError::Unauthorized.retriable(&cfg));
    }

    #[test]
    fn backoff_grows() {
        let cfg = RetryConfig::default();
        let d1 = backoff_delay(1, &cfg).as_millis();
        let d2 = backoff_delay(2, &cfg).as_millis();
        let d3 = backoff_delay(3, &cfg).as_millis();
        assert!(d2 >= d1);
        assert!(d3 >= d2);
        assert!(d3 as u64 <= cfg.max_delay_ms + 120);
    }
}
