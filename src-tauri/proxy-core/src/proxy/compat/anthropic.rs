//! Anthropic Messages API 兼容层（Claude Code）。
//!
//! 请求翻译（Anthropic → chat completions）：
//! - `system`（字符串 / blocks）→ system message
//! - user 消息中的 `tool_result` blocks → `role:"tool"` 消息（一条一个结果），
//!   `text` / `image` blocks → user 消息的 text / image_url parts
//! - assistant 消息中的 `tool_use` blocks → `tool_calls`，`thinking` blocks 丢弃
//! - `tools[].input_schema` → `tools[].function.parameters`；
//!   `tool_choice` auto/any/none/tool → auto/required/none/function
//! - `max_tokens` / `temperature` / `top_p` / `stop_sequences` → 同名字段
//! - `thinking` / `metadata` 等无对应概念的字段整体丢弃，不透传给上游
//!
//! 响应翻译（chat completions → Anthropic）：
//! - `choices[0].message.content` → `[{type:"text"}]`，`tool_calls` → `[{type:"tool_use"}]`
//! - `finish_reason` stop/length/tool_calls → `stop_reason` end_turn/max_tokens/tool_use
//! - `usage` → `input_tokens` / `output_tokens` / `cache_read_input_tokens`
//!
//! 流式翻译（chat SSE → Anthropic SSE 事件流）：
//! `message_start` → `content_block_start` → `content_block_delta`…
//! → `content_block_stop` → `message_delta`（stop_reason + output_tokens）→ `message_stop`。
//! text 块与每个 tool call 各占一个 content block，按上游出现顺序编号。

use crate::proxy::api::{
    attach_error, check_auth, finalize_log, route_and_call, save_to_db, sse_response,
};
use crate::proxy::compat::{
    chunk_usage, estimate_tokens, next_sse_line, resolve_model_or_fallback, strip_context_suffix,
};
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
use std::time::Instant;
use tokio::sync::mpsc;

// ─────────────────────────── 处理器 ───────────────────────────

/// `POST /v1/messages`：Claude Code 主端点。
pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let mut log = RequestLog::new();
    log.method = "POST".into();
    log.path = "/v1/messages".into();
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

    // 本地鉴权（x-api-key / Bearer 均可）
    {
        let cfg = state.config.read();
        if let Err(e) = check_auth(&state, &cfg, &headers) {
            return error_response(&e);
        }
    }

    // 模型兜底：claude-* 未命中路由时改写为配置的默认模型
    let fallback = {
        let cfg = state.config.read();
        cfg.compat.claude_code_default_model.clone()
    };
    let requested = strip_context_suffix(&log.model);
    let model = resolve_model_or_fallback(&state, requested, &fallback);
    log.model = model.clone();

    // 翻译请求（失败 = 请求本身不合法）
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
                    endpoint: Some("/v1/messages".to_string()),
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
            let message = translate_response(&out.value, &model);
            save_to_db(&state, &log, &chat_body, &out.value, false);
            finalize_log(&state, log);
            let mut res = (StatusCode::OK, Json(message)).into_response();
            apply_upstream_response_headers(&mut res, &out.headers);
            res
        }
        Ok(UpstreamOutcome::StreamWithGuard { resp, guard }) => {
            // 流式：头部返回即视为成功，流内错误走 error 事件
            state.metrics.inc_total();
            state.metrics.inc_success();
            state
                .metrics
                .add_latency(started.elapsed().as_millis() as u64);
            log.status = 200;
            log.latency_ms = started.elapsed().as_millis() as u64;
            log.ttfb_ms = Some(log.latency_ms);
            // 流结束后记账所需字段（finalize_log 会消费 log，先取出）
            let log_id = log.id.clone();
            let log_ts = log.ts;
            let log_model = log.model.clone();
            let log_upstream_model = log.upstream_model.clone();
            let log_provider = log.provider.clone();
            let inflight_key = guard.key().to_string();
            finalize_log(&state, log);
            let upstream_headers = resp.headers().clone();
            sse_response(
                stream_anthropic(
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
            // route_and_call 总会把裸流包装成 StreamWithGuard；防御性处理
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

/// `POST /v1/messages/count_tokens`：Claude Code 的 token 预估端点。
///
/// 翻译成 chat 请求体后按字节数粗估，不发起上游请求。
pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    {
        let cfg = state.config.read();
        if let Err(e) = check_auth(&state, &cfg, &headers) {
            return error_response(&e);
        }
    }
    let fallback = {
        let cfg = state.config.read();
        cfg.compat.claude_code_default_model.clone()
    };
    let requested = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let model = resolve_model_or_fallback(&state, strip_context_suffix(&requested), &fallback);
    let input_tokens = match translate_request(&body, &model) {
        Ok(chat) => estimate_tokens(&chat),
        Err(_) => estimate_tokens(&body),
    };
    Json(serde_json::json!({ "input_tokens": input_tokens })).into_response()
}

// ─────────────────────────── 错误响应 ───────────────────────────

/// ProxyError → Anthropic 错误 JSON（`{"type":"error","error":{...}}`）。
fn error_response(e: &ProxyError) -> Response {
    let status = e.http_status();
    let err_type = match e {
        ProxyError::Unauthorized => "authentication_error",
        ProxyError::QuotaExceeded(_, _) => "rate_limit_error",
        ProxyError::RouteNotFound(_) => "not_found_error",
        ProxyError::BadRequest(_) => "invalid_request_error",
        ProxyError::CircuitOpen(_) => "overloaded_error",
        ProxyError::Upstream { status: 429, .. } => "rate_limit_error",
        ProxyError::Upstream { status: s, .. } if (500..600).contains(s) => "api_error",
        ProxyError::Upstream { .. } => "invalid_request_error",
        _ => "api_error",
    };
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": err_type, "message": e.to_string() }
    });
    (status, Json(body)).into_response()
}

// ─────────────────────────── 请求翻译 ───────────────────────────

/// Anthropic Messages 请求 → chat completions 请求体。
pub(crate) fn translate_request(
    body: &serde_json::Value,
    model: &str,
) -> Result<serde_json::Value, String> {
    let mut messages: Vec<serde_json::Value> = Vec::new();

    // system：字符串或 blocks 数组 → 单条 system 消息
    match body.get("system") {
        Some(serde_json::Value::String(s)) => {
            if !s.is_empty() {
                messages.push(serde_json::json!({ "role": "system", "content": s }));
            }
        }
        Some(serde_json::Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n");
            if !text.is_empty() {
                messages.push(serde_json::json!({ "role": "system", "content": text }));
            }
        }
        _ => {}
    }

    let msgs = body
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or_else(|| "messages must be an array".to_string())?;
    for m in msgs {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        match m.get("content") {
            Some(serde_json::Value::String(s)) => {
                messages.push(serde_json::json!({ "role": role, "content": s }));
            }
            Some(serde_json::Value::Array(blocks)) => {
                translate_blocks(role, blocks, &mut messages);
            }
            _ => return Err("message.content must be a string or an array of blocks".into()),
        }
    }

    let stream = body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    let mut chat = serde_json::json!({
        "model": model,
        // Anthropic 必填 max_tokens；缺失时给一个保守默认
        "max_tokens": body.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(4096),
        "messages": messages,
    });

    // tools：input_schema → function.parameters
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let fns: Vec<serde_json::Value> = tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?;
                Some(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": t.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                        "parameters": t.get("input_schema").cloned().unwrap_or(serde_json::json!({"type":"object"})),
                    }
                }))
            })
            .collect();
        if !fns.is_empty() {
            chat["tools"] = serde_json::Value::Array(fns);
        }
    }

    // tool_choice：auto/any/none/tool → auto/required/none/function
    if let Some(tc) = body.get("tool_choice").and_then(|t| t.as_object()) {
        match tc.get("type").and_then(|t| t.as_str()) {
            Some("auto") => chat["tool_choice"] = serde_json::json!("auto"),
            Some("any") => chat["tool_choice"] = serde_json::json!("required"),
            Some("none") => chat["tool_choice"] = serde_json::json!("none"),
            Some("tool") => {
                if let Some(name) = tc.get("name") {
                    chat["tool_choice"] = serde_json::json!({
                        "type": "function",
                        "function": { "name": name }
                    });
                }
            }
            _ => {}
        }
    }

    for (src, dst) in [("temperature", "temperature"), ("top_p", "top_p")] {
        if let Some(v) = body.get(src) {
            if v.is_number() {
                chat[dst] = v.clone();
            }
        }
    }
    if let Some(stops) = body.get("stop_sequences").and_then(|s| s.as_array()) {
        if !stops.is_empty() {
            chat["stop"] = serde_json::Value::Array(stops.clone());
        }
    }
    if stream {
        chat["stream"] = serde_json::Value::Bool(true);
        chat["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    Ok(chat)
}

/// 翻译一条 Anthropic 消息的 content blocks，追加为若干 chat 消息。
///
/// user 消息中的 `tool_result` 块转成独立的 `role:"tool"` 消息（需紧跟在携带
/// 对应 `tool_use` 的 assistant 消息之后，恰好与 Anthropic 的消息顺序一致）；
/// 其余 text / image 块合并为一条 user 消息。assistant 消息的 `tool_use` 块
/// 聚合为 `tool_calls`，`thinking` 块丢弃。
fn translate_blocks(role: &str, blocks: &[serde_json::Value], out: &mut Vec<serde_json::Value>) {
    if role == "assistant" {
        let mut text_parts: Vec<&str> = Vec::new();
        let mut tool_calls: Vec<serde_json::Value> = Vec::new();
        for b in blocks {
            match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        text_parts.push(t);
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(serde_json::json!({
                        "id": b.get("id").cloned().unwrap_or(serde_json::Value::Null),
                        "type": "function",
                        "function": {
                            "name": b.get("name").cloned().unwrap_or(serde_json::Value::Null),
                            "arguments": b.get("input").cloned().unwrap_or(serde_json::json!({})).to_string(),
                        }
                    }));
                }
                // thinking / redacted_thinking：chat 无对应概念，丢弃
                _ => {}
            }
        }
        let content = if text_parts.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(text_parts.concat())
        };
        let mut msg = serde_json::json!({ "role": "assistant", "content": content });
        if !tool_calls.is_empty() {
            msg["tool_calls"] = serde_json::Value::Array(tool_calls);
        }
        out.push(msg);
        return;
    }

    // user / 其他角色
    let mut parts: Vec<serde_json::Value> = Vec::new();
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    parts.push(serde_json::json!({ "type": "text", "text": t }));
                }
            }
            Some("image") => {
                // base64 图像 → data URI
                let src = b.get("source");
                let media = src
                    .and_then(|s| s.get("media_type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("image/png");
                let data = src
                    .and_then(|s| s.get("data"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !data.is_empty() {
                    parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{media};base64,{data}") }
                    }));
                }
            }
            Some("tool_result") => {
                let tool_use_id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                let content = match b.get("content") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Array(arr)) => arr
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                out.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
            // thinking / server_tool_use / web_search_tool_result 等：丢弃
            _ => {}
        }
    }
    if !parts.is_empty() {
        out.push(serde_json::json!({ "role": role, "content": parts }));
    }
}

// ─────────────────────────── 响应翻译 ───────────────────────────

fn map_stop_reason(finish: Option<&str>) -> &'static str {
    match finish {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        _ => "end_turn",
    }
}

/// chat completions 响应 → Anthropic Messages 响应。
pub(crate) fn translate_response(chat: &serde_json::Value, model: &str) -> serde_json::Value {
    let choice = chat.get("choices").and_then(|c| c.get(0));
    let message = choice.and_then(|c| c.get("message"));
    let mut content: Vec<serde_json::Value> = Vec::new();
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
    {
        content.push(serde_json::json!({ "type": "text", "text": text }));
    }
    if let Some(tcs) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for tc in tcs {
            let input: serde_json::Value = tc
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .and_then(|a| serde_json::from_str(a).ok())
                .unwrap_or(serde_json::json!({}));
            content.push(serde_json::json!({
                "type": "tool_use",
                "id": tc.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "name": tc.pointer("/function/name").cloned().unwrap_or(serde_json::Value::Null),
                "input": input,
            }));
        }
    }

    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str());
    let usage = chat.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = usage
        .and_then(|u| u.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    serde_json::json!({
        "id": chat.get("id").cloned().unwrap_or(serde_json::json!("msg_proxy")),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": map_stop_reason(finish),
        "stop_sequence": serde_json::Value::Null,
        "usage": {
            "input_tokens": input_tokens,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": cached,
            "output_tokens": output_tokens,
        }
    })
}

// ─────────────────────────── 流式翻译 ───────────────────────────

/// 上游 chat SSE → Anthropic SSE 事件流。
///
/// 复用 `forward_sse` 的记账（usage / 字节 / 日志回写 / 入库），仅替换事件生成；
/// 上游断流同样计入熔断失败。
#[allow(clippy::too_many_arguments)]
fn stream_anthropic(
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
    let message_id = format!("msg_{}", uuid::Uuid::new_v4());
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

        // Anthropic 侧状态
        let mut next_block_index: usize = 0;
        let mut open_block: Option<usize> = None;
        // OpenAI tool index → (anthropic block index, tool_call_id, name, arguments 累计)
        let mut tools: std::collections::HashMap<u64, (usize, String, String, String)> =
            std::collections::HashMap::new();
        let mut tool_order: Vec<u64> = Vec::new();
        let mut stop_reason: &'static str = "end_turn";
        let mut out_chars: usize = 0;

        let send = |event: &'static str, data: serde_json::Value, tx: &mpsc::Sender<(&'static str, String)>| {
            let _ = tx.try_send((event, data.to_string()));
        };

        send(
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": serde_json::Value::Null,
                    "stop_sequence": serde_json::Value::Null,
                    "usage": { "input_tokens": est_input, "output_tokens": 0 }
                }
            }),
            &tx,
        );

        let close_open = |open: &mut Option<usize>,
                              tx: &mpsc::Sender<(&'static str, String)>| {
            if let Some(idx) = open.take() {
                let _ = tx.try_send((
                    "content_block_stop",
                    serde_json::json!({ "type": "content_block_stop", "index": idx }).to_string(),
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
                let Some(choice) = chunk
                    .get("choices")
                    .and_then(|c| c.get(0))
                else {
                    continue; // 仅含 usage 的收尾块
                };
                let delta = choice.get("delta");

                // 文本增量
                if let Some(text) = delta
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                    .filter(|s| !s.is_empty())
                {
                    if open_block.is_none() {
                        close_open(&mut open_block, &tx);
                        let idx = next_block_index;
                        next_block_index += 1;
                        send(
                            "content_block_start",
                            serde_json::json!({
                                "type": "content_block_start",
                                "index": idx,
                                "content_block": { "type": "text", "text": "" }
                            }),
                            &tx,
                        );
                        open_block = Some(idx);
                    }
                    out_chars += text.len();
                    send(
                        "content_block_delta",
                        serde_json::json!({
                            "type": "content_block_delta",
                            "index": open_block.unwrap_or(0),
                            "delta": { "type": "text_delta", "text": text }
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
                            close_open(&mut open_block, &tx);
                            let idx = next_block_index;
                            next_block_index += 1;
                            let id = tc
                                .get("id")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("toolu_{:012x}", uuid::Uuid::new_v4().as_u128()));
                            let name = tc
                                .pointer("/function/name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            send(
                                "content_block_start",
                                serde_json::json!({
                                    "type": "content_block_start",
                                    "index": idx,
                                    "content_block": {
                                        "type": "tool_use",
                                        "id": id,
                                        "name": name,
                                        "input": {}
                                    }
                                }),
                                &tx,
                            );
                            tools.insert(ti, (idx, id, name, String::new()));
                            tool_order.push(ti);
                            open_block = Some(idx);
                        }
                        if let Some(args) = tc
                            .pointer("/function/arguments")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            let (_, _, _, acc) = tools.get_mut(&ti).expect("tool entry just inserted");
                            acc.push_str(args);
                            let block_index = tools.get(&ti).map(|(i, _, _, _)| *i).unwrap_or(0);
                            send(
                                "content_block_delta",
                                serde_json::json!({
                                    "type": "content_block_delta",
                                    "index": block_index,
                                    "delta": { "type": "input_json_delta", "partial_json": args }
                                }),
                                &tx,
                            );
                        }
                    }
                }

                if let Some(fr) = choice
                    .get("finish_reason")
                    .and_then(|f| f.as_str())
                {
                    stop_reason = map_stop_reason(Some(fr));
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
                            "error",
                            serde_json::json!({
                                "type": "error",
                                "error": {
                                    "type": "api_error",
                                    "message": format!("upstream stream interrupted: {e}")
                                }
                            })
                            .to_string(),
                        ))
                        .await;
                    break 'outer;
                }
                None => break 'outer, // 上游 EOF（Anthropic 不发 [DONE]，事件流自然收尾）
            }
        }

        // 收尾：关闭未闭合块 → message_delta（stop_reason + output_tokens）→ message_stop
        close_open(&mut open_block, &tx);
        let output_tokens = usage.map(|(_, c)| c).unwrap_or((out_chars / 4) as u64);
        send(
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": serde_json::Value::Null },
                "usage": { "output_tokens": output_tokens }
            }),
            &tx,
        );
        send("message_stop", serde_json::json!({ "type": "message_stop" }), &tx);

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
    use serde_json::json;

    #[test]
    fn translate_request_basic() {
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 256,
            "system": "be nice",
            "temperature": 0.5,
            "stop_sequences": ["END"],
            "messages": [
                { "role": "user", "content": "hello" },
                { "role": "assistant", "content": "hi there" },
            ],
        });
        let chat = translate_request(&body, "glm-4.6").unwrap();
        assert_eq!(chat["model"], "glm-4.6");
        assert_eq!(chat["max_tokens"], 256);
        assert_eq!(chat["temperature"], 0.5);
        assert_eq!(chat["stop"], json!(["END"]));
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(chat["messages"][1]["content"], "hello");
        assert_eq!(chat["messages"][2]["content"], "hi there");
        assert!(chat.get("stream").is_none());
        // thinking / metadata 等字段不得透传
        let body2 = json!({
            "model": "m", "max_tokens": 1, "thinking": { "type": "enabled" },
            "metadata": { "user_id": "u" },
            "messages": [],
        });
        let chat2 = translate_request(&body2, "m").unwrap();
        assert!(chat2.get("thinking").is_none());
        assert!(chat2.get("metadata").is_none());
    }

    #[test]
    fn translate_request_tool_blocks() {
        let body = json!({
            "model": "m",
            "max_tokens": 100,
            "tools": [{
                "name": "get_weather",
                "description": "query weather",
                "input_schema": { "type": "object", "properties": { "city": { "type": "string" } } },
            }],
            "tool_choice": { "type": "any" },
            "messages": [
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "internal", "signature": "sig" },
                    { "type": "tool_use", "id": "tu_1", "name": "get_weather", "input": { "city": "BJ" } },
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "tu_1", "content": "sunny 25C" },
                    { "type": "text", "text": "and tomorrow?" },
                ] },
            ],
        });
        let chat = translate_request(&body, "m").unwrap();
        let tools = chat["tools"].as_array().unwrap();
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(chat["tool_choice"], "required");

        let msgs = chat["messages"].as_array().unwrap();
        // assistant：thinking 丢弃，tool_use → tool_calls
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["tool_calls"][0]["id"], "tu_1");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"BJ\"}"
        );
        // user 中的 tool_result → role:"tool" 消息；text 保留为 user 消息
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["tool_call_id"], "tu_1");
        assert_eq!(tool["content"], "sunny 25C");
        let user = msgs.last().unwrap();
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"][0]["text"], "and tomorrow?");
    }

    #[test]
    fn translate_response_blocks_and_usage() {
        let chat = json!({
            "id": "chatcmpl-x",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        { "id": "call_9", "type": "function", "function": { "name": "f", "arguments": "{\"a\":1}" } }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "prompt_tokens_details": { "cached_tokens": 4 }
            }
        });
        let v = translate_response(&chat, "glm-4.6");
        assert_eq!(v["type"], "message");
        assert_eq!(v["stop_reason"], "tool_use");
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["id"], "call_9");
        assert_eq!(v["content"][0]["input"]["a"], 1);
        assert_eq!(v["usage"]["input_tokens"], 12);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 4);
        assert_eq!(v["usage"]["output_tokens"], 8);

        // 纯文本 + stop
        let chat2 = json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 2 }
        });
        let v2 = translate_response(&chat2, "m");
        assert_eq!(v2["content"][0]["text"], "hi");
        assert_eq!(v2["stop_reason"], "end_turn");
        // length → max_tokens
        let chat3 = json!({
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "x" }, "finish_reason": "length" }],
        });
        assert_eq!(translate_response(&chat3, "m")["stop_reason"], "max_tokens");
    }

    #[test]
    fn translate_request_stream_adds_usage_option() {
        let body = json!({
            "model": "m", "max_tokens": 10, "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        });
        let chat = translate_request(&body, "m").unwrap();
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["stream_options"]["include_usage"], true);
    }
}
