use crate::proxy::circuit::CircuitBreaker;
use crate::proxy::config::AppConfig;
use crate::proxy::db::Database;
use crate::proxy::retry::ErrorDetail;
use crate::proxy::ServerSupervisor;
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
    /// 错误的简短文本（兼容旧版展示，同时用于列表页快速浏览）
    pub error: Option<String>,
    /// 结构化错误详情（详情弹窗中分块展示；成功请求为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<ErrorDetail>,
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
            error_detail: None,
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

    /// 流式响应结束后回写 response_bytes / tokens_out / tokens_in
    pub fn update_log_stream_stats(
        &self,
        id: &str,
        response_bytes: u64,
        tokens_out: Option<u64>,
        tokens_in: Option<u64>,
    ) {
        let mut q = self.logs.lock();
        for log in q.iter_mut() {
            if log.id == id {
                log.response_bytes = Some(response_bytes);
                if let Some(t) = tokens_out {
                    log.tokens_out = Some(t);
                }
                if let Some(t) = tokens_in {
                    log.tokens_in = Some(t);
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
    /// 最近一次采样时刻（秒级 epoch）；0 表示从未采样
    last_sample_at: AtomicU64,
}

/// 样本有效期（秒）。超过该时长未刷新的 EWMA 视为陈旧，
/// 不再作为 `FastestResponse` 的决策依据（避免陈旧数据长期主导选择，
/// 同时让新加入或刚恢复的上游有机会获得流量完成冷启动）。
pub const LATENCY_SAMPLE_TTL_SECS: u64 = 300;

impl RouteLatencyStats {
    pub fn new() -> Self {
        Self {
            ewma_ms: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            last_sample_at: AtomicU64::new(0),
        }
    }

    /// 当前秒级 epoch 时间戳。
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
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
        self.last_sample_at
            .store(Self::now_secs(), Ordering::Relaxed);
    }

    /// 当前 EWMA 延迟（毫秒）；0 表示尚无数据
    pub fn avg_ms(&self) -> u64 {
        self.ewma_ms.load(Ordering::Relaxed)
    }

    /// 已采集样本数
    pub fn sample_count(&self) -> u64 {
        self.samples.load(Ordering::Relaxed)
    }

    /// 样本是否新鲜（有样本且未超过 TTL）。
    ///
    /// 陈旧的 EWMA 不应参与 `FastestResponse` 比较：它既不反映上游
    /// 当前状态，又会阻塞新上游的冷启动。
    pub fn is_fresh(&self) -> bool {
        let last = self.last_sample_at.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        Self::now_secs().saturating_sub(last) < LATENCY_SAMPLE_TTL_SECS
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
    /// 代理服务生命周期管理器（监听地址变更时重启监听）
    pub server: Arc<ServerSupervisor>,
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
    /// SWRR 当前权重：key = `{model}|{provider_id}|{upstream_model}` → current_weight。
    ///
    /// 平滑加权轮询的运行状态（Nginx 算法）：每轮 `cw[i] += w[i]`，选出最大者后
    /// `cw[winner] -= Σw`。相比朴素计数器取模，分配序列均匀无突发、权重感知、
    /// 候选集变化时自然过渡。
    ///
    /// 整轮「累加 → 取最大 → 减 total」必须在同一把锁内完成：若拆成逐键原子
    /// 操作，并发请求交错会打破 SWRR 的序列不变量。单轮只涉及几个候选键，
    /// 锁竞争开销可忽略。过期条目由 sticky 清理任务回收。
    pub swrr_weights: parking_lot::Mutex<std::collections::HashMap<String, i64>>,
    /// 会话粘性映射：key = session_key → StickyEntry
    pub sticky_sessions: DashMap<String, StickyEntry>,
    /// 在途请求计数：key = `{model}|{provider_id}|{upstream_model}`
    pub in_flight: DashMap<String, AtomicU64>,
    /// 用户已用请求数：key = UserConfig.id
    ///
    /// 单独存放而非写入配置文件，是因为 GUI 保存配置时提交的是此前获取的快照，
    /// 若计数存在配置里会被旧值覆盖，导致配额限制被绕过。
    /// 代价是计数不随重启保留，可通过界面手动重置。
    pub user_usage: DashMap<String, AtomicU64>,
}

impl AppState {
    pub fn new(config: AppConfig, config_path: PathBuf) -> Self {
        // 默认头给出"普通客户端"的底色：带 UA、带 accept。
        // 具体请求若透传到了客户端自己的头，会在请求级覆盖这里的默认值。
        // 同时由 Cargo feature 打开 gzip/brotli/deflate 与 http2，
        // 使 accept-encoding 与协议版本也贴近常规客户端。
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .default_headers(crate::proxy::headers::client_default_headers())
            .build()
            .expect("failed to build http client");
        Self {
            config: Arc::new(RwLock::new(config)),
            config_path,
            server: Arc::new(ServerSupervisor::new()),
            metrics: Arc::new(Metrics::default()),
            breakers: Arc::new(DashMap::new()),
            http,
            started_at: std::time::Instant::now(),
            sink: Mutex::new(None),
            db: Mutex::new(None),
            route_latency: DashMap::new(),
            swrr_weights: parking_lot::Mutex::new(std::collections::HashMap::new()),
            sticky_sessions: DashMap::new(),
            in_flight: DashMap::new(),
            user_usage: DashMap::new(),
        }
    }

    /// 读取某个用户的已用请求数
    pub fn user_used(&self, id: &str) -> u64 {
        self.user_usage
            .get(id)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// 重置某个用户的已用请求数（传 None 表示重置全部）
    pub fn reset_user_usage(&self, id: Option<&str>) {
        match id {
            Some(id) => {
                self.user_usage.remove(id);
            }
            None => self.user_usage.clear(),
        }
    }

    /// 尝试为某个用户消耗一次配额。
    ///
    /// - `max` 为 `None`：不限次，直接累加并返回 `true`
    /// - `max` 为 `Some(n)`：仅当已用数 < n 时原子地 +1 并返回 `true`；
    ///   已达上限返回 `false` 且不消耗。
    ///
    /// 用 `fetch_update` 做原子「检查-自增」，避免并发请求同时通过校验而超额。
    pub fn consume_user_quota(&self, id: &str, max: Option<u64>) -> bool {
        let counter = self
            .user_usage
            .entry(id.to_string())
            .or_insert_with(|| AtomicU64::new(0));

        match max {
            None => {
                counter.fetch_add(1, Ordering::SeqCst);
                true
            }
            Some(limit) => counter
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                    if v >= limit {
                        None
                    } else {
                        Some(v + 1)
                    }
                })
                .is_ok(),
        }
    }

    /// 配置中的监听地址（`host:port`）
    pub fn configured_addr(&self) -> String {
        let cfg = self.config.read();
        format!("{}:{}", cfg.server.host, cfg.server.port)
    }

    /// 便捷构造：仅用于测试与独立运行（不启动 HTTP 服务）
    pub fn with_config(config: AppConfig) -> Self {
        Self::new(config, PathBuf::from("config.json"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::UserConfig;

    fn state_with_users(users: Vec<UserConfig>) -> AppState {
        AppState::with_config(AppConfig {
            users,
            ..AppConfig::default()
        })
    }

    #[test]
    fn unlimited_user_never_runs_out() {
        let s = state_with_users(vec![UserConfig {
            id: "u1".into(),
            key: "secret".into(),
            max_requests: None,
            ..Default::default()
        }]);
        for _ in 0..1000 {
            assert!(s.consume_user_quota("u1", None));
        }
        assert_eq!(s.user_used("u1"), 1000);
    }

    #[test]
    fn quota_stops_exactly_at_limit() {
        let s = state_with_users(vec![]);
        // 上限 3：第 1~3 次成功，第 4 次起拒绝
        assert!(s.consume_user_quota("u", Some(3)));
        assert!(s.consume_user_quota("u", Some(3)));
        assert!(s.consume_user_quota("u", Some(3)));
        assert!(!s.consume_user_quota("u", Some(3)), "超出上限应被拒绝");
        assert!(!s.consume_user_quota("u", Some(3)), "重复请求仍应被拒绝");
        // 达到上限时计数不应继续增长
        assert_eq!(s.user_used("u"), 3);
    }

    #[test]
    fn raising_limit_releases_quota() {
        let s = state_with_users(vec![]);
        let _ = s.consume_user_quota("u", Some(1));
        assert!(!s.consume_user_quota("u", Some(1)));
        // 调高上限后应恢复可用（对应界面上调 max_requests 的场景）
        assert!(s.consume_user_quota("u", Some(2)));
        assert_eq!(s.user_used("u"), 2);
    }

    #[test]
    fn reset_usage_clears_counter() {
        let s = state_with_users(vec![]);
        let _ = s.consume_user_quota("a", Some(1));
        let _ = s.consume_user_quota("b", Some(1));

        // 重置单个
        s.reset_user_usage(Some("a"));
        assert_eq!(s.user_used("a"), 0);
        assert_eq!(s.user_used("b"), 1, "不应影响其它用户");
        assert!(s.consume_user_quota("a", Some(1)), "重置后应可再次使用");

        // 重置全部
        s.reset_user_usage(None);
        assert_eq!(s.user_used("a"), 0);
        assert_eq!(s.user_used("b"), 0);
    }

    #[test]
    fn quota_is_per_user() {
        let s = state_with_users(vec![]);
        let _ = s.consume_user_quota("a", Some(1));
        assert!(!s.consume_user_quota("a", Some(1)));
        assert!(s.consume_user_quota("b", Some(1)), "不同用户的配额相互独立");
    }

    #[test]
    fn concurrent_consumption_never_exceeds_limit() {
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
        use std::sync::Arc as StdArc;
        use std::thread;

        let s = StdArc::new(state_with_users(vec![]));
        let limit = 50u64;
        let granted = StdArc::new(AtomicU64::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = s.clone();
                let granted = granted.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        if s.consume_user_quota("u", Some(limit)) {
                            granted.fetch_add(1, AtomicOrdering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // 并发下批准的总数必须恰好等于上限，不能超额
        assert_eq!(granted.load(AtomicOrdering::SeqCst), limit);
        assert_eq!(s.user_used("u"), limit);
    }
}
