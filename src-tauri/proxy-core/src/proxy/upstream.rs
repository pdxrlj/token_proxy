use crate::proxy::config::ProviderConfig;
use crate::proxy::retry::ProxyError;
use crate::proxy::state::AppState;
use axum::body::Body;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use futures_util::stream::StreamExt;
use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::warn;

/// 上游调用结果：完整 JSON 或 可转发的响应流
pub enum UpstreamOutcome {
    Json(serde_json::Value),
    Stream(reqwest::Response),
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
    Body::from_stream(
        resp.bytes_stream()
            .map(|c| c.map(axum::body::Bytes::from).map_err(std::io::Error::other)),
    )
}

/// SSE 流式转发：逐行解析并转发，统计 usage，断流时发 error 事件
pub fn forward_sse(
    state: Arc<AppState>,
    provider_name: String,
    model: String,
    resp: reqwest::Response,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let usage = Arc::new(Mutex::new(None::<(u64, u64)>));
    let bytes_total = Arc::new(AtomicU64::new(0));
    let metrics = state.metrics.clone();

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
                            }
                        }
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
                            yield Ok(Event::default().data(data.trim().to_string()));
                        }
                    }
                    break 'outer;
                }
            }
        }

        // 流结束：写入 token 统计（usage 缺失时按字节近似）
        let (input, output) = (*usage.lock()).unwrap_or((0, 0));
        let out_approx = if output == 0 {
            (bytes_total.load(Ordering::Relaxed) / 4) as u64
        } else {
            output
        };
        metrics.add_tokens(input, out_approx);
    };

    Sse::new(inner).keep_alive(KeepAlive::default())
}
