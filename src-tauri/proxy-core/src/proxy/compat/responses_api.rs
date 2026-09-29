//! OpenAI Responses API 兼容层（Codex）。
//!
//! Codex 默认以 Responses API 请求 `/v1/responses`：`instructions + input items`
//! 消息结构（message / function_call / function_call_output / reasoning）、
//! `response.created / response.output_text.delta / response.completed` SSE 事件流。
//! 这里把请求**翻译成 chat completions** 走主链路（路由 / 负载均衡 / 熔断 / 重试
//! 全部复用），响应再翻译回 Responses 格式。
//!
//! # 翻译规则
//! - `instructions` → system message；`input` 字符串 → 单条 user 消息
//! - input items：
//!   - `message`（user/system/developer → user/system；assistant 的 output_text）
//!   - 连续的 `function_call` → 聚合为一条 assistant 消息的 `tool_calls`
//!     （与 chat 协议"工具结果必须紧跟带 tool_calls 的 assistant 消息"对齐）
//!   - `function_call_output` → `role:"tool"` 消息（`call_id` 作为 `tool_call_id`）
//!   - `reasoning` / `item_reference` / `local_shell_call`：chat 无对应概念，跳过
//! - tools：Responses 的扁平 function 格式 → chat 嵌套格式；custom / local_shell
//!   等其它类型跳过（Codex 在 `wire_api="chat"` 下同样只用 function 工具）
//! - `max_output_tokens` → `max_tokens`；流式附加 `stream_options.include_usage`
//!
//! # Codex 侧接入
//! `~/.codex/config.toml` 中把 provider 指向本代理即可（见 README）。

use crate::proxy::api::{
    attach_error, check_auth, finalize_log, route_and_call, save_to_db, sse_response,
};
use crate::proxy::compat::{chunk_usage, estimate_tokens, next_sse_line, resolve_model_or_fallback};
use crate::proxy::headers::apply_upstream_response_headers;
use crate::proxy::retry::{ErrorContext, ProxyError};
use crate::proxy::state::{AppState, RequestLog};
use crate::proxy::upstream::{finish_stream_bookkeeping, StreamBookkeeping, UpstreamOutcome};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

// ─────────────────────────── 处理器 ───────────────────────────

/// `POST /v1/responses`：Codex 主端点。
pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let mut log = RequestLog::new();
    log.method = "POST".into();
    log.path = "/v1/responses".into();
    log.model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    log.stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    log.tokens_in = estimate_tokens(&body).into();

    // 本地鉴权
    {
        let cfg = state.config.read();
        if let Err(e) = check_auth(&state, &cfg, &headers) {
            return error_response(&e);
        }
    }

    // 模型兜底：codex 默认模型名（gpt-5-* 等）未命中路由时改写
    let fallback = {
        let cfg = state.config.read();
        cfg.compat.codex_default_model.clone()
    };
    let model = resolve_model_or_fallback(&state, &log.model, &fallback);
    log.model = model.clone();

    let chat_body = match translate_request(&body, &model) {
        Ok(b) => b,
        Err(msg) => {
            let e = ProxyError::BadRequest(msg);
            state.metrics.inc_total();
            state.metrics.inc_failed();
            state.metrics.add_latency(started.elapsed().as_millis() as u64);
            log.status = e.http_status().as_u16();
            log.latency_ms = started.elapsed().as_millis() as u64;
            attach_error(&mut log, &e, ErrorContext::default());
            finalize_log(&state, log);
            return error_response(&e);
        }
    };

    // 请求开始即推送"进行中"日志
    state.metrics.record(log.clone());
    state.emit_log(&log);

    let result = route_and_call(&state, &headers, "/chat/completions", &chat_body, &mut log).await;

    if let Err(e) = &result {
        if log.error_detail.is_none() {
            attach_error(
                &mut log,
                e,
                ErrorContext {
                    endpoint: Some("/v1/responses".to_string()),
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
            if let Some(u) = out.value.get("usage") {
                if let Some(pt) = u.get("prompt_tokens").and_then(|v| v.as_u64()) {
                    log.tokens_in = Some(pt);
                }
                log.tokens_out = u.get("completion_tokens").and_then(|v| v.as_u64());
                log.cache_hit = crate::proxy::upstream::detect_cache_hit(u);
                let ct = crate::proxy::upstream::extract_cached_tokens(u);
                log.cached_tokens = (ct > 0).then_some(ct);
            }
            let response = translate_response(&out.value, &model);
            save_to_db(&state, &log, &chat_body, &out.value, false);
            finalize_log(&state, log);
            let mut res = (StatusCode::OK, Json(response)).into_response();
            apply_upstream_response_headers(&mut res, &out.headers);
            res
        }
        Ok(UpstreamOutcome::StreamWithGuard { resp, guard }) => {
            state.metrics.inc_total();
            state.metrics.inc_success();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            let log_id = log.id.clone();
            let log_ts = log.ts;
            let log_model = log.model.clone();
            let log_upstream_model = log.upstream_model.clone();
            let log_provider = log.provider.clone();
            let inflight_key = guard.key().to_string();
            finalize_log(&state, log);
            let upstream_headers = resp.headers().clone();
            sse_response(
                stream_responses(
                    state.clone(),
                    resp,
                    Some(guard),
                    chat_body,
                    model,
                    log_id,
                    log_ts,
                    log_model,
                    log_upstream_model,
                    log_provider,
                    inflight_key,
                ),
                &upstream_headers,
            )
        }
        Ok(UpstreamOutcome::Stream(_)) => {
            let e = ProxyError::BadRequest("unexpected bare stream outcome".into());
            state.metrics.inc_total();
            state.metrics.inc_failed();
            finalize_log(&state, log);
            error_response(&e)
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
            error_response(&e)
        }
    }
}

// ─────────────────────────── 错误响应 ───────────────────────────

/// ProxyError → OpenAI 错误 JSON（`{"error":{"code":...,"message":...}}`）。
fn error_response(e: &ProxyError) -> Response {
    let status = e.http_status();
    let code = match e {
        ProxyError::Unauthorized => "invalid_api_key",
        ProxyError::QuotaExceeded(_, _) => "rate_limit_exceeded",
        ProxyError::RouteNotFound(_) => "model_not_found",
        ProxyError::BadRequest(_) => "invalid_request_error",
        ProxyError::CircuitOpen(_) => "server_error",
        ProxyError::Upstream { status: 429, .. } => "rate_limit_exceeded",
        ProxyError::Upstream { status: s, .. } if (500..600).contains(s) => "server_error",
        ProxyError::Upstream { .. } => "invalid_request_error",
        _ => "server_error",
    };
    let body = serde_json::json!({ "error": { "code": code, "message": e.to_string() } });
    (status, Json(body)).into_response()
}

// ─────────────────────────── 请求翻译 ───────────────────────────

/// Responses 请求 → chat completions 请求体。
pub(crate) fn translate_request(
    body: &serde_json::Value,
    model: &str,
) -> Result<serde_json::Value, String> {
    let mut messages: Vec<serde_json::Value> = Vec::new();

    // instructions → system message
    if let Some(instr) = body.get("instructions").and_then(|v| v.as_str()) {
        if !instr.is_empty() {
            messages.push(serde_json::json!({ "role": "system", "content": instr }));
        }
    }

    // 连续的 function_call items 聚合为一条 assistant 消息的 tool_calls：
    // chat 协议要求 tool 结果紧跟在携带对应 tool_calls 的 assistant 消息之后，
    // 逐条展开会产生"assistant(a) assistant(b) … tool(a) tool(b)"的非法顺序。
    let mut pending_tool_calls: Vec<serde_json::Value> = Vec::new();
    fn flush_tools(
        pending: &mut Vec<serde_json::Value>,
        messages: &mut Vec<serde_json::Value>,
    ) {
        if !pending.is_empty() {
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": serde_json::Value::Null,
                "tool_calls": pending,
            }));
            pending.clear();
        }
    }

    match body.get("input") {
        Some(serde_json::Value::String(s)) => {
            messages.push(serde_json::json!({ "role": "user", "content": s }));
        }
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(|t| t.as_str()).unwrap_or("message") {
                    "message" => {
                        flush_tools(&mut pending_tool_calls, &mut messages);
                        let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                        let chat_role = match role {
                            "assistant" => "assistant",
                            "system" | "developer" => "system",
                            _ => "user",
                        };
                        match item.get("content") {
                            Some(serde_json::Value::String(s)) => {
                                messages.push(
                                    serde_json::json!({ "role": chat_role, "content": s }),
                                );
                            }
                            Some(serde_json::Value::Array(parts)) => {
                                let mut content: Vec<serde_json::Value> = Vec::new();
                                for p in parts {
                                    match p.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                                        "input_text" | "output_text" | "text" => {
                                            if let Some(t) = p.get("text").and_then(|t| t.as_str())
                                            {
                                                content.push(serde_json::json!(
                                                    { "type": "text", "text": t }
                                                ));
                                            }
                                        }
                                        "input_image" => {
                                            let url = match p.get("image_url") {
                                                Some(serde_json::Value::String(u)) => u.clone(),
                                                Some(v) => v
                                                    .get("url")
                                                    .and_then(|u| u.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                None => String::new(),
                                            };
                                            if !url.is_empty() {
                                                content.push(serde_json::json!({
                                                    "type": "image_url",
                                                    "image_url": { "url": url }
                                                }));
                                            }
                                        }
                                        // refusal 等：取文本兜底
                                        _ => {
                                            if let Some(t) = p.get("text").and_then(|t| t.as_str())
                                            {
                                                content.push(serde_json::json!(
                                                    { "type": "text", "text": t }
                                                ));
                                            }
                                        }
                                    }
                                }
                                if !content.is_empty() {
                                    messages.push(serde_json::json!(
                                        { "role": chat_role, "content": content }
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                    "function_call" => {
                        pending_tool_calls.push(serde_json::json!({
                            "id": item.get("call_id").cloned().unwrap_or(serde_json::Value::Null),
                            "type": "function",
                            "function": {
                                "name": item.get("name").cloned().unwrap_or(serde_json::Value::Null),
                                "arguments": item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
                            }
                        }));
                    }
                    "function_call_output" => {
                        flush_tools(&mut pending_tool_calls, &mut messages);
                        let output = match item.get("output") {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(v) => v.to_string(),
                            None => String::new(),
                        };
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": item.get("call_id").cloned().unwrap_or(serde_json::Value::Null),
                            "content": output,
                        }));
                    }
                    // reasoning / item_reference / local_shell_call / 等：跳过
                    _ => {}
                }
            }
        }
        _ => {} // input 缺失：交给上游按空消息报错（语义与直连一致）
    }
    flush_tools(&mut pending_tool_calls, &mut messages);

    let stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    let mut chat = serde_json::json!({
        "model": model,
        "messages": messages,
    });
    if let Some(mt) = body.get("max_output_tokens") {
        if mt.is_u64() {
            chat["max_tokens"] = mt.clone();
        }
    }
    for src in ["temperature", "top_p", "parallel_tool_calls"] {
        if let Some(v) = body.get(src) {
            if v.is_number() || v.is_boolean() {
                chat[src] = v.clone();
            }
        }
    }

    // tools：扁平 function 格式 → chat 嵌套格式；其它类型跳过
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let fns: Vec<serde_json::Value> = tools
            .iter()
            .filter_map(|t| {
                if t.get("type").and_then(|v| v.as_str()) != Some("function") {
                    return None;
                }
                let name = t.get("name")?.as_str()?;
                let mut f = serde_json::json!({
                    "name": name,
                    "parameters": t.get("parameters").cloned().unwrap_or(serde_json::json!({"type":"object"})),
                });
                if let Some(d) = t.get("description").and_then(|d| d.as_str()) {
                    f["description"] = serde_json::json!(d);
                }
                if let Some(s) = t.get("strict") {
                    f["strict"] = s.clone();
                }
                Some(serde_json::json!({ "type": "function", "function": f }))
            })
            .collect();
        if !fns.is_empty() {
            chat["tools"] = serde_json::Value::Array(fns);
        }
    }
    // tool_choice：字符串直传；{type:function,name} → chat 嵌套
    match body.get("tool_choice") {
        Some(serde_json::Value::String(s)) => {
            chat["tool_choice"] = serde_json::json!(s);
        }
        Some(tc) if tc.get("type").and_then(|t| t.as_str()) == Some("function") => {
            if let Some(name) = tc.get("name") {
                chat["tool_choice"] = serde_json::json!({
                    "type": "function",
                    "function": { "name": name }
                });
            }
        }
        _ => {}
    }

    if stream {
        chat["stream"] = serde_json::Value::Bool(true);
        chat["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    Ok(chat)
}

// ─────────────────────────── 响应翻译 ───────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// usage → Responses 的 usage 结构。
fn responses_usage(input: u64, output: u64, cached: u64) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": input,
        "input_tokens_details": { "cached_tokens": cached },
        "output_tokens": output,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": input + output,
    })
}

/// chat completions 响应 → Responses 响应（非流式）。
pub(crate) fn translate_response(chat: &serde_json::Value, model: &str) -> serde_json::Value {
    let choice = chat.get("choices").and_then(|c| c.get(0));
    let message = choice.and_then(|c| c.get("message"));
    let mut output: Vec<serde_json::Value> = Vec::new();
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
    {
        output.push(serde_json::json!({
            "type": "message",
            "id": format!("msg_{}", uuid::Uuid::new_v4()),
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        }));
    }
    if let Some(tcs) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for tc in tcs {
            output.push(serde_json::json!({
                "type": "function_call",
                "id": format!("fc_{}", uuid::Uuid::new_v4()),
                "call_id": tc.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "name": tc.pointer("/function/name").cloned().unwrap_or(serde_json::Value::Null),
                "arguments": tc.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("{}"),
                "status": "completed",
            }));
        }
    }

    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str());
    let incomplete = finish == Some("length");
    let usage = chat.get("usage");
    let input = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let out = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = usage
        .and_then(|u| u.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    serde_json::json!({
        "id": chat.get("id").cloned().unwrap_or(serde_json::json!("resp_proxy")),
        "object": "response",
        "created_at": now_secs(),
        "status": if incomplete { "incomplete" } else { "completed" },
        "incomplete_details": if incomplete { serde_json::json!({"reason": "max_output_tokens"}) } else { serde_json::Value::Null },
        "model": model,
        "output": output,
        "usage": responses_usage(input, out, cached),
        "error": serde_json::Value::Null,
        "metadata": {},
    })
}

// ─────────────────────────── 流式翻译 ───────────────────────────

/// 上游 chat SSE → Responses SSE 事件流。
///
/// 事件顺序遵循 Responses 协议：`response.created` → 每个 output item 依次
/// `output_item.added` →（文本：`output_text.delta`…）→ `output_item.done`
/// → 最终 `response.completed`（聚合全部 output 与 usage）。
#[allow(clippy::too_many_arguments)]
fn stream_responses(
    state: Arc<AppState>,
    resp: reqwest::Response,
    guard: Option<crate::proxy::lb::InFlightGuard>,
    chat_body: serde_json::Value,
    model: String,
    log_id: String,
    log_ts: i64,
    log_model: String,
    log_upstream_model: Option<String>,
    log_provider: Option<String>,
    inflight_key: String,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let (tx, mut rx) = mpsc::channel::<(&'static str, String)>(64);
    let response_id = format!("resp_{}", uuid::Uuid::new_v4());
    let est_input = estimate_tokens(&chat_body);

    tokio::spawn(async move {
        // guard 随后台任务移动：任务结束时（含提前 break）析构归还在途计数
        let _inflight = guard;
        // 上游侧统计（chat 格式，供 StreamBookkeeping 入库）
        let mut usage: Option<(u64, u64)> = None;
        let mut cached_tokens: u64 = 0;
        let mut chunk_lines: Vec<String> = Vec::new();
        let mut bytes_total: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();

        // Responses 侧状态
        let mut next_output_index: usize = 0;
        // 文本消息 item（懒创建：首个 content 增量到达时 added）
        let mut text_item: Option<(usize, String, String)> = None; // (output_index, item_id, 累计 text)
        // OpenAI tool index → (output_index, item_id, call_id, name, arguments 累计)
        let mut tools: std::collections::HashMap<u64, (usize, String, String, String, String)> =
            std::collections::HashMap::new();
        let mut tool_order: Vec<u64> = Vec::new();
        let mut stop_reason: Option<String> = None;
        let mut failed = false;

        let send = |event: &'static str, data: serde_json::Value, tx: &mpsc::Sender<(&'static str, String)>| {
            let _ = tx.try_send((event, data.to_string()));
        };

        send(
            "response.created",
            serde_json::json!({
                "response": {
                    "id": response_id,
                    "object": "response",
                    "created_at": now_secs(),
                    "status": "in_progress",
                    "model": model,
                    "output": [],
                    "usage": serde_json::Value::Null,
                }
            }),
            &tx,
        );

        let close_text = |text: &mut Option<(usize, String, String)>,
                              tx: &mpsc::Sender<(&'static str, String)>|
         -> Option<(usize, String, String)> {
            if let Some((oi, item_id, acc)) = text.take() {
                let _ = tx.try_send((
                    "response.output_text.done",
                    serde_json::json!({
                        "item_id": item_id,
                        "output_index": oi,
                        "content_index": 0,
                        "text": acc,
                    })
                    .to_string(),
                ));
                let _ = tx.try_send((
                    "response.content_part.done",
                    serde_json::json!({
                        "item_id": item_id,
                        "output_index": oi,
                        "content_index": 0,
                        "part": { "type": "output_text", "text": acc, "annotations": [] },
                    })
                    .to_string(),
                ));
                let _ = tx.try_send((
                    "response.output_item.done",
                    serde_json::json!({
                        "output_index": oi,
                        "item": {
                            "type": "message",
                            "id": item_id,
                            "status": "completed",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": acc, "annotations": [] }],
                        }
                    })
                    .to_string(),
                ));
                return Some((oi, item_id, acc));
            }
            None
        };
        let close_tool = |tools: &mut std::collections::HashMap<u64, (usize, String, String, String, String)>,
                              ti: u64,
                              tx: &mpsc::Sender<(&'static str, String)>| {
            if let Some((oi, item_id, _, _, acc)) = tools.get(&ti) {
                let _ = tx.try_send((
                    "response.function_call_arguments.done",
                    serde_json::json!({
                        "item_id": item_id,
                        "output_index": oi,
                        "arguments": acc,
                    })
                    .to_string(),
                ));
                let _ = tx.try_send((
                    "response.output_item.done",
                    serde_json::json!({
                        "output_index": oi,
                        "item": {
                            "type": "function_call",
                            "id": item_id,
                            "call_id": tools.get(&ti).map(|(_, _, c, _, _)| c.clone()).unwrap_or_default(),
                            "name": tools.get(&ti).map(|(_, _, _, n, _)| n.clone()).unwrap_or_default(),
                            "arguments": acc,
                            "status": "completed",
                        }
                    })
                    .to_string(),
                ));
            }
        };

        'outer: loop {
            if let Some(line) = next_sse_line(&mut buf) {
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    break 'outer;
                }
                let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                chunk_lines.push(data.to_string());
                if let Some((p, c, cached)) = chunk_usage(&chunk) {
                    usage = Some((p, c));
                    cached_tokens = cached;
                }
                let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
                    continue; // 仅含 usage 的收尾块
                };
                let delta = choice.get("delta");

                // 文本增量
                if let Some(text) = delta
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                    .filter(|s| !s.is_empty())
                {
                    if text_item.is_none() {
                        let oi = next_output_index;
                        next_output_index += 1;
                        let item_id = format!("msg_{}", uuid::Uuid::new_v4());
                        send(
                            "response.output_item.added",
                            serde_json::json!({
                                "output_index": oi,
                                "item": {
                                    "type": "message",
                                    "id": item_id,
                                    "status": "in_progress",
                                    "role": "assistant",
                                    "content": [],
                                }
                            }),
                            &tx,
                        );
                        send(
                            "response.content_part.added",
                            serde_json::json!({
                                "item_id": item_id,
                                "output_index": oi,
                                "content_index": 0,
                                "part": { "type": "output_text", "text": "", "annotations": [] },
                            }),
                            &tx,
                        );
                        text_item = Some((oi, item_id, String::new()));
                    }
                    let (oi, item_id, acc) = text_item.as_mut().expect("text item just created");
                    acc.push_str(text);
                    send(
                        "response.output_text.delta",
                        serde_json::json!({
                            "item_id": item_id,
                            "output_index": oi,
                            "content_index": 0,
                            "delta": text,
                        }),
                        &tx,
                    );
                }

                // 工具调用增量
                if let Some(tcs) = delta
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(|t| t.as_array())
                {
                    for tc in tcs {
                        let ti = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                        if !tools.contains_key(&ti) {
                            // 新工具调用：先闭合文本 item（chat 协议下文本与工具
                            // 调用同属一个 message，但 Responses 要求分 item）
                            close_text(&mut text_item, &tx);
                            let oi = next_output_index;
                            next_output_index += 1;
                            let item_id = format!("fc_{}", uuid::Uuid::new_v4());
                            let call_id = tc
                                .get("id")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                                .unwrap_or("")
                                .to_string();
                            let name = tc
                                .pointer("/function/name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            send(
                                "response.output_item.added",
                                serde_json::json!({
                                    "output_index": oi,
                                    "item": {
                                        "type": "function_call",
                                        "id": item_id,
                                        "call_id": call_id,
                                        "name": name,
                                        "arguments": "",
                                        "status": "in_progress",
                                    }
                                }),
                                &tx,
                            );
                            tools.insert(ti, (oi, item_id, call_id, name, String::new()));
                            tool_order.push(ti);
                        }
                        if let Some(args) = tc
                            .pointer("/function/arguments")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            let entry = tools.get_mut(&ti).expect("tool entry just inserted");
                            entry.4.push_str(args);
                            send(
                                "response.function_call_arguments.delta",
                                serde_json::json!({
                                    "item_id": entry.1,
                                    "output_index": entry.0,
                                    "delta": args,
                                }),
                                &tx,
                            );
                        }
                    }
                }

                if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                    stop_reason = Some(fr.to_string());
                }
                continue;
            }

            match stream.next().await {
                Some(Ok(bytes)) => {
                    bytes_total += bytes.len() as u64;
                    buf.extend_from_slice(&bytes);
                }
                Some(Err(e)) => {
                    // 断流计入熔断失败：与 forward_sse 语义一致
                    if let Some(b) = state.breakers.get(&inflight_key) {
                        let cb_cfg = state.config.read().retry.circuit_breaker.clone();
                        b.on_failure(&cb_cfg);
                    }
                    let _ = tx
                        .send((
                            "response.failed",
                            serde_json::json!({
                                "response": {
                                    "id": response_id,
                                    "status": "failed",
                                    "error": {
                                        "code": "api_error",
                                        "message": format!("upstream stream interrupted: {e}"),
                                    },
                                }
                            })
                            .to_string(),
                        ))
                        .await;
                    failed = true;
                    break 'outer;
                }
                None => break 'outer,
            }
        }

        if !failed {
            // 闭合未完成的 item：文本 item（通常是 output_index 0）先闭合，
            // 工具 item 按出现顺序闭合；done 事件按 item_id 关联，顺序无碍
            let finalized_text = close_text(&mut text_item, &tx);
            for ti in &tool_order {
                close_tool(&mut tools, *ti, &tx);
            }

            // 聚合最终 output（按 output_index 排序）
            let mut items: Vec<(usize, serde_json::Value)> = Vec::new();
            if let Some((oi, item_id, acc)) = finalized_text {
                items.push((
                    oi,
                    serde_json::json!({
                        "type": "message",
                        "id": item_id,
                        "status": "completed",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": acc, "annotations": [] }],
                    }),
                ));
            }
            for ti in &tool_order {
                if let Some((oi, item_id, call_id, name, args)) = tools.get(ti) {
                    items.push((
                        *oi,
                        serde_json::json!({
                            "type": "function_call",
                            "id": item_id,
                            "call_id": call_id,
                            "name": name,
                            "arguments": args,
                            "status": "completed",
                        }),
                    ));
                }
            }
            items.sort_by_key(|(oi, _)| *oi);
            let output: Vec<serde_json::Value> = items.into_iter().map(|(_, v)| v).collect();

            let input_tokens = usage.map(|(p, _)| p).unwrap_or(est_input);
            let output_tokens = usage
                .map(|(_, c)| c)
                .unwrap_or(0);
            let incomplete = stop_reason.as_deref() == Some("length");
            send(
                "response.completed",
                serde_json::json!({
                    "response": {
                        "id": response_id,
                        "object": "response",
                        "created_at": now_secs(),
                        "status": if incomplete { "incomplete" } else { "completed" },
                        "incomplete_details": if incomplete { serde_json::json!({"reason": "max_output_tokens"}) } else { serde_json::Value::Null },
                        "model": model,
                        "output": output,
                        "usage": responses_usage(input_tokens, output_tokens, cached_tokens),
                        "error": serde_json::Value::Null,
                        "metadata": {},
                    }
                }),
                &tx,
            );
        }

        finish_stream_bookkeeping(
            &state,
            StreamBookkeeping {
                log_id,
                log_ts,
                log_model,
                log_upstream_model,
                log_provider,
                req_body: chat_body,
                chunk_lines,
                usage,
                bytes_total,
                cache_hit: cached_tokens > 0,
                cached_tokens,
            },
        );
        drop(tx);
    });

    let inner = async_stream::stream! {
        while let Some((event, data)) = rx.recv().await {
            yield Ok(Event::default().event(event).data(data));
        }
    };

    Sse::new(inner).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn translate_request_basic() {
        let body = json!({
            "model": "gpt-5-codex",
            "instructions": "Be terse.",
            "input": "hi",
            "max_output_tokens": 64,
            "temperature": 0.2,
        });
        let chat = translate_request(&body, "glm-4.6").unwrap();
        assert_eq!(chat["model"], "glm-4.6");
        assert_eq!(chat["max_tokens"], 64);
        assert_eq!(chat["temperature"], 0.2);
        let msgs = chat["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be terse.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "hi");
        // reasoning / store / metadata 等不透传
        let body2 = json!({
            "model": "m", "input": [],
            "reasoning": { "effort": "high" }, "store": false,
            "metadata": {}, "include": ["reasoning.encrypted_content"],
        });
        let chat2 = translate_request(&body2, "m").unwrap();
        assert!(chat2.get("reasoning").is_none());
        assert!(chat2.get("store").is_none());
        assert!(chat2.get("include").is_none());
    }

    #[test]
    fn translate_request_function_calls() {
        let body = json!({
            "model": "gpt-5-codex",
            "tools": [
                { "type": "function", "name": "run_cmd", "description": "run", "strict": false,
                  "parameters": { "type": "object", "properties": { "cmd": { "type": "string" } } } },
                { "type": "web_search" },
            ],
            "tool_choice": "auto",
            "input": [
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "list" }] },
                // 连续两条 function_call → 聚合为一条 assistant 消息的两个 tool_calls
                { "type": "function_call", "call_id": "c1", "name": "run_cmd", "arguments": "{\"cmd\":\"ls\"}" },
                { "type": "function_call", "call_id": "c2", "name": "run_cmd", "arguments": "{\"cmd\":\"pwd\"}" },
                { "type": "function_call_output", "call_id": "c1", "output": "a" },
                { "type": "function_call_output", "call_id": "c2", "output": "b" },
                { "type": "reasoning", "summary": [] },
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "done" }] },
            ],
        });
        let chat = translate_request(&body, "m").unwrap();
        let tools = chat["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1, "web_search 应被跳过");
        assert_eq!(tools[0]["function"]["name"], "run_cmd");
        assert_eq!(tools[0]["function"]["strict"], false);

        let msgs = chat["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        let tcs = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 2, "连续 function_call 应聚合: {msgs:?}");
        assert_eq!(tcs[0]["id"], "c1");
        assert_eq!(tcs[1]["id"], "c2");
        let tool_msgs: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(tool_msgs.len(), 2);
        assert_eq!(tool_msgs[0]["tool_call_id"], "c1");
        assert_eq!(tool_msgs[0]["content"], "a");
        assert_eq!(tool_msgs[1]["content"], "b");
        // reasoning 之后的 assistant 消息保留（content 为 parts 数组）
        assert_eq!(msgs.last().unwrap()["content"][0]["text"], "done");
    }

    #[test]
    fn translate_response_items() {
        let chat = json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "hello",
                    "tool_calls": [
                        { "id": "call_1", "type": "function", "function": { "name": "f", "arguments": "{\"x\":2}" } }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 9, "completion_tokens": 3,
                       "prompt_tokens_details": { "cached_tokens": 2 } }
        });
        let v = translate_response(&chat, "glm-4.6");
        assert_eq!(v["object"], "response");
        assert_eq!(v["status"], "completed");
        let output = v["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[0]["content"][0]["text"], "hello");
        assert_eq!(output[1]["type"], "function_call");
        assert_eq!(output[1]["call_id"], "call_1");
        assert_eq!(output[1]["arguments"], "{\"x\":2}");
        assert_eq!(v["usage"]["input_tokens"], 9);
        assert_eq!(v["usage"]["input_tokens_details"]["cached_tokens"], 2);
        assert_eq!(v["usage"]["total_tokens"], 12);

        // length → incomplete
        let chat2 = json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "x" }, "finish_reason": "length" }],
        });
        let v2 = translate_response(&chat2, "m");
        assert_eq!(v2["status"], "incomplete");
        assert_eq!(v2["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn chunk_usage_extracts_and_ignores_empty() {
        assert_eq!(
            chunk_usage(&json!({"usage": {"prompt_tokens": 3, "completion_tokens": 4}})),
            Some((3, 4, 0))
        );
        assert_eq!(
            chunk_usage(&json!({"usage": {"prompt_tokens": 3, "completion_tokens": 4,
                                          "prompt_tokens_details": {"cached_tokens": 1}}})),
            Some((3, 4, 1))
        );
        // 空 usage 对象 / 缺 usage → None
        assert_eq!(chunk_usage(&json!({"usage": {}})), None);
        assert_eq!(chunk_usage(&json!({"choices": []})), None);
    }
}
