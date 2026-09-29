use crate::proxy::circuit::CircuitBreaker;
use crate::proxy::config::{AppConfig, Candidate, ProviderConfig, RouteConfig};
use crate::proxy::db::{build_finetune_jsonl, spawn_insert, InsertRecord};
use crate::proxy::headers::apply_upstream_response_headers;
use crate::proxy::lb;
use crate::proxy::retry::{backoff_delay, ErrorContext, ProxyError};
use crate::proxy::state::{AppState, RequestLog};
use crate::proxy::upstream::{
    call_upstream, detect_cache_hit, extract_cached_tokens, forward_raw, forward_sse,
    UpstreamOutcome,
};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::warn;

/// 单个请求的"重试 × failover"总耗时上限（秒）。
///
/// 默认 `max_retries = 3` 时，最坏情况为 `候选数 × (3+1)` 次上游调用，
/// 叠加指数退避与上游超时，总耗时可达数分钟，远超客户端超时窗口——
/// 客户端早已断开，代理侧的重试全部白费，还会堆积占用连接与内存。
/// 设定全局上限后，超时即快速失败，把响应时间控制在可预期的范围内。
const TOTAL_REQUEST_TIMEOUT_SECS: u64 = 120;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        // 兼容客户端把 base 配成不含 /v1 的情况
        .route("/chat/completions", post(chat_completions))
        .route("/completions", post(chat_completions))
        .route("/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(chat_completions))
        .route("/v1/embeddings", post(chat_completions))
        // 客户端协议兼容层：Claude Code（Anthropic API）与 Codex（Responses API）。
        // 注意 `/v1/responses` 不能映射到 chat_completions——Responses 格式的请求体
        // 发给上游 /chat/completions 必然报错，必须走协议翻译。
        .route("/v1/messages", post(crate::proxy::compat::anthropic::messages))
        .route("/messages", post(crate::proxy::compat::anthropic::messages))
        .route(
            "/v1/messages/count_tokens",
            post(crate::proxy::compat::anthropic::count_tokens),
        )
        .route(
            "/v1/responses",
            post(crate::proxy::compat::responses_api::responses),
        )
        .route(
            "/responses",
            post(crate::proxy::compat::responses_api::responses),
        )
        .route("/v1/{*path}", post(fallback_proxy))
        .with_state(state)
        // 允许任意来源调用（本地代理场景）
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
}

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = state.config.read();
    Json(serde_json::json!({
        "status": "ok",
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "port": cfg.server.port,
        "providers": cfg.providers.iter().filter(|p| p.enabled).count(),
    }))
}

async fn list_models(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let cfg = state.config.read();
    let mut seen = std::collections::HashSet::new();
    let mut data = Vec::new();
    for p in &cfg.providers {
        if !p.enabled {
            continue;
        }
        for m in &p.models {
            if seen.insert(m.clone()) {
                data.push(serde_json::json!({
                    "id": m,
                    "object": "model",
                    "created": 0,
                    "owned_by": p.id,
                }));
            }
        }
    }
    Json(serde_json::json!({ "object": "list", "data": data }))
}

/// 从请求头中提取客户端令牌。
///
/// 优先 `Authorization: Bearer <token>`；缺失时回落 `x-api-key`
/// （Claude Code 以 ANTHROPIC_API_KEY 方式接入时用该头携带令牌）。
pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let from_auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    from_auth.or_else(|| {
        headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|t| !t.is_empty())
    })
}

/// 本地鉴权 + 用户配额校验。
///
/// # 规则
///
/// **严格模式**（`server.require_api_key = true`）：只接受已配置且启用用户的 API Key，
/// 未携带或未登记的 Key 一律 401。要让配额真正生效必须开启此模式。
///
/// **宽松模式**（默认）：
/// 1. 携带了 Key 且命中用户表 → 按 `max_requests` 校验并原子消耗配额，
///    超出上限返回 429；无上限则直接放行。
/// 2. 未命中用户表：
///    - 设置了「本地访问令牌」→ Key 必须与之匹配，否则 401
///    - 未设置 → 当作匿名请求放行
///
///    之所以放行未登记的 Key，是因为多数 OpenAI SDK 连本地代理时也会带上自己的
///    key（如 `sk-...`），若一律拒绝会导致这些客户端全部不可用。
///    代价是配额可被"改用随机 Key"绕过，故需要强制时请开启严格模式。
/// 3. 未携带 Key：同 2 —— 未设置本地令牌时放行且不计配额（即"不携带 Key 不限次数"）。
pub(crate) fn check_auth(state: &AppState, cfg: &AppConfig, headers: &HeaderMap) -> Result<(), ProxyError> {
    let local_key = cfg.server.local_api_key.trim();
    let strict = cfg.server.require_api_key;

    match bearer_token(headers) {
        None => {
            if strict {
                return Err(ProxyError::Unauthorized);
            }
            if local_key.is_empty() {
                Ok(())
            } else {
                Err(ProxyError::Unauthorized)
            }
        }
        Some(token) => {
            // 命中用户表 → 走配额
            if let Some(u) = cfg.users.iter().find(|u| u.enabled && u.key == token) {
                if state.consume_user_quota(&u.id, u.max_requests) {
                    return Ok(());
                }
                let limit = u.max_requests.unwrap_or(0);
                let name = if u.name.trim().is_empty() {
                    u.id.clone()
                } else {
                    u.name.clone()
                };
                return Err(ProxyError::QuotaExceeded(name, limit));
            }

            // 未命中
            if strict {
                return Err(ProxyError::Unauthorized);
            }
            if local_key.is_empty() || token == local_key {
                Ok(())
            } else {
                Err(ProxyError::Unauthorized)
            }
        }
    }
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let mut log = RequestLog::new();
    log.method = "POST".into();
    log.path = "/v1/chat/completions".into();
    log.model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    log.stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    log.tokens_in = estimate_input_tokens(&body);

    // 本地鉴权
    {
        let cfg = state.config.read();
        if let Err(e) = check_auth(&state, &cfg, &headers) {
            return (e.http_status(), Json(e.openai_error())).into_response();
        }
    }

    // 请求开始即推送"进行中"日志（status=0 表示进行中）
    state.metrics.record(log.clone());
    state.emit_log(&log);

    let result = route_and_call(&state, &headers, "/chat/completions", &body, &mut log).await;

    // route_and_call 之外的错误（如 model 缺失）在此补挂结构化详情
    if let Err(e) = &result {
        if log.error_detail.is_none() {
            attach_error(
                &mut log,
                e,
                ErrorContext {
                    endpoint: Some("/chat/completions".to_string()),
                    ..Default::default()
                },
            );
        }
    }

    match result {
        Ok(UpstreamOutcome::Json(out)) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            let resp_bytes = serde_json::to_vec(&out.value)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            log.response_bytes = Some(resp_bytes);
            // 统计请求体大小到 bytes_in（之前始终为 0）
            let req_bytes = serde_json::to_vec(&body)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            state.metrics.add_bytes(req_bytes, resp_bytes);
            let usage = out.value.get("usage");
            // 用上游返回的真实 prompt_tokens 覆盖估算值
            if let Some(pt) = usage
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|t| t.as_u64())
            {
                log.tokens_in = Some(pt);
            }
            log.tokens_out = usage
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|t| t.as_u64());
            // 缓存命中检测：基于 usage 中的 cached_tokens 等字段
            let cache_hit = usage.map(|u| detect_cache_hit(u)).unwrap_or(false);
            log.cache_hit = cache_hit;
            let cached_tokens = usage.map(|u| extract_cached_tokens(u)).filter(|v| *v > 0);
            log.cached_tokens = cached_tokens;
            if cache_hit {
                state.metrics.inc_cache_hit();
            }
            state
                .metrics
                .add_tokens(log.tokens_in.unwrap_or(0), log.tokens_out.unwrap_or(0));
            save_to_db(&state, &log, &body, &out.value, false);
            finalize_log(&state, log);
            // 回传上游响应头（x-request-id / 限流余量等），让客户端看到的世界与直连一致
            let mut res = (StatusCode::OK, Json(out.value)).into_response();
            apply_upstream_response_headers(&mut res, &out.headers);
            res
        }
        // 裸 Stream 不应出现在业务层（route_and_call 总会包装成 StreamWithGuard）
        Ok(UpstreamOutcome::Stream(resp)) => {
            let upstream_headers = resp.headers().clone();
            sse_response(
                forward_sse(
                    state.clone(),
                    log.provider.clone().unwrap_or_default(),
                    log.model.clone(),
                    resp,
                    log.id.clone(),
                    body.clone(),
                    log.ts,
                    log.model.clone(),
                    log.upstream_model.clone(),
                    log.provider.clone(),
                    log.tokens_in,
                    None,
                    None,
                ),
                &upstream_headers,
            )
        }
        Ok(UpstreamOutcome::StreamWithGuard { resp, guard }) => {
            // 流式响应：头部返回即视为成功，流内错误走 error 事件
            let provider_name = log.provider.clone().unwrap_or_default();
            let model = log.model.clone();
            let log_id = log.id.clone();
            state.metrics.inc_total();
            state.metrics.inc_success();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            // 流式存库：把请求体 clone 传入 forward_sse，流结束后异步写入
            let req_body = body.clone();
            let log_ts = log.ts;
            let log_model = log.model.clone();
            let log_upstream_model = log.upstream_model.clone();
            let log_provider = log.provider.clone();
            let log_tokens_in = log.tokens_in;
            // 断流记熔断失败用的 route_key：从在途 guard 透传（与 route_and_call 一致）
            let inflight_key = guard.key().to_string();
            finalize_log(&state, log);
            let upstream_headers = resp.headers().clone();
            sse_response(
                forward_sse(
                    state.clone(),
                    provider_name,
                    model,
                    resp,
                    log_id,
                    req_body,
                    log_ts,
                    log_model,
                    log_upstream_model,
                    log_provider,
                    log_tokens_in,
                    Some(inflight_key),
                    Some(guard),
                ),
                &upstream_headers,
            )
        }
        Err(e) => {
            state.metrics.inc_total();
            state.metrics.inc_failed();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = e.http_status().as_u16();
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            log.error = Some(e.to_string());
            finalize_log(&state, log);
            let mut res = (e.http_status(), Json(e.openai_error())).into_response();
            // 上游错误也回传其响应头：429 的 retry-after、限流余量、x-request-id 等
            if let Some(h) = e.upstream_headers() {
                apply_upstream_response_headers(&mut res, h);
            }
            res
        }
    }
}

/// 兜底：其它 /v1/* 端点透传到上游（保持 OpenAI 格式）
async fn fallback_proxy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let mut log = RequestLog::new();
    log.method = "POST".into();
    log.path = format!("/v1/{path}");
    log.model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    log.stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    // 本地鉴权
    {
        let cfg = state.config.read();
        if let Err(e) = check_auth(&state, &cfg, &headers) {
            return (e.http_status(), Json(e.openai_error())).into_response();
        }
    }

    let started = Instant::now();
    // 请求开始即推送"进行中"日志（status=0 表示进行中）
    state.metrics.record(log.clone());
    state.emit_log(&log);

    let outcome = route_and_call(&state, &headers, &path, &body, &mut log).await;

    // route_and_call 之外的错误（如 model 缺失）在此补挂结构化详情
    if let Err(e) = &outcome {
        if log.error_detail.is_none() {
            attach_error(
                &mut log,
                e,
                ErrorContext {
                    endpoint: Some(format!("/v1/{path}")),
                    ..Default::default()
                },
            );
        }
    }

    match outcome {
        Ok(UpstreamOutcome::Json(out)) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            let resp_bytes = serde_json::to_vec(&out.value)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            log.response_bytes = Some(resp_bytes);
            // 统计请求体大小到 bytes_in（之前始终为 0）
            let req_bytes = serde_json::to_vec(&body)
                .map(|b| b.len() as u64)
                .unwrap_or(0);
            state.metrics.add_bytes(req_bytes, resp_bytes);
            // 缓存命中检测
            let usage = out.value.get("usage");
            // 用上游返回的真实 prompt_tokens 覆盖估算值
            if let Some(pt) = usage
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|t| t.as_u64())
            {
                log.tokens_in = Some(pt);
            }
            log.tokens_out = usage
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|t| t.as_u64());
            let cache_hit = usage.map(|u| detect_cache_hit(u)).unwrap_or(false);
            log.cache_hit = cache_hit;
            let cached_tokens = usage
                .map(|u| extract_cached_tokens(u))
                .filter(|val| *val > 0);
            log.cached_tokens = cached_tokens;
            if cache_hit {
                state.metrics.inc_cache_hit();
            }
            state
                .metrics
                .add_tokens(log.tokens_in.unwrap_or(0), log.tokens_out.unwrap_or(0));
            save_to_db(&state, &log, &body, &out.value, false);
            finalize_log(&state, log);
            // 回传上游响应头（x-request-id / 限流余量等）
            let mut res = (StatusCode::OK, Json(out.value)).into_response();
            apply_upstream_response_headers(&mut res, &out.headers);
            res
        }
        // 裸 Stream 不应出现在业务层（route_and_call 总会包装成 StreamWithGuard）
        Ok(UpstreamOutcome::Stream(resp)) => forward_sse(
            state.clone(),
            log.provider.clone().unwrap_or_default(),
            log.model.clone(),
            resp,
            log.id.clone(),
            body.clone(),
            log.ts,
            log.model.clone(),
            log.upstream_model.clone(),
            log.provider.clone(),
            log.tokens_in,
            None,
            None,
        )
        .into_response(),
        Ok(UpstreamOutcome::StreamWithGuard { resp, guard }) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            let provider_name = log.provider.clone().unwrap_or_default();
            let model = log.model.clone();
            let log_id = log.id.clone();
            let log_ts = log.ts;
            let log_model = log.model.clone();
            let log_upstream_model = log.upstream_model.clone();
            let log_provider = log.provider.clone();
            let log_tokens_in = log.tokens_in;
            // 断流记熔断失败用的 route_key：从在途 guard 透传（与 route_and_call 一致）
            let inflight_key = guard.key().to_string();
            let req_body = body.clone();
            finalize_log(&state, log);
            let upstream_headers = resp.headers().clone();
            let ct = upstream_headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/event-stream")
                .to_string();
            // SSE 流式：走 forward_sse 以统计字节数和 usage
            if ct.contains("text/event-stream") {
                return sse_response(
                    forward_sse(
                        state.clone(),
                        provider_name,
                        model,
                        resp,
                        log_id,
                        req_body,
                        log_ts,
                        log_model,
                        log_upstream_model,
                        log_provider,
                        log_tokens_in,
                        Some(inflight_key),
                        Some(guard),
                    ),
                    &upstream_headers,
                );
            }
            // 非 SSE 流：响应体由 axum 直接消费，此处立即归还在途计数
            drop(guard);
            let mut res = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, ct)],
                forward_raw(resp),
            )
                .into_response();
            apply_upstream_response_headers(&mut res, &upstream_headers);
            res
        }
        Err(e) => {
            state.metrics.inc_total();
            state.metrics.inc_failed();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = e.http_status().as_u16();
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            log.error = Some(e.to_string());
            finalize_log(&state, log);
            let mut res = (e.http_status(), Json(e.openai_error())).into_response();
            // 上游错误也回传其响应头：429 的 retry-after、限流余量、x-request-id 等
            if let Some(h) = e.upstream_headers() {
                apply_upstream_response_headers(&mut res, h);
            }
            res
        }
    }
}

/// 把 SSE 流包装为响应，并回传上游响应头。
///
/// `content-type` 固定为 `text/event-stream`：SSE 帧由本层重新生成，
/// 上游声明的值透传过来可能与实际的帧格式不符（如 gzip 之类已被解压）。
pub(crate) fn sse_response<S, E>(sse: axum::response::sse::Sse<S>, upstream: &HeaderMap) -> Response
where
    S: futures_util::Stream<Item = Result<axum::response::sse::Event, E>> + Send + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + Send + 'static,
{
    let mut res = sse.into_response();
    apply_upstream_response_headers(&mut res, upstream);
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    res
}

/// 路由上下文：从配置中解析出的负载均衡、重试、熔断配置和候选列表。
struct RoutingContext {
    lb_cfg: crate::proxy::config::LoadBalanceConfig,
    retry_cfg: crate::proxy::config::RetryConfig,
    breaker_cfg: crate::proxy::config::CircuitBreakerConfig,
    /// 候选列表（已 clone 出 read lock）：(provider, route)
    candidates: Vec<(ProviderConfig, Option<RouteConfig>)>,
    /// 负载均衡选中的初始候选索引
    selected_idx: usize,
    /// 会话标识（粘性路由用；未启用或提取失败时为 None）
    session_key: Option<String>,
}

/// 解析路由上下文：候选路由 + 负载均衡选择 + 重试/熔断配置。
///
/// 在 read lock 内完成候选解析和 LB 选择，然后 clone 出锁外。
fn resolve_routing_context(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    body: &serde_json::Value,
    model: &str,
) -> Result<RoutingContext, ProxyError> {
    let cfg = state.config.read();
    let candidates = cfg.resolve_candidates(model);
    if candidates.is_empty() {
        // 区分两种原因，让用户知道该去哪里修：
        // - 完全没配这个模型 → 去路由页添加
        // - 配了但渠道被禁用 → 去渠道页启用
        let known_model = cfg.routes.iter().any(|r| r.model == model)
            || cfg
                .providers
                .iter()
                .any(|p| p.models.iter().any(|m| m == model));
        let e = if known_model {
            ProxyError::RouteNotFound(format!("{model}（已配置，但对应渠道未启用或路由被禁用）"))
        } else {
            ProxyError::RouteNotFound(model.to_string())
        };
        return Err(e);
    }

    let lb_cfg = cfg.load_balance.clone();
    let breaker_cfg = cfg.retry.circuit_breaker.clone();

    // 提取会话 Key（用于粘性路由）
    let session_key = if lb_cfg.sticky.enabled {
        lb::extract_session_key(headers, body, &lb_cfg.sticky.session_key_source)
    } else {
        None
    };

    // eligible 子集：过滤掉熔断中的候选（保留 CLOSED / HALF_OPEN / OPEN-已到期）。
    // 熔断未启用时不过滤。全部候选熔断时子集为空——从首个候选起轮转，
    // 在 route_and_call 的 Panic 分支统一处理（最早熔断者限速放行）。
    let eligible: Vec<usize> = if breaker_cfg.enabled {
        candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let key = lb::route_key_for_candidate(model, c);
                state
                    .breakers
                    .get(&key)
                    .map(|b| b.is_eligible(&breaker_cfg))
                    .unwrap_or(true)
            })
            .map(|(i, _)| i)
            .collect()
    } else {
        (0..candidates.len()).collect()
    };

    // 负载均衡选择（在 read lock 内完成，因为 select 需要引用候选）；
    // 选择范围限定在 eligible 子集上，选中者保证未被熔断拦截
    let selected_idx = if lb_cfg.enabled && candidates.len() > 1 && !eligible.is_empty() {
        lb::select(
            state,
            &candidates,
            model,
            session_key.as_deref(),
            &lb_cfg,
            Some(&eligible),
        )
        .unwrap_or(eligible[0])
    } else {
        0
    };

    // Clone 候选数据出 read lock
    let candidates: Vec<(ProviderConfig, Option<RouteConfig>)> = candidates
        .into_iter()
        .map(|c| (c.provider.clone(), c.route.cloned()))
        .collect();

    Ok(RoutingContext {
        lb_cfg,
        retry_cfg: cfg.retry.clone(),
        breaker_cfg: cfg.retry.circuit_breaker.clone(),
        candidates,
        selected_idx,
        session_key,
    })
}

/// 路由 + 负载均衡 + 熔断 + 重试 + failover 循环。
///
/// # 流程
/// 1. 解析路由上下文（候选 + LB 选择）
/// 2. 对选中候选执行请求：熔断检查 → 在途计数 → 调用上游
/// 3. 成功则记录延迟并返回；失败则按重试策略退避重试
/// 4. 重试耗尽或熔断打开时，若还有候选 → failover 到下一候选
///    （无论负载均衡开关是否启用，故障切换始终生效）
///
/// # 粘性保证
/// 重试始终在同一候选上进行（不切换上游），避免上游缓存失效；
/// 仅在熔断打开或重试耗尽时才 failover，且 failover 成功后
/// 会把粘性记录更新到实际成功的候选，后续同会话请求直达健康上游。
pub(crate) async fn route_and_call(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    path: &str,
    body: &serde_json::Value,
    log: &mut RequestLog,
) -> Result<UpstreamOutcome, ProxyError> {
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ProxyError::BadRequest("model is required".into()))?;

    // ── 1. 解析路由上下文 ──
    let ctx = resolve_routing_context(state, headers, body, model)?;
    let RoutingContext {
        lb_cfg,
        retry_cfg,
        breaker_cfg,
        candidates,
        selected_idx,
        session_key,
    } = ctx;

    let total_candidates = candidates.len();
    let mut current_idx = selected_idx;
    let mut attempt: u32 = 0;
    let mut candidates_tried: usize = 1;
    // 全局 deadline：限制"重试 × failover"的总耗时。
    // 默认 max_retries=3 时，最坏情况每候选 4 次尝试 × N 个候选，
    // 叠加指数退避可达数分钟，远超客户端超时，全部工作都会白费。
    let deadline = Instant::now() + Duration::from_secs(TOTAL_REQUEST_TIMEOUT_SECS);
    // Panic 放行标记：本轮请求是"全部候选熔断"后的限速试探，
    // 跳过熔断检查直接调用（结果仍上报熔断器驱动状态转换）
    let mut panic_bypass = false;

    // 收集"最后一次失败"的上下文，用于出错时生成结构化错误详情。
    // 每次进入候选时更新，因此最终保留的是失败发生的那个候选的信息。
    let mut last_ctx: ErrorContext;

    // 用闭包统一构造错误：附带上下文后返回，避免每个 return 点重复拼装。
    let fail = |e: ProxyError,
                ctx: &ErrorContext,
                log: &mut RequestLog|
     -> Result<UpstreamOutcome, ProxyError> {
        attach_error(log, &e, ctx.clone());
        Err(e)
    };

    loop {
        let (provider, route) = &candidates[current_idx];
        let body = apply_overrides(body, route.as_ref());

        log.provider = Some(provider.name.clone());
        log.provider_id = Some(provider.id.clone());

        // 记录实际转发给上游的模型名（经 upstream_model 改写后）
        if let Some(m) = body.get("model").and_then(|m| m.as_str()) {
            if m != model {
                log.upstream_model = Some(m.to_string());
            }
        }

        let upstream_model_str = route
            .as_ref()
            .map(|r| r.upstream_model.as_str())
            .unwrap_or("");
        let inflight_key = lb::route_key(model, &provider.id, upstream_model_str);

        // 熔断器按 route 维度隔离（model|provider|upstream_model）：
        // 若按 provider 共享，某个 model 的故障会连带熔断该 provider 上的
        // 所有其它健康 model，故障域被不必要地放大。
        let breaker = state
            .breakers
            .entry(inflight_key.clone())
            .or_insert_with(|| Arc::new(CircuitBreaker::new()))
            .clone();

        // 更新当前候选的错误上下文（熔断状态需在此处采样）
        let breaker_status = breaker.status();
        last_ctx = ErrorContext {
            provider: Some(provider.name.clone()),
            provider_id: Some(provider.id.clone()),
            upstream_model: if upstream_model_str.is_empty() {
                None
            } else {
                Some(upstream_model_str.to_string())
            },
            endpoint: Some(provider.endpoint(path)),
            candidates_tried: Some(candidates_tried),
            total_candidates: Some(total_candidates),
            retries: Some(attempt),
            consecutive_failures: Some(breaker_status.consecutive_failures),
            cooldown_remaining_secs: cooldown_remaining_secs(&breaker, &breaker_cfg),
        };

        // ── 2. 熔断检查：打开时 failover 到下一候选 ──
        // panic_bypass：本轮是 Panic 试探（已抢占窗口名额），跳过检查直接放行
        if breaker_cfg.enabled && !panic_bypass && breaker.before_request(&breaker_cfg).is_err() {
            if candidates_tried < total_candidates {
                warn!(
                    provider = %provider.id,
                    model = %model,
                    "circuit open, failing over to next candidate"
                );
                current_idx = (current_idx + 1) % total_candidates;
                candidates_tried += 1;
                attempt = 0;
                continue;
            }
            // 全部候选均已熔断 → Panic 模式：让"最早熔断"的渠道按窗口限速
            // 放行真实请求作为试探。既避免单次偶发 503 把唯一渠道硬拒整个
            // 熔断时长，试探频率又受窗口约束，不会打死尚未恢复的上游。
            match claim_panic_probe(
                state,
                model,
                &candidates,
                breaker_cfg.panic_probe_window_secs,
            ) {
                Some(idx) => {
                    warn!(
                        provider = %candidates[idx].0.id,
                        model = %model,
                        window_secs = breaker_cfg.panic_probe_window_secs,
                        "all candidates circuit-open, panic probe admitted"
                    );
                    current_idx = idx;
                    panic_bypass = true;
                    continue;
                }
                None => {
                    return fail(
                        ProxyError::CircuitOpen(provider.name.clone()),
                        &last_ctx,
                        log,
                    );
                }
            }
        }
        // Panic 放行仅对单次调用生效；后续重试 / 再进入仍要走熔断检查
        panic_bypass = false;

        // ── 3. 在途计数 + 调用上游 ──
        // guard 在构造时 inc_in_flight，析构时自动归还，
        // 保证任何提前返回路径（含 `?` 与 failover `continue`）都不会漏还。
        let inflight_guard = lb::InFlightGuard::new(state.clone(), &inflight_key);
        let call_started = Instant::now();

        // 透传客户端请求头：让上游看到的请求与客户端直连时一致
        match call_upstream(state, provider, path, body.clone(), headers).await {
            Ok(outcome) => {
                // 成功：记录延迟样本，返回结果
                breaker.on_success(&breaker_cfg);
                let latency_ms = call_started.elapsed().as_millis() as u64;
                lb::record_latency(state, &inflight_key, latency_ms);

                if matches!(outcome, UpstreamOutcome::Stream(_)) {
                    // 流式：计数必须持有到 SSE 流真正结束，不能在这里归还。
                    // 把 guard 的所有权转移给调用方，由其在流结束时归还，
                    // 否则 LeastConnections 策略下所有候选恒为 0 而失效。
                    return Ok(UpstreamOutcome::StreamWithGuard {
                        resp: match outcome {
                            UpstreamOutcome::Stream(r) => r,
                            _ => unreachable!(),
                        },
                        guard: inflight_guard,
                    });
                }
                // 非流式：响应已完整接收，立即归还计数
                inflight_guard.release();
                // failover 成功后把粘性更新到实际成功的候选，
                // 避免同会话下一个请求又撞回故障上游
                if current_idx != selected_idx && lb_cfg.sticky.enabled {
                    if let Some(key) = session_key.as_deref() {
                        let cand = Candidate {
                            provider,
                            route: route.as_ref(),
                            index: current_idx,
                        };
                        lb::record_sticky(state, key, current_idx, &cand, model);
                    }
                }
                log.retries = attempt;
                return Ok(outcome);
            }
            Err(e) if e.is_503() => {
                // ── 503 专用分支（优先于一切重试判定，与 retry_statuses 无关）──
                // 上游明确表示"服务不可用"：立即跳闸熔断 + 跳过同渠道退避重试，
                // 直接 failover 到下一候选。重试一个正在 503 的上游大概率
                // 只是白等退避时间，还会放大其压力。
                if breaker_cfg.enabled {
                    breaker.trip_now();
                }
                drop(inflight_guard);
                if candidates_tried < total_candidates && Instant::now() < deadline {
                    warn!(
                        provider = %provider.id,
                        model = %model,
                        "upstream 503, circuit tripped immediately, failing over"
                    );
                    current_idx = (current_idx + 1) % total_candidates;
                    candidates_tried += 1;
                    attempt = 0;
                    continue;
                }
                let mut ctx = last_ctx.clone();
                ctx.candidates_tried = Some(candidates_tried);
                ctx.retries = Some(attempt);
                return fail(e, &ctx, log);
            }
            Err(e) if e.retriable(&retry_cfg) && attempt < retry_cfg.max_retries => {
                // 可重试错误：退避后重试（同一候选，保持上下文）
                if breaker_cfg.enabled {
                    breaker.on_failure(&breaker_cfg);
                }
                drop(inflight_guard);

                // 全局 deadline 检查：退避后可能已超时，此时再重试毫无意义
                let delay = backoff_delay(attempt + 1);
                if Instant::now() + delay > deadline {
                    warn!(
                        provider = %provider.id,
                        model = %model,
                        error = %e,
                        "request deadline reached, aborting retries"
                    );
                    // 用 Timeout 包装，让详情里能体现"总耗时超限"这一真实原因
                    let wrapped = ProxyError::Timeout(format!(
                        "单个请求总耗时超过 {}s 上限（已重试 {} 次，最近一次错误：{}）",
                        TOTAL_REQUEST_TIMEOUT_SECS, attempt, e
                    ));
                    return fail(wrapped, &last_ctx, log);
                }

                attempt += 1;
                state.metrics.inc_retries();
                log.retries = attempt;
                // 推送重试更新（前端看到 retries 变化）
                state.metrics.record(log.clone());
                state.emit_log(log);
                warn!(
                    provider = %provider.id,
                    model = %model,
                    attempt,
                    error = %e,
                    "retrying upstream call"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                // 不可重试或重试耗尽：failover 到下一候选
                if breaker_cfg.enabled {
                    breaker.on_failure(&breaker_cfg);
                }
                drop(inflight_guard);
                if candidates_tried < total_candidates && Instant::now() < deadline {
                    warn!(
                        provider = %provider.id,
                        model = %model,
                        error = %e,
                        "exhausted retries, failing over to next candidate"
                    );
                    current_idx = (current_idx + 1) % total_candidates;
                    candidates_tried += 1;
                    attempt = 0;
                    continue;
                }
                if candidates_tried >= total_candidates {
                    // 所有候选都试过了：把"试过几个候选"补进上下文再返回
                    let mut ctx = last_ctx.clone();
                    ctx.candidates_tried = Some(candidates_tried);
                    ctx.retries = Some(attempt);
                    return fail(e, &ctx, log);
                }
                // deadline 耗尽：不再 failover，直接返回最后的错误
                warn!(
                    provider = %provider.id,
                    model = %model,
                    error = %e,
                    "request deadline reached, aborting failover"
                );
                return fail(
                    ProxyError::Timeout(format!(
                        "单个请求总耗时超过 {TOTAL_REQUEST_TIMEOUT_SECS}s 上限（已尝试 {candidates_tried}/{total_candidates} 个渠道）"
                    )),
                    &last_ctx,
                    log,
                );
            }
        }
    }
}

/// 为请求日志附加结构化错误详情。
///
/// 所有失败出口统一走这里，保证 `error`（简短文本）与 `error_detail`（结构化）
/// 成对写入，前端详情弹窗才能展示完整的诊断信息。
pub(crate) fn attach_error(log: &mut RequestLog, e: &ProxyError, ctx: ErrorContext) {
    log.error = Some(e.to_string());
    log.error_detail = Some(e.to_detail(ctx));
}

/// 采样熔断冷却剩余秒数，用于错误详情中告知用户"还需多久可恢复"。
fn cooldown_remaining_secs(
    breaker: &std::sync::Arc<CircuitBreaker>,
    cfg: &crate::proxy::config::CircuitBreakerConfig,
) -> Option<u64> {
    if !cfg.enabled {
        return None;
    }
    breaker.cooldown_remaining_secs(cfg)
}

/// Panic 模式试探：在全部候选中找"最早熔断"的 OPEN 渠道并抢占窗口名额。
///
/// 返回 `Some(idx)` 表示抢占成功——调用方应把该候选放行一次真实请求，
/// 其结果照常通过 `on_success` / `on_failure` / `trip_now` 上报熔断器，
/// 驱动 OPEN → HALF_OPEN → CLOSED 的恢复流程。
/// 返回 `None` 表示无熔断候选可用，或窗口限速拒绝了本次试探。
fn claim_panic_probe(
    state: &Arc<AppState>,
    model: &str,
    candidates: &[(ProviderConfig, Option<RouteConfig>)],
    window_secs: u64,
) -> Option<usize> {
    // (opened_at, 候选索引, 熔断器)：最早熔断者胜出
    let mut earliest: Option<(Instant, usize, Arc<CircuitBreaker>)> = None;
    for (i, (p, r)) in candidates.iter().enumerate() {
        let upstream_model = r.as_ref().map(|r| r.upstream_model.as_str()).unwrap_or("");
        let key = lb::route_key(model, &p.id, upstream_model);
        let Some(b) = state.breakers.get(&key) else {
            continue;
        };
        // 只考虑 OPEN 状态：HALF_OPEN 候选本就可参与正常选择（is_eligible）
        if b.status().state != crate::proxy::circuit::BreakerState::Open {
            continue;
        }
        let Some(at) = b.opened_at() else {
            continue;
        };
        if earliest.as_ref().map(|(t, _, _)| at < *t).unwrap_or(true) {
            earliest = Some((at, i, b.clone()));
        }
    }
    let (_, idx, breaker) = earliest?;
    breaker.try_claim_panic_probe(window_secs).then_some(idx)
}

/// 路由参数改写：`upstream_model` 覆盖 `model` 字段，`route.overrides` 覆盖其余字段。
///
/// 应用顺序：先 `upstream_model`，再 `overrides`（后者可进一步覆盖任意字段，包括 model）。
fn apply_overrides(
    body: &serde_json::Value,
    route: Option<&crate::proxy::config::RouteConfig>,
) -> serde_json::Value {
    let mut b = body.clone();
    if let Some(r) = route {
        if let Some(obj) = b.as_object_mut() {
            // 先应用 upstream_model（非空时改写请求体的 model）
            if !r.upstream_model.is_empty() {
                obj.insert(
                    "model".into(),
                    serde_json::Value::String(r.upstream_model.clone()),
                );
            }
            // 再合并 overrides（可进一步覆盖任意字段，包括 model）
            for (k, v) in &r.overrides {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    b
}

fn estimate_input_tokens(body: &serde_json::Value) -> Option<u64> {
    // 粗略估计：JSON 字节数 / 4
    body.to_string().len().checked_div(4).map(|v| v as u64)
}

pub(crate) fn finalize_log(state: &Arc<AppState>, log: RequestLog) {
    state.metrics.record(log.clone());
    state.emit_log(&log);
}

/// 把成功请求的 prompt + completion 存入 SQLite（用于数据存储）
pub(crate) fn save_to_db(
    state: &Arc<AppState>,
    log: &RequestLog,
    request_body: &serde_json::Value,
    response_body: &serde_json::Value,
    stream: bool,
) {
    let db = match state.db.lock().clone() {
        Some(db) => db,
        None => return,
    };
    let req_str = serde_json::to_string(request_body).unwrap_or_default();
    let resp_str = serde_json::to_string(response_body).unwrap_or_default();
    let finetune = build_finetune_jsonl(request_body, response_body, stream);
    spawn_insert(
        db,
        InsertRecord {
            ts: log.ts,
            model: log.model.clone(),
            upstream_model: log.upstream_model.clone(),
            provider: log.provider.clone(),
            stream,
            tokens_in: log.tokens_in,
            tokens_out: log.tokens_out,
            cache_hit: log.cache_hit,
            cached_tokens: log.cached_tokens,
            request_body: req_str,
            response_body: resp_str,
            finetune_jsonl: finetune,
        },
    );
}
