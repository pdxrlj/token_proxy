use crate::proxy::circuit::CircuitBreaker;
use crate::proxy::config::AppConfig;
use crate::proxy::retry::{ProxyError, backoff_delay};
use crate::proxy::state::{AppState, RequestLog};
use crate::proxy::upstream::{UpstreamOutcome, call_upstream, forward_raw, forward_sse};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
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

async fn list_models(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
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
    log.stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    log.tokens_in = estimate_input_tokens(&body);

    // 本地鉴权
    {
        let cfg = state.config.read();
        if let Err(e) = check_auth(&headers, &cfg) {
            return (e.http_status(), Json(e.openai_error())).into_response();
        }
    }

    let result = route_and_call(&state, "/chat/completions", &body, &mut log).await;

    match result {
        Ok(UpstreamOutcome::Json(v)) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state.metrics.add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.tokens_out = v
                .get("usage")
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|t| t.as_u64());
            state.metrics.add_tokens(
                log.tokens_in.unwrap_or(0),
                log.tokens_out.unwrap_or(0),
            );
            finalize_log(&state, log);
            (StatusCode::OK, Json(v)).into_response()
        }
        Ok(UpstreamOutcome::Stream(resp)) => {
            // 流式响应：头部返回即视为成功，流内错误走 error 事件
            let provider_name = log.provider.clone().unwrap_or_default();
            let model = log.model.clone();
            state.metrics.inc_total();
            state.metrics.inc_success();
            state.metrics.add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            finalize_log(&state, log);
            forward_sse(state.clone(), provider_name, model, resp).into_response()
        }
        Err(e) => {
            state.metrics.inc_total();
            state.metrics.inc_failed();
            state.metrics.add_latency(started.elapsed().as_millis() as u64);
            log.status = e.http_status().as_u16();
            log.latency_ms = started.elapsed().as_millis() as u64;
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
    log.stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    // 本地鉴权
    {
        let cfg = state.config.read();
        if let Err(e) = check_auth(&headers, &cfg) {
            return (e.http_status(), Json(e.openai_error())).into_response();
        }
    }

    let started = Instant::now();
    match route_and_call(&state, &path, &body, &mut log).await {
        Ok(UpstreamOutcome::Json(v)) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state.metrics.add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            finalize_log(&state, log);
            (StatusCode::OK, Json(v)).into_response()
        }
        Ok(UpstreamOutcome::Stream(resp)) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state.metrics.add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            finalize_log(&state, log);
            let ct = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/event-stream")
                .to_string();
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
            state.metrics.add_latency(started.elapsed().as_millis() as u64);
            log.status = e.http_status().as_u16();
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.error = Some(e.to_string());
            finalize_log(&state, log);
            (e.http_status(), Json(e.openai_error())).into_response()
        }
    }
}

/// 路由 + 熔断 + 重试循环
async fn route_and_call(
    state: &Arc<AppState>,
    path: &str,
    body: &serde_json::Value,
    log: &mut RequestLog,
) -> Result<UpstreamOutcome, ProxyError> {
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ProxyError::BadRequest("model is required".into()))?;

    let (breaker_cfg, retry_cfg, provider) = {
        let cfg = state.config.read();
        let (provider, route) = cfg
            .resolve_provider(model)
            .ok_or_else(|| ProxyError::RouteNotFound(model.to_string()))?;
        let retry_cfg = cfg.retry.clone();
        let breaker_cfg = cfg.retry.circuit_breaker.clone();
        let provider = provider.clone();
        let body = apply_overrides(body, route);
        (breaker_cfg, retry_cfg, (provider, body))
    };

    let (provider, body) = provider;
    log.provider = Some(provider.name.clone());

    let breaker = state
        .breakers
        .entry(provider.id.clone())
        .or_insert_with(|| Arc::new(CircuitBreaker::new(breaker_cfg.clone())))
        .clone();

    let mut attempt: u32 = 0;
    loop {
        if !breaker_cfg.enabled {
            // 熔断关闭时直接放行
        } else if let Err(_) = breaker.before_request() {
            return Err(ProxyError::CircuitOpen(provider.id.clone()));
        }

        match call_upstream(state, &provider, path, body.clone()).await {
            Ok(outcome) => {
                breaker.on_success();
                log.retries = attempt;
                return Ok(outcome);
            }
            Err(e) if e.retriable(&retry_cfg) && attempt < retry_cfg.max_retries => {
                if breaker_cfg.enabled {
                    breaker.on_failure();
                }
                attempt += 1;
                state.metrics.inc_retries();
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
                if breaker_cfg.enabled {
                    breaker.on_failure();
                }
                return Err(e);
            }
        }
    }
}

/// 路由参数改写：route.overrides 覆盖请求体字段
fn apply_overrides(
    body: &serde_json::Value,
    route: Option<&crate::proxy::config::RouteConfig>,
) -> serde_json::Value {
    let mut b = body.clone();
    if let Some(r) = route {
        if let Some(obj) = b.as_object_mut() {
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
