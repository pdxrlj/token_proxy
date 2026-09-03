use crate::proxy::config::ProviderConfig;
use crate::proxy::headers::build_upstream_headers;
use crate::proxy::retry::ProxyError;
use crate::proxy::state::AppState;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::StreamExt;
use futures_util::Stream;
use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::warn;

/// 完整 JSON 响应 + 上游响应头。
///
/// 带上响应头是为了把上游的 `x-request-id` / 限流 / 组织等头回传给客户端，
/// 否则客户端看到的世界与直连不一致（排障时拿不到上游追踪 ID）。
pub struct JsonOutcome {
    pub value: serde_json::Value,
    pub headers: HeaderMap,
}

/// 上游调用结果：完整 JSON 或 可转发的响应流
pub enum UpstreamOutcome {
    Json(JsonOutcome),
    /// 尚未绑定在途计数 guard 的原始流（仅由 `call_upstream` 产生）。
    ///
    /// `route_and_call` 会立即将其包装为 [`UpstreamOutcome::StreamWithGuard`]，
    /// 因此业务层不应直接匹配此变体。
    Stream(reqwest::Response),
    /// 流式响应 + 在途计数 guard（业务层唯一应处理的流式变体）。
    ///
    /// SSE 流的生命周期跨越 `route_and_call` 返回之后，guard 随之转移给
    /// 流式转发逻辑，在流真正结束时归还计数；这样 `LeastConnections`
    /// 策略才能感知到流式请求的并发，不至于退化成"永远选第一个候选"。
    StreamWithGuard {
        resp: reqwest::Response,
        guard: crate::proxy::lb::InFlightGuard,
    },
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
///
/// `client_headers` 是客户端发往本代理的原始请求头，按白名单透传给上游，
/// 使上游看到的请求与客户端直连时一致（详见 [`crate::proxy::headers`]）。
pub async fn call_upstream(
    state: &AppState,
    provider: &ProviderConfig,
    path: &str,
    body: serde_json::Value,
    client_headers: &HeaderMap,
) -> Result<UpstreamOutcome, ProxyError> {
    let url = provider.endpoint(path);
    let mut req = state.http.post(&url).json(&body);
    // 逐个 header 追加（而非整体替换）：保留 reqwest 自动设置的
    // content-type / host 等，只覆盖同名的那几项。
    for (name, value) in build_upstream_headers(client_headers, provider) {
        req = req.header(name, value);
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
    // 上游响应头需要回传给客户端（追踪 ID、限流余量等），
    // 而 resp 随后会被消费成 JSON / 字节流，故先 clone。
    let resp_headers = resp.headers().clone();
    if status.is_success() {
        let wants_stream = body
            .get("stream")
            .and_then(|s| s.as_bool())
            .unwrap_or(false);
        if wants_stream {
            // 流式请求不在此时归还在途计数：由 route_and_call 接手 guard
            // 并随流转移，在流真正结束时归还。
            return Ok(UpstreamOutcome::Stream(resp));
        }
        let json = resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| ProxyError::Network(format!("invalid upstream json: {e}")))?;
        Ok(UpstreamOutcome::Json(JsonOutcome {
            value: json,
            headers: resp_headers,
        }))
    } else {
        let err_body = resp
            .json::<serde_json::Value>()
            .await
            .unwrap_or_else(|_| serde_json::json!({ "message": "upstream error" }));
        Err(ProxyError::Upstream {
            status: status.as_u16(),
            body: err_body,
            headers: resp_headers,
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
///
/// `provider_id` 用于在上游流中断时向对应熔断器记一次失败，
/// 使持续断流的上游也能被熔断/故障切换机制隔离。
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
    _log_tokens_in: Option<u64>,
    provider_id: Option<String>,
    // 在途计数 guard：随流转移进来，在流结束时归还。
    // 为 `None` 时表示调用方未启用在途计数（如兜底透传路径）。
    inflight: Option<crate::proxy::lb::InFlightGuard>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // 后台任务向此 channel 发送已序列化的 SSE data 行；客户端从接收端读取。
    // channel 容量有限：当客户端断连后接收端被丢弃，发送端会返回错误，
    // 后台任务据此感知断连但仍会继续消费完上游流并完成统计写入。
    let (tx, mut rx) = mpsc::channel::<String>(64);

    // 后台任务：消费上游字节流，解析 SSE 行，统计 usage / 字节，聚合入库
    tokio::spawn(async move {
        // guard 随后台任务移动：无论流是正常结束、出错还是被提前中断，
        // 都会在任务结束时（guard 析构）归还在途计数。
        let _inflight = inflight;
        let usage = Mutex::new(None::<(u64, u64)>);
        let mut cache_hit = false;
        let mut cached_tokens: u64 = 0;
        let mut bytes_total: u64 = 0;
        let mut collected: Vec<String> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();

        // 辅助：处理一行 SSE，返回 true 表示应继续，false 表示遇到 [DONE] 或错误
        let send = |data: String, tx: &mpsc::Sender<String>| -> bool {
            // channel 发送失败说明客户端已断连，但这里仍返回 true 继续消费上游
            let _ = tx.try_send(data);
            true
        };

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
                                let input =
                                    u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
                                let output = u
                                    .get("completion_tokens")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0);
                                *usage.lock() = Some((input, output));
                                let ct = extract_cached_tokens(u);
                                if ct > 0 {
                                    cache_hit = true;
                                    cached_tokens = ct;
                                }
                            }
                        }
                        collected.push(data.to_string());
                    }
                    send(data.to_string(), &tx);
                } else {
                    // 非标准行，保底原样透传
                    send(line, &tx);
                }
                continue;
            }

            // 缓冲不足一行，读取下一数据块
            match stream.next().await {
                Some(Ok(bytes)) => {
                    bytes_total += bytes.len() as u64;
                    buf.extend_from_slice(&bytes);
                }
                Some(Err(e)) => {
                    warn!(provider = %provider_name, model = %model, error = %e, "upstream stream interrupted");
                    // 断流计入熔断失败：持续断流的上游同样应被隔离
                    if let Some(pid) = provider_id.as_deref() {
                        if let Some(b) = state.breakers.get(pid) {
                            b.on_failure();
                        }
                    }
                    let _ = tx
                        .send(
                            serde_json::json!({
                                "error": {
                                    "message": format!("upstream stream interrupted: {e}"),
                                    "type": "upstream_stream_error",
                                    "code": null,
                                }
                            })
                            .to_string(),
                        )
                        .await;
                    break 'outer;
                }
                None => {
                    // EOF：冲刷残留数据
                    if !buf.is_empty() {
                        let tail = String::from_utf8_lossy(&buf).trim().to_string();
                        if let Some(data) = tail.strip_prefix("data:") {
                            let data = data.trim();
                            if data != "[DONE]" {
                                collected.push(data.to_string());
                            }
                            send(data.to_string(), &tx);
                        }
                    }
                    break 'outer;
                }
            }
        }

        // 上游流结束（或出错）：无论客户端是否断连，都完成统计写入
        let (input, output) = (*usage.lock()).unwrap_or((0, 0));
        // 先聚合，得到聚合后的响应体大小，用于 tokens_out 估算和 response_bytes
        let chunk_vals: Vec<serde_json::Value> = collected
            .iter()
            .filter_map(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .collect();
        let agg = crate::proxy::db::aggregate_stream_response(&chunk_vals, &model, *usage.lock());
        let agg_bytes = serde_json::to_vec(&agg)
            .map(|b| b.len() as u64)
            .unwrap_or(bytes_total);
        // 上游未返回 usage 时，用聚合后响应体大小 / 4 估算 tokens_out
        // （比用 SSE 原始流字节数 / 4 更准确，排除了协议开销）
        let out_approx = if output == 0 {
            (agg_bytes / 4) as u64
        } else {
            output
        };
        state.metrics.add_tokens(input, out_approx);
        // bytes_in：统计请求体大小；bytes_out：用聚合后响应体大小（非 SSE 原始流）
        let req_bytes = serde_json::to_vec(&req_body)
            .map(|b| b.len() as u64)
            .unwrap_or(0);
        state.metrics.add_bytes(req_bytes, agg_bytes);
        let ct_opt = if cached_tokens > 0 {
            Some(cached_tokens)
        } else {
            None
        };
        // 回写流式统计到对应日志条目（含缓存命中状态）
        // tokens_in 用上游返回的真实 prompt_tokens 覆盖估算值
        let real_tokens_in = if input > 0 { Some(input) } else { None };
        state
            .metrics
            .update_log_stream_stats(&log_id, agg_bytes, Some(out_approx), real_tokens_in);
        state
            .metrics
            .update_log_cache_hit(&log_id, cache_hit, ct_opt);
        // 推送更新后的完整日志到前端（局部刷新字节数、tokens、缓存命中）
        state.emit_log_update(&log_id);
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
                    // 用上游返回的真实 prompt_tokens 覆盖估算值
                    tokens_in: real_tokens_in,
                    tokens_out: Some(out_approx),
                    cache_hit,
                    cached_tokens: ct_opt,
                    request_body: req_str,
                    response_body: resp_str,
                    finetune_jsonl: finetune,
                },
            );
        }
        // drop tx 让接收端收到 None 从而结束客户端流
        drop(tx);
    });

    // 客户端流：从 channel 接收已序列化的 data，包装为 SSE Event 转发
    let inner = async_stream::stream! {
        while let Some(data) = rx.recv().await {
            yield Ok(Event::default().data(data));
        }
    };

    Sse::new(inner).keep_alive(KeepAlive::default())
}
