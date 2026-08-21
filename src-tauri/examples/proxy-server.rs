// 独立启动代理服务器（无 GUI），用于冒烟测试：
// cargo run --example proxy-server --manifest-path src-tauri/Cargo.toml
// （在 proxy-core crate 内测试请用：cargo test -p token-proxy-core -- smoke_full_chain）
use std::path::PathBuf;
use std::sync::Arc;
use token_proxy_core::proxy::config::{AppConfig, ProviderConfig, RouteConfig};
use token_proxy_core::proxy::state::AppState;

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    let mut cfg = AppConfig::default();
    cfg.server.port = 8787;
    cfg.providers = vec![ProviderConfig {
        id: "mock".into(),
        name: "Mock Upstream".into(),
        base_url: "http://127.0.0.1:9999/v1".into(),
        api_key: String::new(),
        timeout_secs: 30,
        enabled: true,
        models: vec!["mock-model".into(), "gpt-4o".into()],
    }];
    cfg.routes = vec![RouteConfig {
        model: "gpt-4o".into(),
        provider: "mock".into(),
        overrides: serde_json::Map::new(),
    }];

    let state = Arc::new(AppState::new(cfg, PathBuf::from("smoke-config.json")));
    token_proxy_core::proxy::serve(state)
        .await
        .expect("proxy server exited");
}
