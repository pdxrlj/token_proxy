use axum::http::{HeaderMap, StatusCode};
use serde::Serialize;
use thiserror::Error;

// ─────────────────────────── 重试内部固定参数（不暴露配置） ───────────────────────────

/// 指数退避基数（毫秒）：delay(attempt) = min(BASE × 2^(attempt-1), MAX)
pub const RETRY_BACKOFF_BASE_MS: u64 = 500;
/// 退避上限（毫秒）：500ms → 1s → 2s → 4s → 8s 封顶
pub const RETRY_BACKOFF_MAX_MS: u64 = 8000;

/// 代理层统一错误
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("upstream {status}: {body}")]
    Upstream {
        status: u16,
        body: serde_json::Value,
        /// 上游响应头（用于把 `x-request-id`、`retry-after` 等回传给客户端）
        headers: HeaderMap,
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
    /// API Key 配额已用尽：`{0}` 已达 `{1}` 次请求上限
    #[error("api key quota exceeded: '{0}' reached limit of {1} requests")]
    QuotaExceeded(String, u64),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// 错误大类：用于前端展示时选择合适的标题与配色。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// 上游返回了非 2xx 响应
    Upstream,
    /// 熔断器打开，请求被直接拒绝
    CircuitOpen,
    /// 网络层错误（连接失败、DNS、重置等）
    Network,
    /// 超时（上游超时或请求总时限耗尽）
    Timeout,
    /// 找不到该模型的路由
    RouteNotFound,
    /// 请求本身不合法
    BadRequest,
    /// 本地鉴权失败
    Unauthorized,
    /// API Key 配额已用尽
    QuotaExceeded,
    /// 其它内部错误
    Internal,
}

/// 结构化错误详情：随日志推送给前端，用于在"请求详情"里展示可读的诊断信息。
///
/// 相比原先只有一个扁平字符串（如 `circuit breaker open for provider 'xxx'`），
/// 这里把错误拆解为"发生了什么 / 为什么 / 怎么办 / 发生在哪"四个部分，
/// 让用户无需查看服务端日志即可定位问题。
#[derive(Debug, Clone, Serialize)]
pub struct ErrorDetail {
    /// 错误大类（前端据此选择标题与配色）
    pub kind: ErrorKind,
    /// 简短摘要，一句话说明发生了什么
    pub summary: String,
    /// 上游/系统返回的原始错误详情（如有）
    pub detail: Option<String>,
    /// 面向用户的排查建议
    pub hint: Option<String>,
    /// 出错的 provider 名称
    pub provider: Option<String>,
    /// 出错的 provider id
    pub provider_id: Option<String>,
    /// 请求时使用的上游模型名
    pub upstream_model: Option<String>,
    /// 请求的端点路径
    pub endpoint: Option<String>,
    /// 上游返回的 HTTP 状态码（仅 `Upstream` 类型）
    pub upstream_status: Option<u16>,
    /// 已尝试的候选数 / 总候选数（failover 场景）
    pub candidates_tried: Option<usize>,
    pub total_candidates: Option<usize>,
    /// 重试次数
    pub retries: Option<u32>,
    /// 熔断相关：连续失败次数
    pub consecutive_failures: Option<u32>,
    /// 熔断相关：还需冷却的秒数（0 表示可立即重试）
    pub cooldown_remaining_secs: Option<u64>,
}

impl ProxyError {
    /// 上游 503：由熔断器接管（单次立即跳闸 + 换渠道），不走同渠道退避重试。
    ///
    /// 与 `retry_statuses` 无关——列表里有没有 503 都不影响该判定，
    /// 错误处理流程中 503 分支优先于任何重试判断。
    pub fn is_503(&self) -> bool {
        matches!(self, ProxyError::Upstream { status: 503, .. })
    }

    /// 该错误是否值得**同渠道退避重试**（只对"重试可能成功"的失败重试）。
    ///
    /// - 上游状态码：在 `retry_statuses` 列表中（503 在错误处理流程中被熔断分支
    ///   先行接管，无论列表是否包含 503 都不会走到同渠道重试）
    /// - 网络错误/超时：始终重试（内部固定语义，不可配置）
    pub fn retriable(&self, cfg: &crate::proxy::config::RetryConfig) -> bool {
        match self {
            ProxyError::Upstream { status, .. } => cfg.retry_statuses.contains(status),
            ProxyError::Network(_) | ProxyError::Timeout(_) => true,
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
            // 429 Too Many Requests 是配额耗尽的语义化状态码
            ProxyError::QuotaExceeded(_, _) => StatusCode::TOO_MANY_REQUESTS,
            ProxyError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 错误大类（供结构化错误详情使用）
    pub fn kind(&self) -> ErrorKind {
        match self {
            ProxyError::Upstream { .. } => ErrorKind::Upstream,
            ProxyError::CircuitOpen(_) => ErrorKind::CircuitOpen,
            ProxyError::Network(_) => ErrorKind::Network,
            ProxyError::Timeout(_) => ErrorKind::Timeout,
            ProxyError::RouteNotFound(_) => ErrorKind::RouteNotFound,
            ProxyError::BadRequest(_) => ErrorKind::BadRequest,
            ProxyError::Unauthorized => ErrorKind::Unauthorized,
            ProxyError::QuotaExceeded(_, _) => ErrorKind::QuotaExceeded,
            ProxyError::Io(_) => ErrorKind::Internal,
        }
    }

    /// 一句话摘要：说明"发生了什么"，不含原始堆栈式的冗长信息。
    pub fn summary(&self) -> String {
        match self {
            ProxyError::Upstream { status, .. } => format!("上游返回错误（HTTP {status}）"),
            ProxyError::CircuitOpen(p) => format!("渠道「{p}」已熔断，请求被直接拒绝"),
            ProxyError::Network(_) => "无法连接到上游服务".to_string(),
            ProxyError::Timeout(_) => "请求超时".to_string(),
            ProxyError::RouteNotFound(m) => format!("找不到模型「{m}」的可用路由"),
            ProxyError::BadRequest(m) => format!("请求参数有误：{m}"),
            ProxyError::Unauthorized => "本地访问令牌校验失败".to_string(),
            ProxyError::QuotaExceeded(name, limit) => {
                format!("用户「{name}」的请求次数已达上限（{limit} 次）")
            }
            ProxyError::Io(_) => "代理内部 IO 错误".to_string(),
        }
    }

    /// 面向用户的排查建议。
    pub fn hint(&self) -> Option<String> {
        Some(match self {
            ProxyError::Upstream { status, .. } => match *status {
                401 | 403 => "上游拒绝了鉴权：请检查该渠道的 API Key 是否正确或已过期。".into(),
                404 => "上游不存在该模型或端点：请检查路由配置的模型名与上游模型名。".into(),
                429 => "上游限流：可降低请求频率，或为该模型配置更多渠道分流。".into(),
                500..=599 => {
                    "上游服务异常：通常会自动重试并切换到其它渠道，若持续出现请检查上游状态。"
                        .into()
                }
                _ => "请求被上游拒绝：请检查请求参数是否符合该上游的要求。".into(),
            },
            ProxyError::CircuitOpen(_) => {
                "该渠道已被熔断隔离（默认 1 小时），期间新请求会自动切换到其它可用渠道；\
                 冷却结束后自动探活，连续成功后逐步恢复全部流量。\
                 若所有渠道均熔断，代理会按 Panic 模式限速放行试探请求，不会无限拒绝。\
                 可在「渠道」页手动重置熔断器。"
                    .into()
            }
            ProxyError::Network(_) => {
                "请检查网络连通性、代理设置，以及渠道 base_url 是否填写正确。".into()
            }
            ProxyError::Timeout(_) => "上游响应过慢或单个请求的总耗时超出上限。\
                 可尝试调大渠道超时时间，或改用响应更快的渠道。"
                .into(),
            ProxyError::RouteNotFound(_) => {
                "请到「路由」页为该模型添加路由，或确认渠道已启用且模型列表中包含该模型。".into()
            }
            ProxyError::BadRequest(_) => {
                "请检查请求体是否符合 OpenAI API 格式（model 字段必填）。".into()
            }
            ProxyError::Unauthorized => {
                "请在设置中填写与本地访问令牌一致的 Authorization: Bearer <token>。".into()
            }
            ProxyError::QuotaExceeded(_, _) => "该用户的请求次数已用尽。\
                 可到「用户管理」页调高上限，或重置该用户的已用次数。"
                .into(),
            ProxyError::Io(_) => "请查看应用日志获取详细信息。".into(),
        })
    }

    /// 上游/系统返回的原始错误详情（已尽量去除冗余前缀）。
    pub fn raw_detail(&self) -> Option<String> {
        match self {
            ProxyError::Upstream { body, .. } => Some(format_upstream_body(body)),
            ProxyError::CircuitOpen(p) => Some(format!("provider: {p}")),
            ProxyError::Network(e) | ProxyError::Timeout(e) => Some(e.clone()),
            ProxyError::RouteNotFound(m) => Some(format!("model: {m}")),
            ProxyError::BadRequest(m) => Some(m.clone()),
            ProxyError::Io(e) => Some(e.to_string()),
            ProxyError::Unauthorized => None,
            ProxyError::QuotaExceeded(name, limit) => {
                Some(format!("api_key: {name}, limit: {limit}"))
            }
        }
    }

    /// 上游响应头（仅 `Upstream` 类型有值；用于回传给客户端）
    pub fn upstream_headers(&self) -> Option<&HeaderMap> {
        match self {
            ProxyError::Upstream { headers, .. } => Some(headers),
            _ => None,
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
                    ProxyError::QuotaExceeded(_, _) => "rate_limit_exceeded",
                    _ => "proxy_error",
                },
                "code": null,
            }
        })
    }

    /// 构造结构化错误详情，`ctx` 提供请求上下文（provider / 模型 / 尝试次数等）。
    pub fn to_detail(&self, ctx: ErrorContext) -> ErrorDetail {
        ErrorDetail {
            kind: self.kind(),
            summary: self.summary(),
            detail: self.raw_detail(),
            hint: self.hint(),
            provider: ctx.provider,
            provider_id: ctx.provider_id,
            upstream_model: ctx.upstream_model,
            endpoint: ctx.endpoint,
            upstream_status: match self {
                ProxyError::Upstream { status, .. } => Some(*status),
                _ => None,
            },
            candidates_tried: ctx.candidates_tried,
            total_candidates: ctx.total_candidates,
            retries: ctx.retries,
            consecutive_failures: ctx.consecutive_failures,
            cooldown_remaining_secs: ctx.cooldown_remaining_secs,
        }
    }
}

/// 构造错误详情所需的请求上下文。
///
/// 由 `route_and_call` 在出错点收集，用于在详情面板中还原"当时到底怎么回事"。
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    pub provider: Option<String>,
    pub provider_id: Option<String>,
    pub upstream_model: Option<String>,
    pub endpoint: Option<String>,
    pub candidates_tried: Option<usize>,
    pub total_candidates: Option<usize>,
    pub retries: Option<u32>,
    pub consecutive_failures: Option<u32>,
    pub cooldown_remaining_secs: Option<u64>,
}

/// 把上游返回的错误体格式化为可读文本。
///
/// 各家厂商的错误体结构不一，这里按常见字段依次尝试提取，
/// 都取不到时退化为紧凑 JSON，避免把一大坨原始 JSON 直接糊到界面上。
fn format_upstream_body(body: &serde_json::Value) -> String {
    // OpenAI 风格：{ "error": { "message": ..., "type": ..., "code": ... } }
    if let Some(err) = body.get("error") {
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
            parts.push(m.to_string());
        }
        let mut meta: Vec<String> = Vec::new();
        if let Some(t) = err.get("type").and_then(|t| t.as_str()) {
            meta.push(format!("type={t}"));
        }
        if let Some(c) = err.get("code").and_then(|c| c.as_str()) {
            meta.push(format!("code={c}"));
        }
        if !meta.is_empty() {
            parts.push(format!("({})", meta.join(", ")));
        }
        if !parts.is_empty() {
            return parts.join(" ");
        }
        // error 是字符串的情况
        if let Some(s) = err.as_str() {
            return s.to_string();
        }
    }
    // 其它常见字段名
    for key in ["message", "msg", "error_msg", "detail"] {
        if let Some(s) = body.get(key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    // 兜底：紧凑 JSON（截断避免过长）
    let s = serde_json::to_string(body).unwrap_or_default();
    const MAX: usize = 500;
    if s.len() > MAX {
        format!("{}…（已截断）", &s[..MAX])
    } else {
        s
    }
}

/// 指数退避（毫秒）。attempt 从 1 开始。
///
/// `delay = min(RETRY_BACKOFF_BASE_MS × 2^(attempt-1), RETRY_BACKOFF_MAX_MS)`
/// → 500ms → 1s → 2s → 4s → 8s 封顶。
/// 基数与上限为内部固定参数，不对外暴露配置；重试本身低频，无抖动。
pub fn backoff_delay(attempt: u32) -> std::time::Duration {
    let exp = RETRY_BACKOFF_BASE_MS.saturating_mul(1u64 << attempt.saturating_sub(1).min(4));
    std::time::Duration::from_millis(exp.min(RETRY_BACKOFF_MAX_MS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::RetryConfig;

    #[test]
    fn retry_semantics() {
        let cfg = RetryConfig::default();
        assert!(ProxyError::Upstream {
            status: 429,
            body: serde_json::json!({}),
            headers: HeaderMap::new(),
        }
        .retriable(&cfg));
        // 503 不在默认列表中 → retriable() 为 false。
        // 语义澄清：503 由错误处理流程的熔断分支（is_503）先行接管，
        // 无论列表是否包含 503，都轮不到 retriable() 判定同渠道重试。
        assert!(!ProxyError::Upstream {
            status: 503,
            body: serde_json::json!({}),
            headers: HeaderMap::new(),
        }
        .retriable(&cfg));
        assert!(!ProxyError::Upstream {
            status: 400,
            body: serde_json::json!({}),
            headers: HeaderMap::new(),
        }
        .retriable(&cfg));
        assert!(ProxyError::Network("boom".into()).retriable(&cfg));
        assert!(!ProxyError::RouteNotFound("x".into()).retriable(&cfg));
        assert!(!ProxyError::Unauthorized.retriable(&cfg));
    }

    #[test]
    fn is_503_standalone_detection() {
        // 503 判定独立于 retry_statuses：只看状态码本身
        assert!(ProxyError::Upstream {
            status: 503,
            body: serde_json::json!({}),
            headers: HeaderMap::new(),
        }
        .is_503());
        assert!(!ProxyError::Upstream {
            status: 500,
            body: serde_json::json!({}),
            headers: HeaderMap::new(),
        }
        .is_503());
        assert!(!ProxyError::Network("conn refused".into()).is_503());
        assert!(!ProxyError::Timeout("slow".into()).is_503());
    }

    #[test]
    fn error_detail_carries_context() {
        // 熔断错误应带上渠道与冷却信息，而不是只有一句 "circuit breaker open ..."
        let e = ProxyError::CircuitOpen("日日新2".into());
        let d = e.to_detail(ErrorContext {
            provider: Some("日日新2".into()),
            provider_id: Some("sunrise2".into()),
            upstream_model: Some("glm-4-plus".into()),
            endpoint: Some("https://api.example.cn/v1/chat/completions".into()),
            candidates_tried: Some(2),
            total_candidates: Some(2),
            retries: Some(3),
            consecutive_failures: Some(5),
            cooldown_remaining_secs: Some(18),
        });
        assert_eq!(d.kind, ErrorKind::CircuitOpen);
        assert!(d.summary.contains("日日新2"), "summary={}", d.summary);
        assert!(d.hint.is_some(), "熔断错误应附带排查建议");
        assert_eq!(d.candidates_tried, Some(2));
        assert_eq!(d.consecutive_failures, Some(5));
        assert_eq!(d.cooldown_remaining_secs, Some(18));
        assert_eq!(d.upstream_status, None, "非上游错误不应有状态码");
    }

    #[test]
    fn error_detail_extracts_upstream_status_and_message() {
        // 上游错误应提取出状态码与可读的 message，而非整坨 JSON
        let body = serde_json::json!({
            "error": { "message": "insufficient quota", "type": "quota_exceeded", "code": "429" }
        });
        let e = ProxyError::Upstream {
            status: 429,
            body,
            headers: HeaderMap::new(),
        };
        let d = e.to_detail(ErrorContext::default());
        assert_eq!(d.kind, ErrorKind::Upstream);
        assert_eq!(d.upstream_status, Some(429));
        assert!(d.summary.contains("429"), "summary={}", d.summary);
        let detail = d.detail.unwrap_or_default();
        assert!(detail.contains("insufficient quota"), "detail={detail}");
        assert!(detail.contains("code=429"), "detail={detail}");
        assert!(d.hint.unwrap_or_default().contains("限流"));
    }

    #[test]
    fn format_upstream_body_handles_shapes() {
        // OpenAI 风格
        assert_eq!(
            format_upstream_body(&serde_json::json!({"error": {"message": "boom"}})),
            "boom"
        );
        // error 为字符串
        assert_eq!(
            format_upstream_body(&serde_json::json!({"error": "plain text"})),
            "plain text"
        );
        // 顶层 message
        assert_eq!(
            format_upstream_body(&serde_json::json!({"message": "nope"})),
            "nope"
        );
        // 无已知字段 → 退化成 JSON
        let raw = format_upstream_body(&serde_json::json!({"weird": 1}));
        assert!(raw.contains("weird"), "raw={raw}");
    }

    #[test]
    fn backoff_sequence_capped() {
        // 固定指数退避：500ms → 1s → 2s → 4s → 8s 封顶，无抖动
        assert_eq!(backoff_delay(1).as_millis(), 500);
        assert_eq!(backoff_delay(2).as_millis(), 1000);
        assert_eq!(backoff_delay(3).as_millis(), 2000);
        assert_eq!(backoff_delay(4).as_millis(), 4000);
        assert_eq!(backoff_delay(5).as_millis(), 8000);
        assert_eq!(backoff_delay(9).as_millis(), 8000, "8s 封顶");
    }
}
