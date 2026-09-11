// 端到端冒烟测试（lib 内 #[cfg(test)]，避免独立测试二进制的 WebView2 链接问题）：
// 进程内启动 mock 上游 + 代理服务器，验证 转发 / 流式 SSE / 重试 / 熔断 全链路。
use crate::proxy;
use crate::proxy::config::{AppConfig, ProviderConfig, RouteConfig};
use crate::proxy::lb;
use crate::proxy::state::AppState;
use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::TcpListener;

/// mock 记录每次收到的请求体（用于断言重试时参数不丢失）
type BodyRecorder = Arc<Mutex<Vec<Value>>>;

fn mock_router(
    counter: Arc<AtomicU32>,
    recorder: Option<BodyRecorder>,
    auth_seen: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Router {
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
            post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                if let Some(f) = &auth_seen {
                    if headers.contains_key(axum::http::header::AUTHORIZATION) {
                        f.store(true, Ordering::SeqCst);
                    }
                }
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
    spawn_mock_full(port, counter, None, None).await;
}

async fn spawn_mock_with_recorder(
    port: u16,
    counter: Arc<AtomicU32>,
    recorder: Option<BodyRecorder>,
) {
    spawn_mock_full(port, counter, recorder, None).await;
}

async fn spawn_mock_full(
    port: u16,
    counter: Arc<AtomicU32>,
    recorder: Option<BodyRecorder>,
    auth_seen: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind mock port");
    let app = mock_router(counter, recorder, auth_seen);
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock server");
    });
}

async fn spawn_proxy(port: u16, mock_port: u16, retry_override: Option<u32>) -> Arc<AppState> {
    spawn_proxy_with_key(port, mock_port, retry_override, "").await
}

async fn spawn_proxy_with_key(
    port: u16,
    mock_port: u16,
    retry_override: Option<u32>,
    local_api_key: &str,
) -> Arc<AppState> {
    let mut cfg = AppConfig::default();
    cfg.server.port = port;
    cfg.server.local_api_key = local_api_key.into();
    cfg.providers = vec![ProviderConfig {
        id: "mock".into(),
        name: "Mock".into(),
        base_url: format!("http://127.0.0.1:{mock_port}/v1"),
        api_key: String::new(),
        timeout_secs: 30,
        enabled: true,
        models: vec!["mock-model".into()],
        ..ProviderConfig::demo()
    }];
    cfg.routes = vec![RouteConfig {
        model: "mock-model".into(),
        provider: "mock".into(),
        upstream_model: String::new(),
        overrides: serde_json::Map::new(),
        weight: 1,
        disabled: false,
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
    assert_eq!(
        mock_counter2.load(Ordering::SeqCst),
        3,
        "3 attempts expected"
    );
    let snap = state2.metrics.snapshot();
    assert_eq!(snap.retries, 2, "retry counter");
    assert_eq!(snap.total, 1);

    // 重试不丢参数：三次收到的请求体必须完全一致（含原始 messages/stream 与注入的 x_mock_fail_n）
    let received = recorder.lock().unwrap();
    assert_eq!(received.len(), 3, "mock should receive 3 attempts");
    assert_eq!(received[0], received[1], "retry must resend identical body");
    assert_eq!(received[1], received[2], "retry must resend identical body");
    assert_eq!(
        received[0]["messages"][0]["content"], "hi",
        "messages preserved"
    );
    assert_eq!(received[0]["stream"], json!(false), "stream flag preserved");
    assert_eq!(
        received[0]["x_mock_fail_n"],
        json!(2),
        "injected field preserved"
    );
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
        cfg.retry.circuit_breaker.open_duration_secs = 60;
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
    // 熔断器按 route 维度隔离，key = `model|provider|upstream_model`，已跳闸
    assert_eq!(
        state4
            .breakers
            .get(&lb::route_key("mock-model", "mock", ""))
            .expect("breaker exists")
            .status()
            .state,
        crate::proxy::circuit::BreakerState::Open
    );
    // 第 2 次请求：单候选已熔断 → Panic 模式窗口放行 1 次真实试探（打到上游
    // 得 500），其重试在窗口内被拒绝 → 最终以 CircuitOpen 503 结束
    let resp = client
        .post("http://127.0.0.1:8794/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(
        mock_counter4.load(Ordering::SeqCst),
        5,
        "panic window admits exactly 1 real request"
    );
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["type"], "circuit_open");

    // 熔断请求应在日志里留下结构化错误详情（含渠道、连续失败次数、冷却剩余）
    let snap = state4.metrics.snapshot();
    let failed_log = snap
        .recent
        .iter()
        .find(|l| l.error_detail.is_some())
        .expect("failed request should carry error_detail");
    let detail = failed_log.error_detail.as_ref().unwrap();
    assert_eq!(detail.kind, crate::proxy::retry::ErrorKind::CircuitOpen);
    assert!(
        detail.summary.contains("Mock"),
        "summary should name the provider: {}",
        detail.summary
    );
    assert!(detail.hint.is_some(), "熔断错误应附带排查建议");
    assert!(detail.consecutive_failures.unwrap_or(0) >= 4);
    assert!(
        detail.cooldown_remaining_secs.is_some(),
        "应报告熔断冷却剩余时间"
    );

    // 手动重置熔断后恢复
    state4
        .breakers
        .get(&lb::route_key("mock-model", "mock", ""))
        .expect("breaker exists")
        .reset();
    body["x_mock_fail_n"] = json!(0);
    body["x_mock_fail"] = Value::Null;
    let resp = client
        .post("http://127.0.0.1:8794/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "recovered after reset");

    // ---- 场景 F：本地访问令牌语义 ----
    // F1: 令牌为空 + 客户端传任意 API key → 放行 200，且 key 不透传到上游
    let mock_counter5 = Arc::new(AtomicU32::new(0));
    let auth_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_mock_full(9905, mock_counter5.clone(), None, Some(auth_seen.clone())).await;
    let _state5 = spawn_proxy_with_key(8795, 9905, None, "").await;
    let resp = client
        .post("http://127.0.0.1:8795/v1/chat/completions")
        .header("Authorization", "Bearer client-provided-key")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "empty local token must ignore client Authorization"
    );
    assert!(
        !auth_seen.load(Ordering::SeqCst),
        "client Authorization must NOT be forwarded to upstream"
    );

    // F2: 令牌非空 → 未带/错误 → 401；正确 → 200
    let mock_counter6 = Arc::new(AtomicU32::new(0));
    spawn_mock_full(9906, mock_counter6.clone(), None, None).await;
    let _state6 = spawn_proxy_with_key(8796, 9906, None, "secret").await;
    let resp = client
        .post("http://127.0.0.1:8796/v1/chat/completions")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "missing token must be rejected");
    let resp = client
        .post("http://127.0.0.1:8796/v1/chat/completions")
        .header("Authorization", "Bearer wrong")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "wrong token must be rejected");
    let resp = client
        .post("http://127.0.0.1:8796/v1/chat/completions")
        .header("Authorization", "Bearer secret")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "correct token must pass");
    assert_eq!(
        mock_counter6.load(Ordering::SeqCst),
        1,
        "only the authed request reaches upstream"
    );
}

/// 端到端验证请求头：客户端的身份类头原样到达上游，代理特征头不外泄。
///
/// 上游若只看请求头也应认为"这是客户端直连"：UA / 组织 / 官方 SDK 指纹头齐全，
/// 且没有 `x-forwarded-for`、`via` 之类自报代理身份的头。
#[tokio::test]
async fn smoke_forwards_client_identity_headers() {
    let seen: Arc<Mutex<Option<axum::http::HeaderMap>>> = Arc::new(Mutex::new(None));
    let recorder = seen.clone();
    // mock 原样回显请求体，便于代理走通非流式路径
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
            *recorder.lock().unwrap() = Some(headers);
            Json(body)
        }),
    );
    let listener = TcpListener::bind(("127.0.0.1", 9911))
        .await
        .expect("bind mock port");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("mock server") });
    let _state = spawn_proxy(8797, 9911, None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post("http://127.0.0.1:8797/v1/chat/completions")
        .header("user-agent", "OpenAI/NodeJS/4.68.4")
        .header("openai-organization", "org-42")
        .header("x-stainless-lang", "node")
        .header("accept", "application/json")
        .header("x-forwarded-for", "1.2.3.4")
        .header("via", "1.1 somewhere")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let first = seen.lock().unwrap().clone().expect("upstream must be hit");
    assert_eq!(first.get("user-agent").unwrap(), "OpenAI/NodeJS/4.68.4");
    assert_eq!(first.get("openai-organization").unwrap(), "org-42");
    assert_eq!(first.get("x-stainless-lang").unwrap(), "node");
    assert_eq!(first.get("accept").unwrap(), "application/json");
    assert!(first.get("x-forwarded-for").is_none(), "不得泄露代理特征头");
    assert!(first.get("via").is_none(), "不得泄露代理特征头");
    assert!(
        first.get("authorization").is_none(),
        "客户端的本地令牌不得外泄到上游"
    );

    // 客户端完全不带 UA 时，上游也应收到一个"普通客户端形态"的 UA，而不是空值。
    // 该 client 由 `Client::new()` 创建，本身不带 UA。
    *seen.lock().unwrap() = None;
    let resp = client
        .post("http://127.0.0.1:8797/v1/chat/completions")
        .json(&chat_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let second = seen.lock().unwrap().clone().expect("upstream must be hit");
    let ua = second
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(!ua.is_empty(), "上游必须能看到 UA，不能是空值");
}
