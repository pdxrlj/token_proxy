//! 负载均衡选择器：从候选路由中按策略选一个。
//!
//! 支持会话粘性（sticky session），确保同一会话的重试/多轮对话请求
//! 始终路由到同一上游，避免上下文丢失或缓存失效。
//!
//! # 模块结构
//! - [`select`]         — 主入口：粘性优先 → 策略选择 → 记录粘性
//! - 策略函数           — `round_robin` / `random_index` / `weighted_random` / `fastest_response` / `least_connections`
//! - 会话 Key 提取      — [`extract_session_key`] 从请求中提取会话标识
//! - 运行时统计辅助     — `inc_in_flight` / `dec_in_flight` / `record_latency`
//! - 后台清理           — [`spawn_sticky_cleanup`] 定时回收过期粘性条目

use crate::proxy::config::{Candidate, LoadBalanceConfig, SessionKeySource};
use crate::proxy::state::{AppState, StickyEntry};
use axum::http::HeaderMap;
use rand::Rng;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::time::Instant;
use tracing::debug;

// ─────────────────────────── 路由标识 ───────────────────────────

/// 生成路由唯一标识，用于延迟统计和粘性校验。
///
/// 格式：`{model}|{provider_id}|{upstream_model}`
pub fn route_key(model: &str, provider_id: &str, upstream_model: &str) -> String {
    format!("{model}|{provider_id}|{upstream_model}")
}

/// 从候选路由中提取 `route_key`（upstream_model 为空时取空串）。
fn route_key_for_candidate(model: &str, candidate: &Candidate<'_>) -> String {
    let upstream_model = candidate
        .route
        .map(|r| r.upstream_model.as_str())
        .unwrap_or("");
    route_key(model, &candidate.provider.id, upstream_model)
}

// ─────────────────────────── 主选择入口 ───────────────────────────

/// 从候选列表中选择一个索引。
///
/// # 选择顺序
/// 1. **粘性优先**：若启用 sticky 且有 session_key，先查粘性映射命中
/// 2. **策略选择**：未命中则按 `lb_cfg.strategy` 选择
/// 3. **记录粘性**：选中后若有 session_key 则写入粘性映射
///
/// # 返回
/// `Some(index)` 表示选中候选在列表中的索引；`None` 表示候选列表为空。
pub fn select(
    state: &AppState,
    candidates: &[Candidate<'_>],
    model: &str,
    session_key: Option<&str>,
    lb_cfg: &LoadBalanceConfig,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    // 只有一个候选时无需选择
    if candidates.len() == 1 {
        return Some(0);
    }

    // 1. 粘性优先
    if lb_cfg.sticky.enabled {
        if let Some(key) = session_key {
            if let Some(idx) = lookup_sticky(state, key, candidates, lb_cfg, model) {
                return Some(idx);
            }
        }
    }

    // 2. 按策略选择
    let idx = select_by_strategy(state, candidates, model, lb_cfg.strategy);

    // 3. 记录粘性
    if lb_cfg.sticky.enabled {
        if let Some(key) = session_key {
            if let Some(i) = idx {
                record_sticky(state, key, i, &candidates[i], model);
            }
        }
    }

    idx
}

// ─────────────────────────── 粘性会话 ───────────────────────────

/// 查找粘性映射，命中且未过期时返回候选索引。
fn lookup_sticky(
    state: &AppState,
    session_key: &str,
    candidates: &[Candidate<'_>],
    lb_cfg: &LoadBalanceConfig,
    model: &str,
) -> Option<usize> {
    // 先判定失效原因，再决定是否立即回收条目（避免持锁期间调用 remove 死锁）
    enum Verdict {
        Hit(usize),
        Expired,
        Stale,
    }

    let verdict = match state.sticky_sessions.get(session_key) {
        None => return None,
        Some(entry) => {
            // 校验未过期
            if entry.created_at.elapsed().as_secs() >= lb_cfg.sticky.ttl_secs {
                Verdict::Expired
            } else if entry.candidate_index >= candidates.len() {
                // 校验候选索引仍有效（配置变更后候选数可能不同）
                Verdict::Stale
            } else {
                // 校验路由标识未变（配置调整或请求模型不同都会导致索引指向错误路线）
                let expected =
                    route_key_for_candidate(model, &candidates[entry.candidate_index]);
                if entry.route_key != expected {
                    Verdict::Stale
                } else {
                    Verdict::Hit(entry.candidate_index)
                }
            }
        }
    };

    // 失效条目立即回收，避免高并发短会话下 sticky_sessions 无界膨胀
    if matches!(verdict, Verdict::Expired | Verdict::Stale) {
        state.sticky_sessions.remove(session_key);
        return None;
    }

    let index = match verdict {
        Verdict::Hit(i) => i,
        _ => unreachable!(),
    };

    debug!(
        session_key = %session_key,
        index = index,
        "sticky session hit"
    );
    Some(index)
}

/// 将选中结果写入粘性映射。
pub fn record_sticky(
    state: &AppState,
    session_key: &str,
    index: usize,
    candidate: &Candidate<'_>,
    model: &str,
) {
    state.sticky_sessions.insert(
        session_key.to_string(),
        StickyEntry {
            candidate_index: index,
            route_key: route_key_for_candidate(model, candidate),
            created_at: Instant::now(),
        },
    );
}

// ─────────────────────────── 均衡策略 ───────────────────────────

/// 按指定策略选择候选索引。
fn select_by_strategy(
    state: &AppState,
    candidates: &[Candidate<'_>],
    model: &str,
    strategy: crate::proxy::config::LbStrategy,
) -> Option<usize> {
    match strategy {
        crate::proxy::config::LbStrategy::RoundRobin => round_robin(state, model, candidates.len()),
        crate::proxy::config::LbStrategy::Random => random_index(candidates.len()),
        crate::proxy::config::LbStrategy::Weighted => weighted_random(candidates),
        crate::proxy::config::LbStrategy::FastestResponse => {
            fastest_response(state, candidates, model)
        }
        crate::proxy::config::LbStrategy::LeastConnections => {
            least_connections(state, candidates, model)
        }
    }
}

/// 轮询：用原子计数器取模，保证均匀分配。
///
/// 计数器 key 包含候选数（`{model}#{len}`）：配置热重载导致候选数变化时
/// 自动切换到新序列，避免模数突变造成的分配错乱。
fn round_robin(state: &AppState, model: &str, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let key = format!("{model}#{len}");
    let counter = state
        .lb_counters
        .entry(key)
        .or_insert_with(|| std::sync::atomic::AtomicU64::new(0));
    let val = counter.fetch_add(1, Ordering::Relaxed);
    Some((val as usize) % len)
}

/// 均匀随机。
fn random_index(len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(rand::thread_rng().gen_range(0..len))
}

/// 加权随机：按 `route.weight` 比例分配概率。
///
/// 权重用 `u64` 累加，避免大权重下 `u32` 溢出。
fn weighted_random(candidates: &[Candidate<'_>]) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    let weights: Vec<u64> = candidates
        .iter()
        .map(|c| c.route.map(|r| r.weight as u64).unwrap_or(1))
        .collect();
    let total: u64 = weights.iter().sum();
    if total == 0 {
        return random_index(candidates.len());
    }
    let r = rand::thread_rng().gen_range(0..total);
    let mut acc = 0u64;
    for (i, &w) in weights.iter().enumerate() {
        acc += w;
        if r < acc {
            return Some(i);
        }
    }
    Some(candidates.len() - 1)
}

/// 按响应速度选择：EWMA 延迟最低的优先。
///
/// # 冷启动与陈旧数据处理
/// 无样本 / 样本过期的候选不再直接用 `u64::MAX` 惩罚，否则：
/// - 新加入的上游永远拿不到流量，无法产生样本，形成死锁；
/// - 陈旧样本会长期主导选择，把流量锁死在早已变慢的上游上。
///
/// 实现上给"无新鲜样本"的候选一个**乐观基线延迟**——取当前所有新鲜样本中
/// 的最小值的 80%（下浮 20% 使其相对更"吸引人"），从而保证它有机会被
/// 选中并完成探测。若所有候选都无新鲜样本，则退化为随机选择。
fn fastest_response(state: &AppState, candidates: &[Candidate<'_>], model: &str) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }

    // 收集每个候选的新鲜 EWMA 延迟
    let fresh: Vec<Option<u64>> = candidates
        .iter()
        .map(|c| {
            let key = route_key_for_candidate(model, c);
            state.route_latency.get(&key).and_then(|s| {
                if s.is_fresh() {
                    Some(s.avg_ms())
                } else {
                    None
                }
            })
        })
        .collect();

    // 全部无新鲜样本 → 随机选一个，让流量有机会产生首批样本
    let min_fresh = fresh.iter().filter_map(|v| *v).min();
    let baseline = match min_fresh {
        None => return random_index(candidates.len()),
        // 无新鲜样本的候选给一个略优于当前最快的乐观基线（下浮 20%）
        Some(m) => m.saturating_mul(4).saturating_div(5),
    };

    candidates
        .iter()
        .enumerate()
        .min_by_key(|(i, _)| fresh[*i].unwrap_or(baseline))
        .map(|(i, _)| i)
}

/// 最少在途连接数：选当前并发请求最少的候选。
fn least_connections(state: &AppState, candidates: &[Candidate<'_>], model: &str) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .min_by_key(|(_, c)| {
            let key = route_key_for_candidate(model, c);
            state
                .in_flight
                .get(&key)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0)
        })
        .map(|(i, _)| i)
}

// ─────────────────────────── 会话 Key 提取 ───────────────────────────

/// 从请求中提取会话标识。
///
/// 根据 `SessionKeySource` 配置，从 HTTP header、请求体字段或消息内容哈希
/// 中提取会话标识。返回 `None` 表示无法提取有效标识。
pub fn extract_session_key(
    headers: &HeaderMap,
    body: &serde_json::Value,
    source: &SessionKeySource,
) -> Option<String> {
    match source {
        SessionKeySource::Header(name) => headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        SessionKeySource::BodyField(field) => body
            .get(field)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        SessionKeySource::FirstUserMessage => {
            body.get("messages")
                .and_then(|m| m.as_array())
                .and_then(|msgs| {
                    msgs.iter()
                        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                })
                .and_then(|m| m.get("content"))
                .and_then(|c| {
                    // content 可能是字符串或数组（多模态），取文本部分
                    c.as_str().or_else(|| {
                        c.as_array().and_then(|arr| {
                            arr.iter()
                                .find_map(|p| p.get("text").and_then(|t| t.as_str()))
                        })
                    })
                })
                .map(|s| hash_str(s))
        }
        SessionKeySource::MessagesHash => body.get("messages").map(|m| hash_str(&m.to_string())),
    }
}

/// 用 `DefaultHasher` 对字符串做哈希，返回 16 位十六进制。
fn hash_str(s: &str) -> String {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

// ─────────────────────────── 运行时统计辅助 ───────────────────────────

/// 在途请求计数 +1。
pub fn inc_in_flight(state: &AppState, key: &str) {
    state
        .in_flight
        .entry(key.to_string())
        .or_insert_with(|| std::sync::atomic::AtomicU64::new(0))
        .fetch_add(1, Ordering::Relaxed);
}

/// 在途请求计数 -1。
///
/// 使用饱和减法：即使因异常路径重复归还，也不会下溢成 `u64::MAX`
/// （那会让该候选在 `LeastConnections` 下被永久排除且极难排查）。
pub fn dec_in_flight(state: &AppState, key: &str) {
    if let Some(counter) = state.in_flight.get(key) {
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            })
            .ok();
    }
}

/// 在途计数 RAII guard：构造时 +1，析构时 -1。
///
/// 用于流式请求——SSE 流的生命周期跨越 `call_upstream` 返回之后，
/// 计数必须持有到流真正结束（客户端断连 / 上游 EOF / 出错）才归还，
/// 否则 `LeastConnections` 策略下所有候选恒为 0，退化成"永远选第一个"。
///
/// 调用方需要"提前归还"时（非流式请求在拿到 JSON 响应后即可归还）
/// 使用 [`InFlightGuard::release`]。
pub struct InFlightGuard {
    state: Option<std::sync::Arc<AppState>>,
    key: String,
}

impl InFlightGuard {
    /// 计数 +1 并接管归还职责。
    pub fn new(state: std::sync::Arc<AppState>, key: &str) -> Self {
        inc_in_flight(&state, key);
        Self {
            state: Some(state),
            key: key.to_string(),
        }
    }

    /// 立即归还计数（之后析构不再重复归还）。
    pub fn release(mut self) {
        if let Some(state) = self.state.take() {
            dec_in_flight(&state, &self.key);
        }
    }

    /// 把 guard 的所有权转移进后台任务，使计数在流结束时才归还。
    ///
    /// 返回 `(Arc<AppState>, String)`，由后台任务在结束时调用
    /// [`dec_in_flight`] 完成归还。
    pub fn into_stream_parts(mut self) -> (std::sync::Arc<AppState>, String) {
        let state = self
            .state
            .take()
            .expect("InFlightGuard already released or moved");
        (state, self.key.clone())
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            dec_in_flight(&state, &self.key);
        }
    }
}

/// 记录延迟样本到路由延迟统计（用于 `FastestResponse` 策略）。
pub fn record_latency(state: &AppState, key: &str, latency_ms: u64) {
    let stats = state
        .route_latency
        .entry(key.to_string())
        .or_insert_with(|| crate::proxy::state::RouteLatencyStats::new());
    stats.record_latency(latency_ms);
}

// ─────────────────────────── 后台清理 ───────────────────────────

/// 启动后台定时任务，每 60 秒清理过期的 sticky 会话条目。
///
/// TTL 每轮从当前配置动态读取（而非启动时固化），使 GUI 修改粘性 TTL 后
/// 无需重启即可生效。返回的 `JoinHandle` 由调用方在服务关闭时 `abort()`。
pub fn spawn_sticky_cleanup(
    state: std::sync::Arc<AppState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            // 动态读取当前 TTL，配置热重载后立即生效
            let ttl_secs = state.config.read().load_balance.sticky.ttl_secs;
            let now = Instant::now();
            state
                .sticky_sessions
                .retain(|_, entry| now.duration_since(entry.created_at).as_secs() < ttl_secs);

            // 顺带回收长期未使用的轮询计数器条目，避免 lb_counters 无界增长。
            // 这里采用"分代清理"的简单策略：条目数超过阈值时整体重建，
            // 因为 AtomicU64 不携带使用时间信息，无法逐条判断空闲时长。
            if state.lb_counters.len() > LB_COUNTER_MAX {
                state.lb_counters.clear();
            }
        }
    })
}

/// `lb_counters` 条目数上限；超过后整体清空重建，防止无界增长。
const LB_COUNTER_MAX: usize = 1024;

// ─────────────────────────── 单元测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::{
        AppConfig, LbStrategy, LoadBalanceConfig, ProviderConfig, SessionKeySource, StickyConfig,
    };
    use crate::proxy::state::AppState;
    use std::path::PathBuf;

    fn make_state() -> AppState {
        let cfg = AppConfig::default();
        AppState::new(cfg, PathBuf::from("/tmp/test.json"))
    }

    fn make_candidates<'a>(cfg: &'a AppConfig, model: &'a str) -> Vec<Candidate<'a>> {
        cfg.resolve_candidates(model)
    }

    #[test]
    fn round_robin_cycles() {
        let mut cfg = AppConfig::default();
        cfg.providers = vec![
            ProviderConfig {
                id: "a".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
            ProviderConfig {
                id: "b".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
        ];
        let state = make_state();
        let candidates = make_candidates(&cfg, "m");
        assert_eq!(candidates.len(), 2);

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::RoundRobin,
            sticky: StickyConfig {
                enabled: false,
                ..Default::default()
            },
        };

        let i1 = select(&state, &candidates, "m", None, &lb).unwrap();
        let i2 = select(&state, &candidates, "m", None, &lb).unwrap();
        let i3 = select(&state, &candidates, "m", None, &lb).unwrap();
        // 轮询应该循环
        assert_ne!(i1, i2);
        assert_eq!(i1, i3);
    }

    #[test]
    fn sticky_session_hits_same_upstream() {
        let mut cfg = AppConfig::default();
        cfg.providers = vec![
            ProviderConfig {
                id: "a".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
            ProviderConfig {
                id: "b".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
        ];
        let state = make_state();
        let candidates = make_candidates(&cfg, "m");

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::RoundRobin,
            sticky: StickyConfig {
                enabled: true,
                session_key_source: SessionKeySource::Header("x-session-id".into()),
                ttl_secs: 1800,
            },
        };

        // 第一次选择（无 sticky 记录）→ 记录 sticky
        let i1 = select(&state, &candidates, "m", Some("sess-1"), &lb).unwrap();
        // 第二次选择（有 sticky 记录）→ 应命中同一索引
        let i2 = select(&state, &candidates, "m", Some("sess-1"), &lb).unwrap();
        assert_eq!(i1, i2, "sticky session should hit same upstream");

        // 不同会话 key 可能选到不同上游
        let _i3 = select(&state, &candidates, "m", Some("sess-2"), &lb);
    }

    #[test]
    fn fastest_response_picks_lowest_latency() {
        let mut cfg = AppConfig::default();
        cfg.providers = vec![
            ProviderConfig {
                id: "fast".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
            ProviderConfig {
                id: "slow".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
        ];
        let state = make_state();
        let candidates = make_candidates(&cfg, "m");

        // 给 "slow" 记录高延迟
        record_latency(&state, &route_key("m", "fast", ""), 50);
        record_latency(&state, &route_key("m", "slow", ""), 500);

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::FastestResponse,
            sticky: StickyConfig {
                enabled: false,
                ..Default::default()
            },
        };

        let idx = select(&state, &candidates, "m", None, &lb).unwrap();
        assert_eq!(candidates[idx].provider.id, "fast");
    }

    #[test]
    fn fastest_response_allows_cold_start() {
        // 从未成功过（无延迟样本）的候选不应被永久饿死：
        // 它必须有机会被选中以产生首批样本，否则新上游永远无法接入流量。
        let mut cfg = AppConfig::default();
        cfg.providers = vec![
            ProviderConfig {
                id: "known".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
            ProviderConfig {
                id: "unknown".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
        ];
        let state = make_state();
        let candidates = make_candidates(&cfg, "m");

        // 只给 "known" 记录延迟；"unknown" 无样本
        record_latency(&state, &route_key("m", "known", ""), 120);

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::FastestResponse,
            sticky: StickyConfig {
                enabled: false,
                ..Default::default()
            },
        };

        let idx = select(&state, &candidates, "m", None, &lb).unwrap();
        assert_eq!(
            candidates[idx].provider.id, "unknown",
            "无样本的候选应获得冷启动机会，而非被 u64::MAX 永久饿死"
        );

        // 冷启动后（"unknown" 产生样本且更慢），流量应回到真正更快的候选
        record_latency(&state, &route_key("m", "unknown", ""), 800);
        let idx2 = select(&state, &candidates, "m", None, &lb).unwrap();
        assert_eq!(
            candidates[idx2].provider.id, "known",
            "产生样本后应恢复按实际延迟选择"
        );
    }

    #[test]
    fn weighted_random_respects_weights() {
        // 权重 9:1 时，采样分布应显著偏向高权重候选
        let mut cfg = AppConfig::default();
        cfg.providers = vec![
            ProviderConfig {
                id: "heavy".into(),
                ..ProviderConfig::demo()
            },
            ProviderConfig {
                id: "light".into(),
                ..ProviderConfig::demo()
            },
        ];
        cfg.routes = vec![
            crate::proxy::config::RouteConfig {
                model: "m".into(),
                provider: "heavy".into(),
                weight: 9,
                ..Default::default()
            },
            crate::proxy::config::RouteConfig {
                model: "m".into(),
                provider: "light".into(),
                weight: 1,
                ..Default::default()
            },
        ];
        let state = make_state();
        let candidates = make_candidates(&cfg, "m");
        assert_eq!(candidates.len(), 2);

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::Weighted,
            sticky: StickyConfig {
                enabled: false,
                ..Default::default()
            },
        };

        let mut heavy = 0;
        let n = 4000;
        for _ in 0..n {
            let idx = select(&state, &candidates, "m", None, &lb).unwrap();
            if candidates[idx].provider.id == "heavy" {
                heavy += 1;
            }
        }
        // 期望约 90%，允许 ±5% 误差
        let ratio = heavy as f64 / n as f64;
        assert!(
            (0.85..=0.95).contains(&ratio),
            "weighted distribution off: got {ratio:.3}, expected ~0.90"
        );
    }

    #[test]
    fn dec_in_flight_never_underflows() {
        let state = make_state();
        let key = route_key("m", "p", "");
        // 未 inc 就 dec，或重复 dec，都不应下溢成 u64::MAX
        dec_in_flight(&state, &key);
        dec_in_flight(&state, &key);
        inc_in_flight(&state, &key);
        dec_in_flight(&state, &key);
        dec_in_flight(&state, &key);
        let v = state
            .in_flight
            .get(&key)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0);
        assert_eq!(v, 0, "在途计数不应下溢");
    }

    #[test]
    fn sticky_miss_when_route_key_stale() {
        let mut cfg = AppConfig::default();
        cfg.providers = vec![
            ProviderConfig {
                id: "a".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
            ProviderConfig {
                id: "b".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
        ];
        let state = make_state();
        let candidates = make_candidates(&cfg, "m");

        // 手工写入一条 route_key 与当前候选不匹配的过期粘性记录
        state.sticky_sessions.insert(
            "sess-x".into(),
            crate::proxy::state::StickyEntry {
                candidate_index: 1,
                route_key: "m|stale-provider|".into(),
                created_at: Instant::now(),
            },
        );

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::RoundRobin,
            sticky: StickyConfig {
                enabled: true,
                session_key_source: SessionKeySource::Header("x-session-id".into()),
                ttl_secs: 1800,
            },
        };

        let idx = select(&state, &candidates, "m", Some("sess-x"), &lb).unwrap();
        // route_key 不匹配 → 粘性失效 → 重新选择并覆盖记录
        let entry = state.sticky_sessions.get("sess-x").unwrap();
        assert_eq!(
            entry.candidate_index, idx,
            "stale sticky entry should be re-selected and overwritten"
        );
        assert_eq!(entry.route_key, route_key_for_candidate("m", &candidates[idx]));
    }

    #[test]
    fn extract_session_key_from_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "abc123".parse().unwrap());
        let body = serde_json::json!({});
        let key = extract_session_key(
            &headers,
            &body,
            &SessionKeySource::Header("x-session-id".into()),
        );
        assert_eq!(key.as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_session_key_from_first_user_message() {
        let headers = HeaderMap::new();
        let body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "you are helpful"},
                {"role": "user", "content": "hello world"},
            ]
        });
        let key = extract_session_key(&headers, &body, &SessionKeySource::FirstUserMessage);
        assert!(key.is_some());
        // 相同内容应产生相同 hash
        let key2 = extract_session_key(&headers, &body, &SessionKeySource::FirstUserMessage);
        assert_eq!(key, key2);
    }
}
