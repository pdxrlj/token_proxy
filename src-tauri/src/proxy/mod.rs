pub mod api;
pub mod circuit;
pub mod config;
pub mod retry;
pub mod state;
pub mod store;
pub mod upstream;

#[cfg(test)]
mod smoke_tests;

use state::AppState;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

/// 启动代理 HTTP 服务（阻塞直至服务器退出）
pub async fn serve(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (host, port) = {
        let cfg = state.config.read();
        (cfg.server.host.clone(), cfg.server.port)
    };
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    info!("proxy listening on http://{addr}");
    let app = api::router(state);
    axum::serve(listener, app).await?;
    Ok(())
}
