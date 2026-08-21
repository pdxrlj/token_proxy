// 端到端冒烟测试（lib 内 #[cfg(test)]，避免独立测试二进制的 WebView2 链接问题）：
// 进程内启动 mock 上游 + 代理服务器，验证 转发 / 流式 SSE / 重试 / 熔断 全链路。
use crate::proxy;
use crate::proxy::config::{AppConfig, ProviderConfig, RouteConfig};
use crate::proxy::state::AppState;
use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream;
use futures_util::stream::StreamExt;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

/// mock 记录每次收到的请求体（用于断言重试时参数不丢失）
type BodyRecorder = Arc<Mutex<Vec<Value>>>;

fn mock_router(counter: Arc<AtomicU32>, recorder: Option<BodyRecorder>) -> Router {
    Router::new()
        .route(
            "/v1/models",
            get(|| async {
                Json(json!({
                    "object": "list",
                    "data": [{ "id": "mock-model", "object": "model", "created": 0, "owned_by": "mock" }]
                }))
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| async move {
                if let Some(r) = &recorder {
                    r.lock().unwrap().push(body.clone());
                }
                let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let fail_code = body.get("x_mock_fail").and_then(|v| v.as_u64());
                let fail_n = body
                    .get("x_mock_fail_n")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let should_fail = fail_code.is_some() || (count as u64) <= fail_n;

                if should_fail {
                    let code = StatusCode::from_u16(fail_code.unwrap_or(500) as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    return (
                        code,
                        Json(json!({ "error": { "message": "mock upstream failure" } })),
                    )
                        .into_response();
                }

                let wants_stream = body
                    .get("stream")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if wants_stream {
                    let chunks = vec!["Hello", " from", " the", " mock", " upstream"];
                    let mut events: Vec<Result<Event, Infallible>> = chunks
                        .into_iter()
                        .map(|c| {
                            Ok(Event::default().data(
                                json!({
                                    "id": "chatcmpl-mock",
                                    "object": "chat.completion.chunk",
                                    "created": 0,
                                    "model": "mock-model",
                                    "choices": [{ "index": 0, "delta": { "content": c }, "finish_reason": null }]
                                })
                                .to_string(),
                            ))
                        })
                        .collect();
                    events.push(Ok(Event::default().data("[DONE]")));
                    let s = Sse::new(stream::iter(events)).keep_alive(KeepAlive::default());
                    return s.into_response();
                }

                (
                    StatusCode::OK,
                    Json(json!({
                        "id": "chatcmpl-mock",
                        "object": "chat.completion",
                        "created": 0,
                        "model": "mock-model",
                        "choices": [{
                            "index": 0,
                            "message": { "role": "assistant", "content": "Hello from the mock upstream" },
                            "finish_reason": "stop"
                        }],
                        "usage": { "prompt_tokens": 10, "completion_tokens": 6, "total_tokens": 16 }
                    })),
                )
                    .into_response()
            }),
        )
}

async fn spawn_mock(port: u16, counter: Arc<AtomicU32>) {
    spawn_mock_with_recorder(port, counter, None).await;
}

async fn spawn_mock_with_recorder(port: u16, counter: Arc<AtomicU32>, recorder: Option<BodyRecorder>) {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind mock port");
    let app = mock_router(counter, recorder);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock server");
    });
}

async fn spawn_proxy(port: u16, mock_port: u16, retry_override: Option<u32>) -> Arc<AppState> {
    let mut cfg = AppConfig::default();
    cfg.server.port = port;
    cfg.providers = vec![ProviderConfig {
        id: "mock".into(),
        name: "Mock".into(),
        base_url: format!("http://127.0.0.1:{mock_port}/v1"),
        api_key: String::new(),
        timeout_secs: 30,
        enabled: true,
        models: vec!["mock-model".into()],
    }];
    cfg.routes = vec![RouteConfig {
        model: "mock-model".into(),
        provider: "mock".into(),
        overrides: serde_json::Map::new(),
    }];
    if let Some(n) = retry_override {
        cfg.retry.max_retries = n;
    }
    let state = Arc::new(AppState::new(cfg, std::path::PathBuf::from("smoke.json")));
    let s = state.clone();
    tokio::spawn(async move {
        proxy::serve(s).await.expect("proxy server");
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    state
}

fn chat_body(stream: bool) -> Value {
    json!({
        "model": "mock-model",
        "stream": stream,
        "messages": [{ "role": "user", "content": "hi" }]
    })
}

#[tokio::test]
async fn smoke_full_chain() {
    // ---- 场景 A：models + 非流式转发 ----
    let mock_counter = Arc::new(AtomicU32::new(0));
    spawn_mock(9901, mock_counter.clone()).await;
    let _state = spawn_proxy(8791, 9901, None).await;
    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:8791";

    let models: Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "mock-model");

    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    if resp.status() != 200 {
        let status = resp.status();
        let t = resp.text().await.unwrap();
        panic!("unexpected status {status} body: {t}");
    }
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(
        v["choices"][0]["message"]["content"],
        "Hello from the mock upstream"
    );
    assert_eq!(v["usage"]["total_tokens"], 16);

    // ---- 场景 B：流式 SSE 透传 ----
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&chat_body(true))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(ct.contains("text/event-stream"), "content-type: {ct}");
    let text = resp.text().await.unwrap();
    assert!(text.contains("Hello"), "stream body: {text}");
    assert!(text.contains("[DONE]"), "missing [DONE] in: {text}");

    // ---- 场景 C：前两次失败，重试后成功 ----
    let mock_counter2 = Arc::new(AtomicU32::new(0));
    let recorder: BodyRecorder = Arc::new(Mutex::new(Vec::new()));
    spawn_mock_with_recorder(9902, mock_counter2.clone(), Some(recorder.clone())).await;
    let state2 = spawn_proxy(8792, 9902, Some(3)).await;
    let mut body = chat_body(false);
    body["x_mock_fail_n"] = json!(2);
    let resp = client
        .post("http://127.0.0.1:8792/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "should succeed after retries");
    assert_eq!(mock_counter2.load(Ordering::SeqCst), 3, "3 attempts expected");
    let snap = state2.metrics.snapshot();
    assert_eq!(snap.retries, 2, "retry counter");
    assert_eq!(snap.total, 1);

    // 重试不丢参数：三次收到的请求体必须完全一致（含原始 messages/stream 与注入的 x_mock_fail_n）
    let received = recorder.lock().unwrap();
    assert_eq!(received.len(), 3, "mock should receive 3 attempts");
    assert_eq!(received[0], received[1], "retry must resend identical body");
    assert_eq!(received[1], received[2], "retry must resend identical body");
    assert_eq!(received[0]["messages"][0]["content"], "hi", "messages preserved");
    assert_eq!(received[0]["stream"], json!(false), "stream flag preserved");
    assert_eq!(received[0]["x_mock_fail_n"], json!(2), "injected field preserved");
    assert!(received[0].get("model").is_some(), "model preserved");
    drop(received);

    // ---- 场景 D：永远失败 → 重试耗尽 → 502 ----
    let mock_counter3 = Arc::new(AtomicU32::new(0));
    spawn_mock(9903, mock_counter3.clone()).await;
    let state3 = spawn_proxy(8793, 9903, Some(2)).await;
    let mut body = chat_body(false);
    body["x_mock_fail"] = json!(500);
    let resp = client
        .post("http://127.0.0.1:8793/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    // 上游 500 透传给客户端（保留原始状态码语义）
    assert_eq!(resp.status(), 500);
    assert_eq!(mock_counter3.load(Ordering::SeqCst), 3, "1 + 2 retries");
    assert_eq!(state3.metrics.snapshot().failed, 1);

    // ---- 场景 E：熔断 ----
    let mock_counter4 = Arc::new(AtomicU32::new(0));
    spawn_mock(9904, mock_counter4.clone()).await;
    let state4 = spawn_proxy(8794, 9904, Some(3)).await;
    {
        let mut cfg = state4.config.write();
        cfg.retry.circuit_breaker.failure_threshold = 4;
        cfg.retry.circuit_breaker.cooldown_secs = 60;
    }
    let mut body = chat_body(false);
    body["x_mock_fail"] = json!(500);
    // 第 1 次请求：1 + 3 重试 = 4 次失败 → 熔断打开 → 上游 500 透传
    let resp = client
        .post("http://127.0.0.1:8794/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);
    assert_eq!(mock_counter4.load(Ordering::SeqCst), 4);
    // 第 2 次请求：熔断打开 → 立即 503，不再打到上游
    {
        let b = state4.breakers.get("mock").expect("breaker exists");
        let st = b.status();
        eprintln!("DEBUG breaker state={:?} failures={}", st.state, st.consecutive_failures);
        eprintln!("DEBUG cfg enabled={}", state4.config.read().retry.circuit_breaker.enabled);
    }
    let resp = client
        .post("http://127.0.0.1:8794/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(mock_counter4.load(Ordering::SeqCst), 4, "no more upstream calls");
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["type"], "circuit_open");
    // 手动重置熔断后恢复
    state4.breakers.get("mock").expect("breaker exists").reset();
    body["x_mock_fail_n"] = json!(0);
    body["x_mock_fail"] = Value::Null;
    let resp = client
        .post("http://127.0.0.1:8794/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "recovered after reset");
}
