//! 请求/响应头透传规则。
//!
//! # 目的
//! 让上游看到的请求与"客户端直连"时尽可能一致。代理若从零重建请求，会留下
//! 一串一眼可见的异常特征：
//! - 没有 `User-Agent`（reqwest 未指定时根本不发该头）
//! - `accept: */*`（reqwest 的默认值，官方 SDK 发的是 `application/json`）
//! - 客户端的组织 / 项目 / beta 头（`openai-organization`、`anthropic-beta` 等）全部丢失
//! - 官方 SDK 的指纹头（`x-stainless-*`）丢失
//!
//! 因此这里采用"白名单透传 + 兜底默认值"：客户端带了就用客户端的，没带就补上
//! 与主流 SDK 一致的值；同时剔除 hop-by-hop 头与代理专属头
//! （`x-forwarded-for` / `via` 等），避免自报身份。

use crate::proxy::config::ProviderConfig;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use axum::response::Response;

/// 客户端未提供 UA 时的兜底值。
///
/// 取 OpenAI Python SDK 的默认 UA 形态：这是各大 OpenAI 兼容上游每天收到量最大的
/// 一类 UA，不会因为罕见而被额外标记。可在渠道配置中用 `user_agent` 覆盖。
pub const DEFAULT_USER_AGENT: &str = "OpenAI/Python 1.55.3";

/// 客户端未提供 `accept` 时的兜底值（JSON API 的常规取值）。
pub const DEFAULT_ACCEPT: &str = "application/json";

/// 允许透传给上游的请求头（精确匹配，小写）。
///
/// 刻意不包含 `origin`：浏览器直连时它指向客户端页面，而穿透后指向本地应用，
/// 反而会引入不一致；确有需要可在渠道的 `headers` 里单独指定。
const FORWARD_EXACT: &[&str] = &[
    "accept",
    "accept-encoding",
    "accept-language",
    "cache-control",
    "content-type",
    "user-agent",
    // OpenAI / Anthropic 官方 SDK 的业务头
    "openai-organization",
    "openai-project",
    "openai-beta",
    "anthropic-version",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    // 中转站常用的归属标识（OpenRouter 等据此统计来源应用）
    "http-referer",
    "referer",
    "x-title",
    // 链路追踪：透传后上游日志与客户端能对上，也更像直连
    "x-request-id",
    "x-client-request-id",
    "x-ms-client-request-id",
];

/// 允许透传的请求头前缀（官方 SDK 指纹头，逐个列举不现实）。
const FORWARD_PREFIX: &[&str] = &["x-stainless-", "x-openai-", "x-anthropic-"];

/// 前缀命中但仍不透传：重试计数由本代理内部维护，
/// 透传会把"代理自己重试了 3 次"暴露成"客户端重试了 3 次"。
const FORWARD_DENY: &[&str] = &["x-stainless-retry-count"];

/// 允许回传给客户端的响应头（精确匹配）。
const RESPONSE_EXACT: &[&str] = &[
    "content-type",
    "date",
    "server",
    "retry-after",
    "x-request-id",
    "request-id",
    "cf-ray",
];

/// 允许回传的响应头前缀：涵盖各家厂商的限流、组织、版本、请求追踪头。
const RESPONSE_PREFIX: &[&str] = &[
    "openai-",
    "x-openai-",
    "anthropic-",
    "x-anthropic-",
    "ratelimit-",
    "x-ratelimit-",
    "x-request-",
    "x-goog-",
    "x-amzn-",
    "x-ms-",
    "apim-",
];

/// 即使命中白名单也不回传的响应头。
///
/// - `content-encoding` / `content-length`：reqwest 已解压响应体，且非流式响应会被
///   重新序列化，保留原值会让客户端按 gzip 去解一段明文。
/// - `transfer-encoding` / `connection`：hop-by-hop，由本层连接自行决定。
/// - `access-control-*`：CORS 由本代理自己的 `CorsLayer` 统一应答，回传上游的会被覆盖或冲突。
const RESPONSE_DENY: &[&str] = &[
    "content-encoding",
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "strict-transport-security",
    "alt-svc",
    "access-control-allow-origin",
    "access-control-allow-credentials",
    "access-control-allow-headers",
    "access-control-allow-methods",
    "access-control-expose-headers",
    "access-control-max-age",
    "access-control-vary",
];

/// HTTP 客户端的默认头：所有出口请求的兜底指纹。
pub fn client_default_headers() -> HeaderMap {
    let mut m = HeaderMap::new();
    m.insert(header::USER_AGENT, header_value(DEFAULT_USER_AGENT));
    m.insert(header::ACCEPT, header_value(DEFAULT_ACCEPT));
    m
}

/// 构造发往上游的请求头。
///
/// 优先级：渠道自定义头 > 客户端透传头 > 兜底默认值。
/// 鉴权单独处理：渠道配了 `api_key` 就用它；没配则沿用客户端的 `Authorization`，
/// 保持"未配置密钥即纯转发"的直连语义。
pub fn build_upstream_headers(
    client: &HeaderMap,
    provider: &ProviderConfig,
) -> Vec<(HeaderName, HeaderValue)> {
    let mut out: Vec<(HeaderName, HeaderValue)> = Vec::new();

    for (name, value) in client.iter() {
        if is_forwardable_request_header(name.as_str()) {
            out.push((name.clone(), value.clone()));
        }
    }

    // 鉴权：只用渠道配置的密钥。
    // 客户端的 Authorization 是本地访问令牌，属于本代理的凭据而非上游凭据，
    // 既不能外泄（上游会看到一串不属于它的 key），也无需外泄——
    // 上游若要鉴权，在渠道里配 api_key 即可。
    if !provider.api_key.trim().is_empty() {
        if let Some(v) = bearer(&provider.api_key) {
            out.push((header::AUTHORIZATION, v));
        }
    }

    // 渠道自定义头（最高优先级，可覆盖上面任意一项）
    for (k, v) in &provider.headers {
        let Ok(name) = HeaderName::try_from(k.as_str()) else {
            continue;
        };
        let Some(value) = header_value_opt(v) else {
            continue;
        };
        out.push((name, value));
    }

    if !has(&out, header::USER_AGENT) {
        let ua = if provider.user_agent.trim().is_empty() {
            DEFAULT_USER_AGENT
        } else {
            provider.user_agent.trim()
        };
        out.push((header::USER_AGENT, header_value(ua)));
    }
    if !has(&out, header::ACCEPT) {
        out.push((header::ACCEPT, header_value(DEFAULT_ACCEPT)));
    }

    out
}

/// 把上游响应头合并进返回给客户端的响应（只合并不被拒绝的白名单项）。
pub fn apply_upstream_response_headers(res: &mut Response, upstream: &HeaderMap) {
    let dst = res.headers_mut();
    for (name, value) in upstream.iter() {
        if is_forwardable_response_header(name.as_str()) {
            dst.insert(name.clone(), value.clone());
        }
    }
}

fn is_forwardable_request_header(name: &str) -> bool {
    if FORWARD_DENY.contains(&name) {
        return false;
    }
    FORWARD_EXACT.contains(&name) || FORWARD_PREFIX.iter().any(|p| name.starts_with(p))
}

fn is_forwardable_response_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if RESPONSE_DENY.contains(&name.as_str()) {
        return false;
    }
    RESPONSE_EXACT.contains(&name.as_str())
        || RESPONSE_PREFIX.iter().any(|p| name.starts_with(p))
}

fn bearer(key: &str) -> Option<HeaderValue> {
    header_value_opt(&format!("Bearer {}", key.trim()))
}

fn header_value(s: &str) -> HeaderValue {
    // 兜底常量均为合法 ASCII，不可能失败
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn header_value_opt(s: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(s.trim()).ok()
}

fn has(headers: &[(HeaderName, HeaderValue)], name: HeaderName) -> bool {
    headers.iter().any(|(n, _)| *n == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::ProviderConfig;
    use axum::response::IntoResponse;

    fn provider() -> ProviderConfig {
        ProviderConfig {
            headers: std::collections::BTreeMap::new(),
            user_agent: String::new(),
            ..ProviderConfig::demo()
        }
    }

    #[test]
    fn forwards_client_identity_headers() {
        let mut client = HeaderMap::new();
        client.insert(header::USER_AGENT, "OpenAI/NodeJS/4.68.4".parse().unwrap());
        client.insert("openai-organization", "org-abc".parse().unwrap());
        client.insert("x-stainless-lang", "node".parse().unwrap());
        // 代理特征头必须被丢弃
        client.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        client.insert("via", "1.1 proxy".parse().unwrap());

        let out = build_upstream_headers(&client, &provider());
        let names: Vec<String> = out.iter().map(|(n, _)| n.to_string()).collect();
        assert!(names.contains(&"user-agent".to_string()));
        assert!(names.contains(&"openai-organization".to_string()));
        assert!(names.contains(&"x-stainless-lang".to_string()));
        assert!(!names.contains(&"x-forwarded-for".to_string()));
        assert!(!names.contains(&"via".to_string()));
    }

    #[test]
    fn falls_back_to_sdk_like_defaults() {
        let out = build_upstream_headers(&HeaderMap::new(), &provider());
        let get = |name: HeaderName| {
            out.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_str().unwrap().to_string())
        };
        assert!(get(header::USER_AGENT).unwrap().starts_with("OpenAI/"));
        assert_eq!(get(header::ACCEPT).unwrap(), DEFAULT_ACCEPT);
    }

    #[test]
    fn provider_key_overrides_client_auth_and_custom_ua() {
        let mut client = HeaderMap::new();
        client.insert(header::AUTHORIZATION, "Bearer client-key".parse().unwrap());
        let mut p = provider();
        p.api_key = "sk-upstream".into();
        p.user_agent = "my-agent/1.0".into();
        p.headers
            .insert("x-title".into(), "my-app".into());

        let out = build_upstream_headers(&client, &p);
        let get = |name: &str| {
            out.iter()
                .find(|(n, _)| n.as_str() == name)
                .map(|(_, v)| v.to_str().unwrap().to_string())
        };
        assert_eq!(get("authorization").unwrap(), "Bearer sk-upstream");
        assert_eq!(get("user-agent").unwrap(), "my-agent/1.0");
        assert_eq!(get("x-title").unwrap(), "my-app");
    }

    #[test]
    fn never_forwards_client_auth() {
        // 客户端的 Authorization 是本地访问令牌，绝不能外泄给上游
        let mut client = HeaderMap::new();
        client.insert(header::AUTHORIZATION, "Bearer local-token".parse().unwrap());
        let out = build_upstream_headers(&client, &provider());
        assert!(
            !out.iter().any(|(n, _)| *n == header::AUTHORIZATION),
            "渠道未配密钥时不应带 Authorization"
        );
    }

    #[test]
    fn drops_encoding_and_length_in_response() {
        let mut upstream = HeaderMap::new();
        upstream.insert("content-encoding", "gzip".parse().unwrap());
        upstream.insert("content-length", "123".parse().unwrap());
        upstream.insert("x-request-id", "req_1".parse().unwrap());
        upstream.insert("openai-organization", "org-1".parse().unwrap());
        upstream.insert("anthropic-ratelimit-requests-remaining", "9".parse().unwrap());

        let mut res = (axum::http::StatusCode::OK, "{}").into_response();
        apply_upstream_response_headers(&mut res, &upstream);
        let h = res.headers();
        assert!(h.get("content-encoding").is_none(), "已解压，不能回传编码");
        assert!(h.get("content-length").is_none(), "长度已变，不能回传");
        assert_eq!(h.get("x-request-id").unwrap(), "req_1");
        assert_eq!(h.get("openai-organization").unwrap(), "org-1");
        assert_eq!(
            h.get("anthropic-ratelimit-requests-remaining").unwrap(),
            "9"
        );
    }
}
