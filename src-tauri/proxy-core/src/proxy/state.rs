use crate::proxy::circuit::CircuitBreaker;
use crate::proxy::config::AppConfig;
use crate::proxy::db::Database;
use chrono::Utc;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// 一条已完成的请求记录（推给前端展示）
#[derive(Debug, Clone, Serialize)]
pub struct RequestLog {
    pub id: String,
    pub ts: i64,
    pub method: String,
    pub path: String,
    pub model: String,
    pub provider: Option<String>,
    /// Provider id（内部使用：流中断时回写熔断器；不推送给前端）
    #[serde(skip)]
    pub provider_id: Option<String>,
    pub status: u16,
    pub retries: u32,
    pub latency_ms: u64,
    pub stream: bool,
    pub error: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// 响应体大小（字节）
    #[serde(default)]
    pub response_bytes: Option<u64>,
    /// 首字节时间（毫秒）；流式为收到第一个 chunk 的时间，非流式为总延迟
    #[serde(default)]
    pub ttfb_ms: Option<u64>,
    /// 是否命中缓存
    #[serde(default)]
    pub cache_hit: bool,
    /// 缓存命中的 token 数量（上游返回的 cached_tokens / cache_read_input_tokens 等）
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    /// 实际转发给上游的模型名（经路由 upstream_model 改写后）；与 model 不同时才有值
    #[serde(default)]
    pub upstream_model: Option<String>,
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
            provider_id: None,
            status: 0,
            retries: 0,
            latency_ms: 0,
            stream: false,
            error: None,
            tokens_in: None,
            tokens_out: None,
            response_bytes: None,
            ttfb_ms: None,
            cache_hit: false,
            cached_tokens: None,
            upstream_model: None,
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
    pub cache_hits: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
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
    cache_hits: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
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
    pub fn inc_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add_bytes(&self, input: u64, output: u64) {
        self.bytes_in.fetch_add(input, Ordering::Relaxed);
        self.bytes_out.fetch_add(output, Ordering::Relaxed);
    }
    pub fn record(&self, log: RequestLog) {
        let mut q = self.logs.lock();
        // 按 ID 去重：如果已存在则更新，否则插入
        if let Some(existing) = q.iter_mut().find(|l| l.id == log.id) {
            *existing = log;
        } else {
            if q.len() >= LOG_BUFFER {
                q.pop_front();
            }
            q.push_back(log);
        }
    }

    /// 按 ID 查找日志（用于流式结束后回写后获取最新副本）
    pub fn get_log(&self, id: &str) -> Option<RequestLog> {
        let q = self.logs.lock();
        q.iter().find(|l| l.id == id).cloned()
    }

    /// 更新日志条目并推送事件（用于流式结束后的局部更新）
    pub fn update_and_emit(&self, id: &str, sink: Option<&dyn EventSink>) {
        let log = {
            let q = self.logs.lock();
            q.iter().find(|l| l.id == id).cloned()
        };
        if let Some(log) = log {
            if let Some(s) = sink {
                s.emit_log(&log);
            }
        }
    }

    /// 流式响应结束后回写 response_bytes / tokens_out
    pub fn update_log_stream_stats(&self, id: &str, response_bytes: u64, tokens_out: Option<u64>) {
        let mut q = self.logs.lock();
        for log in q.iter_mut() {
            if log.id == id {
                log.response_bytes = Some(response_bytes);
                if let Some(t) = tokens_out {
                    log.tokens_out = Some(t);
                }
                break;
            }
        }
    }

    /// 流式响应结束后回写缓存命中状态（命中时累加计数器）
    pub fn update_log_cache_hit(&self, id: &str, cache_hit: bool, cached_tokens: Option<u64>) {
        if cache_hit {
            self.inc_cache_hit();
        }
        let mut q = self.logs.lock();
        for log in q.iter_mut() {
            if log.id == id {
                log.cache_hit = cache_hit;
                log.cached_tokens = cached_tokens;
                break;
            }
        }
    }
    pub fn snapshot(&self) -> MetricsSnapshot {
        let total = self.total.load(Ordering::Relaxed);
        let success = self.success.load(Ordering::Relaxed);
        let failed = self.failed.load(Ordering::Relaxed);
        let retries = self.retries.load(Ordering::Relaxed);
        let tokens_in = self.tokens_in.load(Ordering::Relaxed);
        let tokens_out = self.tokens_out.load(Ordering::Relaxed);
        let latency_sum = self.latency_sum_ms.load(Ordering::Relaxed);
        let cache_hits = self.cache_hits.load(Ordering::Relaxed);
        let bytes_in = self.bytes_in.load(Ordering::Relaxed);
        let bytes_out = self.bytes_out.load(Ordering::Relaxed);
        let q = self.logs.lock();
        MetricsSnapshot {
            total,
            success,
            failed,
            retries,
            tokens_in,
            tokens_out,
            avg_latency_ms: if total > 0 { latency_sum / total } else { 0 },
            cache_hits,
            bytes_in,
            bytes_out,
            recent: q.iter().rev().take(100).cloned().collect(),
        }
    }
}

/// 事件推送接口：由桌面壳（Tauri）实现，core 不依赖 GUI 框架
pub trait EventSink: Send + Sync {
    fn emit_log(&self, log: &RequestLog);
    fn emit_config_changed(&self);
}

// ─────────────────────────── 负载均衡运行时统计 ───────────────────────────

/// 单条路由的延迟统计（EWMA：指数加权移动平均）。
///
/// EWMA 公式：`avg = alpha * new + (1 - alpha) * old`，其中 `alpha = 0.3`。
/// 相比简单平均，EWMA 对近期延迟更敏感，能更快反映上游性能变化，
/// 供 `FastestResponse` 策略使用。
pub struct RouteLatencyStats {
    /// 指数加权移动平均延迟（毫秒）
    ewma_ms: AtomicU64,
    /// 样本数
    samples: AtomicU64,
}

impl RouteLatencyStats {
    pub fn new() -> Self {
        Self {
            ewma_ms: AtomicU64::new(0),
            samples: AtomicU64::new(0),
        }
    }

    /// 记录一次延迟样本，更新 EWMA。alpha = 0.3（新样本权重 30%）
    pub fn record_latency(&self, latency_ms: u64) {
        let prev = self.ewma_ms.load(Ordering::Relaxed);
        let new = if prev == 0 {
            latency_ms
        } else {
            // EWMA = alpha * new + (1 - alpha) * old
            ((latency_ms as f64 * 0.3) + (prev as f64 * 0.7)) as u64
        };
        self.ewma_ms.store(new, Ordering::Relaxed);
        self.samples.fetch_add(1, Ordering::Relaxed);
    }

    /// 当前 EWMA 延迟（毫秒）；0 表示尚无数据
    pub fn avg_ms(&self) -> u64 {
        self.ewma_ms.load(Ordering::Relaxed)
    }

    /// 已采集样本数
    pub fn sample_count(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }
}

impl Default for RouteLatencyStats {
    fn default() -> Self {
        Self::new()
    }
}

/// 会话粘性映射条目。
///
/// 记录某会话上次选中的候选索引，用于重试/多轮对话时路由到同一上游。
#[derive(Debug, Clone)]
pub struct StickyEntry {
    /// 选中的候选索引
    pub candidate_index: usize,
    /// 路由标识（model|provider_id|upstream_model）用于校验配置未变
    pub route_key: String,
    /// 创建时间
    pub created_at: Instant,
}

/// 全局共享状态。
///
/// 在各请求处理函数间共享，包含配置、指标、熔断器、HTTP 客户端
/// 以及负载均衡运行时统计（延迟/计数/粘性/在途）。
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
    /// SQLite 数据存储（可选；测试时可为 None）
    pub db: Mutex<Option<Arc<Database>>>,
    // ── 负载均衡运行时统计 ──
    /// 路由延迟统计：key = `{model}|{provider_id}|{upstream_model}`
    pub route_latency: DashMap<String, RouteLatencyStats>,
    /// 轮询计数器：key = `{model}` → AtomicU64
    pub lb_counters: DashMap<String, AtomicU64>,
    /// 会话粘性映射：key = session_key → StickyEntry
    pub sticky_sessions: DashMap<String, StickyEntry>,
    /// 在途请求计数：key = `{model}|{provider_id}|{upstream_model}`
    pub in_flight: DashMap<String, AtomicU64>,
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
            db: Mutex::new(None),
            route_latency: DashMap::new(),
            lb_counters: DashMap::new(),
            sticky_sessions: DashMap::new(),
            in_flight: DashMap::new(),
        }
    }

    /// 推送日志事件（有 sink 时）
    pub fn emit_log(&self, log: &RequestLog) {
        if let Some(s) = self.sink.lock().as_ref() {
            s.emit_log(log);
        }
    }

    /// 按 ID 查找日志并推送更新事件（用于流式结束后的局部刷新）
    pub fn emit_log_update(&self, id: &str) {
        if let Some(log) = self.metrics.get_log(id) {
            self.emit_log(&log);
        }
    }
}
