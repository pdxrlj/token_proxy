use crate::proxy::circuit::CircuitBreaker;
use crate::proxy::config::{AppConfig, Candidate, ProviderConfig, RouteConfig};
use crate::proxy::db::{build_finetune_jsonl, spawn_insert, InsertRecord};
use crate::proxy::lb;
use crate::proxy::retry::{backoff_delay, ProxyError};
use crate::proxy::state::{AppState, RequestLog};
use crate::proxy::upstream::{
    call_upstream, detect_cache_hit, extract_cached_tokens, forward_raw, forward_sse,
    UpstreamOutcome,
};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;

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
        .route("/v1/responses", post(chat_completions))
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

fn check_auth(headers: &HeaderMap, cfg: &AppConfig) -> Result<(), ProxyError> {
    let key = cfg.server.local_api_key.trim();
    if key.is_empty() {
        return Ok(());
    }
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(bearer) = auth.strip_prefix("Bearer ") {
        if bearer.trim() == key {
            return Ok(());
        }
    }
    Err(ProxyError::Unauthorized)
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
        if let Err(e) = check_auth(&headers, &cfg) {
            return (e.http_status(), Json(e.openai_error())).into_response();
        }
    }

    // 请求开始即推送"进行中"日志（status=0 表示进行中）
    state.metrics.record(log.clone());
    state.emit_log(&log);

    let result = route_and_call(&state, &headers, "/chat/completions", &body, &mut log).await;

    match result {
        Ok(UpstreamOutcome::Json(v)) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            let resp_bytes = serde_json::to_vec(&v).map(|b| b.len() as u64).unwrap_or(0);
            log.response_bytes = Some(resp_bytes);
            state.metrics.add_bytes(0, resp_bytes);
            let usage = v.get("usage");
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
            save_to_db(&state, &log, &body, &v, false);
            finalize_log(&state, log);
            (StatusCode::OK, Json(v)).into_response()
        }
        Ok(UpstreamOutcome::Stream(resp)) => {
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
            let log_provider_id = log.provider_id.clone();
            finalize_log(&state, log);
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
                log_provider_id,
            )
            .into_response()
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
            (e.http_status(), Json(e.openai_error())).into_response()
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
        if let Err(e) = check_auth(&headers, &cfg) {
            return (e.http_status(), Json(e.openai_error())).into_response();
        }
    }

    let started = Instant::now();
    // 请求开始即推送"进行中"日志（status=0 表示进行中）
    state.metrics.record(log.clone());
    state.emit_log(&log);

    match route_and_call(&state, &headers, &path, &body, &mut log).await {
        Ok(UpstreamOutcome::Json(v)) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            let resp_bytes = serde_json::to_vec(&v).map(|b| b.len() as u64).unwrap_or(0);
            log.response_bytes = Some(resp_bytes);
            state.metrics.add_bytes(0, resp_bytes);
            // 缓存命中检测
            let cache_hit = v.get("usage").map(|u| detect_cache_hit(u)).unwrap_or(false);
            log.cache_hit = cache_hit;
            let cached_tokens = v
                .get("usage")
                .map(|u| extract_cached_tokens(u))
                .filter(|val| *val > 0);
            log.cached_tokens = cached_tokens;
            if cache_hit {
                state.metrics.inc_cache_hit();
            }
            save_to_db(&state, &log, &body, &v, false);
            finalize_log(&state, log);
            (StatusCode::OK, Json(v)).into_response()
        }
        Ok(UpstreamOutcome::Stream(resp)) => {
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
            let log_provider_id = log.provider_id.clone();
            let req_body = body.clone();
            finalize_log(&state, log);
            let ct = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/event-stream")
                .to_string();
            // SSE 流式：走 forward_sse 以统计字节数和 usage
            if ct.contains("text/event-stream") {
                return forward_sse(
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
                    log_provider_id,
                )
                .into_response();
            }
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, ct)],
                forward_raw(resp),
            )
                .into_response()
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
            (e.http_status(), Json(e.openai_error())).into_response()
        }
    }
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
        return Err(ProxyError::RouteNotFound(model.to_string()));
    }

    let lb_cfg = cfg.load_balance.clone();

    // 提取会话 Key（用于粘性路由）
    let session_key = if lb_cfg.sticky.enabled {
        lb::extract_session_key(headers, body, &lb_cfg.sticky.session_key_source)
    } else {
        None
    };

    // 负载均衡选择（在 read lock 内完成，因为 select 需要引用候选）
    let selected_idx = if lb_cfg.enabled && candidates.len() > 1 {
        lb::select(state, &candidates, model, session_key.as_deref(), &lb_cfg).unwrap_or(0)
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
async fn route_and_call(
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

        let breaker = state
            .breakers
            .entry(provider.id.clone())
            .or_insert_with(|| Arc::new(CircuitBreaker::new(breaker_cfg.clone())))
            .clone();

        // ── 2. 熔断检查：打开时 failover 到下一候选 ──
        if breaker_cfg.enabled && breaker.before_request().is_err() {
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
            return Err(ProxyError::CircuitOpen(provider.id.clone()));
        }

        // ── 3. 在途计数 + 调用上游 ──
        let upstream_model_str = route
            .as_ref()
            .map(|r| r.upstream_model.as_str())
            .unwrap_or("");
        let inflight_key = lb::route_key(model, &provider.id, upstream_model_str);
        lb::inc_in_flight(state, &inflight_key);
        let call_started = Instant::now();

        match call_upstream(state, provider, path, body.clone()).await {
            Ok(outcome) => {
                // 成功：记录延迟样本，返回结果
                breaker.on_success();
                lb::dec_in_flight(state, &inflight_key);
                lb::record_latency(
                    state,
                    &inflight_key,
                    call_started.elapsed().as_millis() as u64,
                );
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
            Err(e) if e.retriable(&retry_cfg) && attempt < retry_cfg.max_retries => {
                // 可重试错误：退避后重试（同一候选，保持上下文）
                if breaker_cfg.enabled {
                    breaker.on_failure();
                }
                lb::dec_in_flight(state, &inflight_key);
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
                tokio::time::sleep(backoff_delay(attempt, &retry_cfg)).await;
            }
            Err(e) => {
                // 不可重试或重试耗尽：failover 到下一候选
                if breaker_cfg.enabled {
                    breaker.on_failure();
                }
                lb::dec_in_flight(state, &inflight_key);
                if candidates_tried < total_candidates {
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
                return Err(e);
            }
        }
    }
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

fn finalize_log(state: &Arc<AppState>, log: RequestLog) {
    state.metrics.record(log.clone());
    state.emit_log(&log);
}

/// 把成功请求的 prompt + completion 存入 SQLite（用于数据存储）
fn save_to_db(
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
