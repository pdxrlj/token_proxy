// 独立日志代理：读取真实用户配置，打印每个请求的完整日志。
// 用于排查客户端（如 VS Code Copilot）请求失败时请求是否到达代理、发了什么。
// 用法:
//   cargo build --example standalone -p token-proxy-core
//   PORT=8787 RUST_LOG=info ./target/debug/examples/standalone.exe > proxy.log 2>&1
use std::sync::Arc;

use token_proxy_core::proxy::api;
use token_proxy_core::proxy::state::AppState;
use token_proxy_core::proxy::store::load_config;

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .try_init();

    let path = token_proxy_core::proxy::store::config_path();
    let cfg = load_config(&path).expect("failed to load config");
    tracing::info!("config loaded from {}", path.display());

    let state = Arc::new(AppState::new(cfg, path));

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| state.config.read().server.port);

    let app = api::router(state)
        .layer(tower_http::trace::TraceLayer::new_for_http());
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("bind failed — 端口被占用？请先关闭 Token Proxy 应用");
    tracing::info!("standalone proxy listening on http://{addr}");

    axum::serve(listener, app).await.expect("server error");
}
