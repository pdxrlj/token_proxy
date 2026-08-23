use crate::proxy::config::ProviderConfig;
use crate::proxy::retry::ProxyError;
use crate::proxy::state::AppState;
use axum::body::Body;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::StreamExt;
use futures_util::Stream;
use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tracing::warn;

/// 上游调用结果：完整 JSON 或 可转发的响应流
pub enum UpstreamOutcome {
    Json(serde_json::Value),
    Stream(reqwest::Response),
}

/// 从 usage 对象提取缓存命中的 token 数量（返回 0 表示未命中）。
///
/// 不同上游厂商字段命名各异，统一在此处归一化：
/// - OpenAI / DeepSeek / Qwen：`prompt_tokens_details.cached_tokens`
/// - Anthropic：`cache_read_input_tokens`（>0 即命中）
/// - 其它兼容实现：`cached_tokens` / `cache_read_tokens` 等常见变体
pub fn extract_cached_tokens(usage: &serde_json::Value) -> u64 {
    // OpenAI 风格：prompt_tokens_details.cached_tokens
    if let Some(cached) = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
    {
        return cached;
    }
    // Anthropic 风格
    if let Some(cached) = usage
        .get("cache_read_input_tokens")
        .and_then(|x| x.as_u64())
    {
        return cached;
    }
    // 通用变体
    for key in ["cached_tokens", "cache_read_tokens", "cache_hit_tokens"] {
        if let Some(cached) = usage.get(key).and_then(|x| x.as_u64()) {
            return cached;
        }
    }
    0
}

/// 从 usage 对象判断是否命中上游缓存（cached_tokens > 0）。
pub fn detect_cache_hit(usage: &serde_json::Value) -> bool {
    extract_cached_tokens(usage) > 0
}

/// 向指定 provider 发起 OpenAI 兼容请求
pub async fn call_upstream(
    state: &AppState,
    provider: &ProviderConfig,
    path: &str,
    body: serde_json::Value,
) -> Result<UpstreamOutcome, ProxyError> {
    let url = provider.endpoint(path);
    let mut req = state.http.post(&url).json(&body);
    if !provider.api_key.is_empty() {
        req = req.bearer_auth(&provider.api_key);
    }
    let timeout = std::time::Duration::from_secs(provider.timeout_secs.max(30));
    let resp = req.timeout(timeout).send().await.map_err(|e| {
        if e.is_timeout() {
            ProxyError::Timeout(e.to_string())
        } else {
            ProxyError::Network(e.to_string())
        }
    })?;

    let status = resp.status();
    if status.is_success() {
        let wants_stream = body
            .get("stream")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        if wants_stream {
            return Ok(UpstreamOutcome::Stream(resp));
        }
        let json = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| ProxyError::Network(format!("invalid upstream json: {e}")))?;
        Ok(UpstreamOutcome::Json(json))
    } else {
        let err_body = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or_else(|_| serde_json::json!({ "message": "upstream error" }));
        Err(ProxyError::Upstream {
            status: status.as_u16(),
            body: err_body,
        })
    }
}

/// 把上游响应体原样作为字节流转发（用于非 OpenAI 语义的兜底端点）
pub fn forward_raw(resp: reqwest::Response) -> Body {
    Body::from_stream(resp.bytes_stream().map(|c| {
        c.map(axum::body::Bytes::from)
            .map_err(std::io::Error::other)
    }))
}

/// SSE 流式转发：逐行解析并转发，统计 usage，断流时发 error 事件
pub fn forward_sse(
    state: Arc<AppState>,
    provider_name: String,
    model: String,
    resp: reqwest::Response,
    log_id: String,
    req_body: serde_json::Value,
    log_ts: i64,
    log_model: String,
    log_upstream_model: Option<String>,
    log_provider: Option<String>,
    log_tokens_in: Option<u64>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let usage = Arc::new(Mutex::new(None::<(u64, u64)>));
    let cache_hit = Arc::new(AtomicBool::new(false));
    let cached_tokens = Arc::new(AtomicU64::new(0));
    let bytes_total = Arc::new(AtomicU64::new(0));
    let metrics = state.metrics.clone();
    // 收集所有 SSE data 行，用于流结束后聚合
    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let inner = async_stream::stream! {
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();

        'outer: loop {
            // 缓冲中已有完整行 → 切出并转发
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..pos]).trim().to_string();
                buf.drain(..=pos);
                if line.is_empty() || line.starts_with(':') {
                    continue; // 空行 / 心跳注释，跳过
                }
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim();
                    if data != "[DONE]" {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                            if let Some(u) = v.get("usage") {
                                let input = u
                                    .get("prompt_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0);
                                let output = u
                                    .get("completion_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0);
                                *usage.lock() = Some((input, output));
                                // 缓存命中检测 + 缓存 token 数提取
                                let ct = extract_cached_tokens(u);
                                if ct > 0 {
                                    cache_hit.store(true, Ordering::Relaxed);
                                    cached_tokens.store(ct, Ordering::Relaxed);
                                }
                            }
                        }
                        // 收集 data 行用于后续聚合
                        collected.lock().push(data.to_string());
                    }
                    yield Ok(Event::default().data(data.to_string()));
                } else {
                    // 非标准行，保底原样透传
                    yield Ok(Event::default().data(line));
                }
                continue;
            }

            // 缓冲不足一行，读取下一数据块
            match stream.next().await {
                Some(Ok(bytes)) => {
                    bytes_total.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    buf.extend_from_slice(&bytes);
                }
                Some(Err(e)) => {
                    warn!(provider = %provider_name, model = %model, error = %e, "upstream stream interrupted");
                    yield Ok(
                        Event::default()
                            .event("error")
                            .data(
                                serde_json::json!({
                                    "error": {
                                        "message": format!("upstream stream interrupted: {e}"),
                                        "type": "upstream_stream_error",
                                        "code": null,
                                    }
                                })
                                .to_string(),
                            ),
                    );
                    break 'outer;
                }
                None => {
                    // EOF：冲刷残留数据
                    if !buf.is_empty() {
                        let tail = String::from_utf8_lossy(&buf).trim().to_string();
                        if let Some(data) = tail.strip_prefix("data:") {
                            let data = data.trim();
                            if data != "[DONE]" {
                                collected.lock().push(data.to_string());
                            }
                            yield Ok(Event::default().data(data.to_string()));
                        }
                    }
                    break 'outer;
                }
            }
        }

        // 流结束：写入 token 统计（usage 缺失时按字节近似）
        let (input, output) = (*usage.lock()).unwrap_or((0, 0));
        let total_bytes = bytes_total.load(Ordering::Relaxed);
        let out_approx = if output == 0 {
            (total_bytes / 4) as u64
        } else {
            output
        };
        metrics.add_tokens(input, out_approx);
        metrics.add_bytes(0, total_bytes);
        let hit = cache_hit.load(Ordering::Relaxed);
        let ct = cached_tokens.load(Ordering::Relaxed);
        let ct_opt = if ct > 0 { Some(ct) } else { None };
        // 回写流式统计到对应日志条目（含缓存命中状态）
        state
            .metrics
            .update_log_stream_stats(&log_id, total_bytes, Some(out_approx));
        state
            .metrics
            .update_log_cache_hit(&log_id, hit, ct_opt);

        // 聚合 SSE chunks 并异步写入数据库
        let chunks = std::mem::take(&mut *collected.lock());
        // 把 String 解析为 serde_json::Value
        let chunk_vals: Vec<serde_json::Value> = chunks
            .iter()
            .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .collect();
        let agg = crate::proxy::db::aggregate_stream_response(
            &chunk_vals,
            &model,
            *usage.lock(),
        );
        // 写入 DB
        let db_guard = state.db.lock().clone();
        if let Some(db) = db_guard {
            let req_str = serde_json::to_string(&req_body).unwrap_or_default();
            let resp_str = serde_json::to_string(&agg).unwrap_or_default();
            let finetune = crate::proxy::db::build_finetune_jsonl(&req_body, &agg, true);
            crate::proxy::db::spawn_insert(
                db,
                crate::proxy::db::InsertRecord {
                    ts: log_ts,
                    model: log_model,
                    upstream_model: log_upstream_model,
                    provider: log_provider,
                    stream: true,
                    tokens_in: log_tokens_in,
                    tokens_out: Some(out_approx),
                    cache_hit: hit,
                    cached_tokens: ct_opt,
                    request_body: req_str,
                    response_body: resp_str,
                    finetune_jsonl: finetune,
                },
            );
        }
    };

    Sse::new(inner).keep_alive(KeepAlive::default())
}
