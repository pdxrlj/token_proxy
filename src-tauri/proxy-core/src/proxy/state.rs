use crate::proxy::circuit::CircuitBreaker;
use crate::proxy::config::AppConfig;
use chrono::Utc;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// 一条已完成的请求记录（推给前端展示）
#[derive(Debug, Clone, Serialize)]
pub struct RequestLog {
    pub id: String,
    pub ts: i64,
    pub method: String,
    pub path: String,
    pub model: String,
    pub provider: Option<String>,
    pub status: u16,
    pub retries: u32,
    pub latency_ms: u64,
    pub stream: bool,
    pub error: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

impl RequestLog {
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ts: Utc::now().timestamp_millis(),
            method: String::new(),
            path: String::new(),
            model: String::new(),
            provider: None,
            status: 0,
            retries: 0,
            latency_ms: 0,
            stream: false,
            error: None,
            tokens_in: None,
            tokens_out: None,
        }
    }
}

/// 指标快照（前端轮询）
#[derive(Debug, Clone, Serialize, Default)]
pub struct MetricsSnapshot {
    pub total: u64,
    pub success: u64,
    pub failed: u64,
    pub retries: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub avg_latency_ms: u64,
    pub recent: Vec<RequestLog>,
}

#[derive(Default)]
pub struct Metrics {
    total: AtomicU64,
    success: AtomicU64,
    failed: AtomicU64,
    retries: AtomicU64,
    tokens_in: AtomicU64,
    tokens_out: AtomicU64,
    latency_sum_ms: AtomicU64,
    logs: Mutex<VecDeque<RequestLog>>,
}

const LOG_BUFFER: usize = 500;

impl Metrics {
    pub fn inc_total(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_success(&self) {
        self.success.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_retries(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add_tokens(&self, input: u64, output: u64) {
        self.tokens_in.fetch_add(input, Ordering::Relaxed);
        self.tokens_out.fetch_add(output, Ordering::Relaxed);
    }
    pub fn add_latency(&self, ms: u64) {
        self.latency_sum_ms.fetch_add(ms, Ordering::Relaxed);
    }
    pub fn record(&self, log: RequestLog) {
        let mut q = self.logs.lock();
        if q.len() >= LOG_BUFFER {
            q.pop_front();
        }
        q.push_back(log);
    }
    pub fn snapshot(&self) -> MetricsSnapshot {
        let total = self.total.load(Ordering::Relaxed);
        let success = self.success.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        let retries = self.retries.load(Ordering::Relaxed);
        let tokens_in = self.tokens_in.load(Ordering::Relaxed);
        let tokens_out = self.tokens_out.load(Ordering::Relaxed);
        let latency_sum = self.latency_sum_ms.load(Ordering::Relaxed);
        let q = self.logs.lock();
        MetricsSnapshot {
            total,
            success,
            failed,
            retries,
            tokens_in,
            tokens_out,
            avg_latency_ms: if total > 0 { latency_sum / total } else { 0 },
            recent: q.iter().rev().take(100).cloned().collect(),
        }
    }
}

/// 事件推送接口：由桌面壳（Tauri）实现，core 不依赖 GUI 框架
pub trait EventSink: Send + Sync {
    fn emit_log(&self, log: &RequestLog);
    fn emit_config_changed(&self);
}

/// 全局共享状态
pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub config_path: PathBuf,
    pub metrics: Arc<Metrics>,
    /// 每个 Provider 一个熔断器；Arc 共享实例，避免 clone 深拷贝导致状态不同步
    pub breakers: Arc<DashMap<String, Arc<CircuitBreaker>>>,
    pub http: reqwest::Client,
    pub started_at: std::time::Instant,
    /// 事件推送器，桌面运行时注入；纯测试时为空
    pub sink: Mutex<Option<Arc<dyn EventSink>>>,
}

impl AppState {
    pub fn new(config: AppConfig, config_path: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("failed to build http client");
        Self {
            config: Arc::new(RwLock::new(config)),
            config_path,
            metrics: Arc::new(Metrics::default()),
            breakers: Arc::new(DashMap::new()),
            http,
            started_at: std::time::Instant::now(),
            sink: Mutex::new(None),
        }
    }

    /// 推送日志事件（有 sink 时）
    pub fn emit_log(&self, log: &RequestLog) {
        if let Some(s) = self.sink.lock().as_ref() {
            s.emit_log(log);
        }
    }
}
