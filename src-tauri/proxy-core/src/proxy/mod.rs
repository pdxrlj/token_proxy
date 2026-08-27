//! OpenAI 兼容的 LLM 代理核心。
//!
//! # 模块结构
//! - [`config`]  — 配置类型与路由解析
//! - [`api`]      — HTTP 处理器与请求路由
//! - [`lb`]       — 负载均衡选择器（多上游 + 粘性会话）
//! - [`upstream`] — 上游调用与 SSE 流转发
//! - [`retry`]    — 重试退避与错误类型
//! - [`circuit`]  — 熔断器
//! - [`state`]    — 全局共享状态与指标
//! - [`store`]    — 配置持久化与校验
//! - [`db`]       — SQLite 请求日志存储

pub mod api;
pub mod circuit;
pub mod config;
pub mod db;
pub mod lb;
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

/// 启动代理 HTTP 服务（阻塞直至服务器退出）。
///
/// 同时启动 sticky 会话的定时清理后台任务。
pub async fn serve(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (host, port, sticky_ttl) = {
        let cfg = state.config.read();
        (
            cfg.server.host.clone(),
            cfg.server.port,
            cfg.load_balance.sticky.ttl_secs,
        )
    };
    // 启动 sticky 会话清理后台任务
    lb::spawn_sticky_cleanup(state.clone(), sticky_ttl);
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    info!("proxy listening on http://{addr}");
    let app = api::router(state);
    axum::serve(listener, app).await?;
    Ok(())
}
