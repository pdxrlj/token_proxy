//! OpenAI 兼容的 LLM 代理核心。
//!
//! # 模块结构
//! - [`config`]  — 配置类型与路由解析
//! - [`api`]      — HTTP 处理器与请求路由
//! - [`lb`]       — 负载均衡选择器（SWRR / 慢启动 / 粘性会话）
//! - [`upstream`] — 上游调用与 SSE 流转发
//! - [`retry`]    — 重试退避与错误类型
//! - [`circuit`]  — 熔断器（503 立即跳闸 / 半开恢复 / Panic 模式）
//! - [`probe`]    — 主动探活任务（熔断到期后自动试探上游）
//! - [`state`]    — 全局共享状态与指标
//! - [`store`]    — 配置持久化与校验
//! - [`db`]       — SQLite 请求日志存储

pub mod api;
pub mod circuit;
pub mod config;
pub mod db;
pub mod headers;
pub mod lb;
pub mod probe;
pub mod retry;
pub mod state;
pub mod store;
pub mod upstream;

#[cfg(test)]
mod smoke_tests;

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use state::AppState;

/// 启动代理 HTTP 服务（阻塞直至服务器退出）。
///
/// 同时启动 sticky 会话的定时清理后台任务。
pub async fn serve(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (host, port) = {
        let cfg = state.config.read();
        (cfg.server.host.clone(), cfg.server.port)
    };
    // 启动 sticky 会话清理与熔断探活后台任务（TTL / 探活间隔动态读取，随配置热重载生效）
    let cleanup = lb::spawn_sticky_cleanup(state.clone());
    let probe = probe::spawn_probe_task(state.clone());
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    info!("proxy listening on http://{addr}");
    let app = api::router(state);
    let result = axum::serve(listener, app).await;
    // 服务退出时终止后台任务，避免同一进程内重启服务后残留多个任务
    cleanup.abort();
    probe.abort();
    result?;
    Ok(())
}

/// 启动代理 HTTP 服务，并返回用于主动停止它的句柄。
///
/// 相比 [`serve`]，返回的 [`RunningServer`] 允许调用方停止监听，
/// 从而在监听地址（host/port）配置变更后重新绑定端口。
pub async fn serve_cancellable(
    state: Arc<AppState>,
    cancel: CancellationToken,
) -> Result<RunningServer, Box<dyn std::error::Error + Send + Sync>> {
    let (host, port) = {
        let cfg = state.config.read();
        (cfg.server.host.clone(), cfg.server.port)
    };
    // 启动 sticky 会话清理与熔断探活后台任务（TTL / 探活间隔每轮从配置动态读取）
    let cleanup = lb::spawn_sticky_cleanup(state.clone());
    let probe = probe::spawn_probe_task(state.clone());

    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr).await?;
    let bound_addr = listener.local_addr()?;
    info!("proxy listening on http://{addr}");

    let app = api::router(state);
    let (ready_tx, ready_rx) = oneshot::channel();
    let task_cancel = cancel.child_token();
    let shutdown_cancel = task_cancel.clone();
    let handle = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async move {
            shutdown_cancel.cancelled().await;
        });
        // 监听器已进入 accept 循环，通知等待方
        let _ = ready_tx.send(());
        if let Err(e) = server.await {
            error!("proxy server error: {e}");
        }
        // 服务退出后终止后台任务，避免反复重启导致任务累积
        cleanup.abort();
        probe.abort();
    });

    Ok(RunningServer {
        addr,
        bound_addr,
        cancel: task_cancel,
        ready: Some(ready_rx),
        handle: Some(handle),
    })
}

/// 一个运行中的代理服务句柄。
pub struct RunningServer {
    /// 配置中的监听地址（`host:port`）
    pub addr: String,
    /// 操作系统实际绑定的地址（端口为 0 时为内核分配的真实端口）
    pub bound_addr: SocketAddr,
    /// 取消令牌：触发后服务优雅退出
    cancel: CancellationToken,
    /// 就绪信号：监听器开始 accept 后触发
    ready: Option<oneshot::Receiver<()>>,
    handle: Option<JoinHandle<()>>,
}

impl RunningServer {
    /// 等待监听器进入 accept 状态，避免重启后短暂的连接拒绝窗口。
    pub async fn wait_ready(&mut self) {
        if let Some(rx) = self.ready.take() {
            // 发送端不会提前 drop（在 spawned 任务首行即 send），
            // 这里要么收到 Ok(())，要么任务已结束返回 Err，两种都视为已就绪。
            let _ = rx.await;
        }
    }

    /// 停止服务并等待其完全退出。
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

/// 代理服务生命周期管理器。
///
/// 监听地址（`server.host` / `server.port`）变更时，自动停止旧监听并按新地址重启。
/// 其它配置（重试、路由、负载均衡等）走热重载，不需要重启监听。
pub struct ServerSupervisor {
    /// 当前运行的服务（启动失败时为 `None`）
    server: Mutex<Option<RunningServer>>,
    /// 当前生效的监听地址
    addr: Mutex<Option<String>>,
    /// 记录上一次启动的失败原因，供状态查询展示
    last_error: Mutex<Option<String>>,
}

impl ServerSupervisor {
    pub fn new() -> Self {
        Self {
            server: Mutex::new(None),
            addr: Mutex::new(None),
            last_error: Mutex::new(None),
        }
    }

    /// 当前生效的监听地址（未成功启动时为 `None`）
    pub async fn current_addr(&self) -> Option<String> {
        self.addr.lock().await.clone()
    }

    /// 当前绑定的 socket 地址
    pub async fn bound_addr(&self) -> Option<SocketAddr> {
        self.server.lock().await.as_ref().map(|s| s.bound_addr)
    }

    /// 服务是否正在监听
    pub async fn is_running(&self) -> bool {
        self.server.lock().await.is_some()
    }

    /// 上一次启动失败的原因
    pub async fn last_error(&self) -> Option<String> {
        self.last_error.lock().await.clone()
    }

    /// 按当前配置启动服务；已在运行时先停止旧的。
    pub async fn start(&self, state: Arc<AppState>) -> Result<(), String> {
        self.stop().await;

        let cancel = CancellationToken::new();
        let mut server = match serve_cancellable(state, cancel).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("启动代理服务失败：{e}");
                *self.last_error.lock().await = Some(msg.clone());
                *self.addr.lock().await = None;
                error!("{msg}");
                return Err(msg);
            }
        };
        // 等待监听器真正就绪，避免调用方返回后立刻请求被拒绝
        server.wait_ready().await;

        *self.addr.lock().await = Some(server.addr.clone());
        *self.last_error.lock().await = None;
        *self.server.lock().await = Some(server);
        Ok(())
    }

    /// 停止当前服务（若在运行）。
    pub async fn stop(&self) {
        if let Some(s) = self.server.lock().await.take() {
            info!("stopping proxy server on {}", s.addr);
            s.shutdown().await;
        }
    }

    /// 若监听地址与当前生效地址不同，则按新地址重启。
    ///
    /// 返回是否实际执行了重启；地址未变化（或本就未运行且启动成功）时返回 `false`。
    pub async fn restart_if_addr_changed(
        &self,
        state: Arc<AppState>,
        new_addr: String,
    ) -> Result<bool, String> {
        if self.current_addr().await.as_deref() == Some(new_addr.as_str())
            && self.is_running().await
        {
            return Ok(false);
        }
        info!("listen address changed to {new_addr}, restarting proxy server");
        self.start(state).await?;
        Ok(true)
    }
}

impl Default for ServerSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷入口：配置写入后调用，按需重启监听。
pub async fn apply_server_config(state: &Arc<AppState>) -> Result<bool, String> {
    let addr = {
        let cfg = state.config.read();
        format!("{}:{}", cfg.server.host, cfg.server.port)
    };
    state
        .server
        .restart_if_addr_changed(state.clone(), addr)
        .await
}
