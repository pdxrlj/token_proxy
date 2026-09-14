//! 负载均衡选择器：从候选路由中按策略选一个。
//!
//! 支持会话粘性（sticky session），确保同一会话的重试/多轮对话请求
//! 始终路由到同一上游，避免上下文丢失或缓存失效。
//!
//! # 模块结构
//! - [`select`]         — 主入口：粘性优先（含熔断校验）→ 策略选择（eligible 子集）→ 记录粘性
//! - 策略函数           — `smooth_weighted_round_robin`（SWRR，RoundRobin/Weighted 共用）
//!                       / `random_index` / `fastest_response` / `least_connections`
//! - 慢启动             — 熔断恢复后有效权重从 10% 线性爬升到 100%（Nginx `slow_start` 语义）
//! - 会话 Key 提取      — [`extract_session_key`] 从请求中提取会话标识
//! - 运行时统计辅助     — `inc_in_flight` / `dec_in_flight` / `record_latency`
//! - 后台清理           — [`spawn_sticky_cleanup`] 定时回收过期粘性与失效 SWRR 条目
//!
//! # 均衡算法选型（参考成熟方案）
//! - **SWRR**（Smooth Weighted Round Robin，Nginx 同款）：替换朴素计数器取模轮询
//!   与加权随机——分配序列均匀无突发、权重感知、候选集热变化时自然过渡
//! - **慢启动**（Nginx `slow_start`）：熔断恢复的渠道权重逐步爬升，避免刚恢复
//!   就被全量流量冲垮（与熔断器半开恢复阈值形成双层平滑）
//! - `FastestResponse`（EWMA 乐观基线）与 `LeastConnections`（在途计数）天然具备
//!   恢复特性，不需要额外慢启动处理

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
///
/// 供模块内策略函数与 api.rs 的 eligible 过滤共用。
pub fn route_key_for_candidate(model: &str, candidate: &Candidate<'_>) -> String {
    let upstream_model = candidate
        .route
        .map(|r| r.upstream_model.as_str())
        .unwrap_or("");
    route_key(model, &candidate.provider.id, upstream_model)
}

// ─────────────────────────── 主选择入口 ───────────────────────────

/// 从候选列表中选择一个索引（索引相对传入的完整候选列表）。
///
/// # 选择顺序
/// 1. **粘性优先**：若启用 sticky 且有 session_key，先查粘性映射命中；
///    命中但目标已熔断（不在 eligible 子集内）时视为未命中，走策略重选
/// 2. **策略选择**：在 `eligible` 子集上按 `lb_cfg.strategy` 选择
/// 3. **记录粘性**：选中后若有 session_key 则写入粘性映射（记录完整列表索引）
///
/// # 参数
/// - `eligible`：可参与选择的候选索引（熔断过滤后的子集）；`None` 表示不过滤
///
/// # 返回
/// `Some(index)` 表示选中候选在完整列表中的索引；`None` 表示候选列表为空。
pub fn select(
    state: &AppState,
    candidates: &[Candidate<'_>],
    model: &str,
    session_key: Option<&str>,
    lb_cfg: &LoadBalanceConfig,
    eligible: Option<&[usize]>,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }

    // 参与选择的候选池（熔断过滤后的 eligible 子集）
    let pool: Vec<usize> = match eligible {
        Some(e) => e.to_vec(),
        None => (0..candidates.len()).collect(),
    };
    if pool.is_empty() {
        return None;
    }
    if pool.len() == 1 {
        return Some(pool[0]);
    }

    // 1. 粘性优先（命中且目标可参与选择时直接复用）
    if lb_cfg.sticky.enabled {
        if let Some(key) = session_key {
            if let Some(idx) = lookup_sticky(state, key, candidates, lb_cfg, model) {
                if pool.contains(&idx) {
                    return Some(idx);
                }
                // 粘性目标已熔断：视为未命中，走策略重选
            }
        }
    }

    // 2. 在 eligible 子集上按策略选择
    let subset: Vec<Candidate<'_>> = pool.iter().map(|&i| candidates[i]).collect();
    let pos = select_by_strategy(state, &subset, model, lb_cfg.strategy)?;
    let orig = pool[pos];

    // 3. 记录粘性（记录完整列表索引，下次校验 route_key 用）
    if lb_cfg.sticky.enabled {
        if let Some(key) = session_key {
            record_sticky(state, key, orig, &candidates[orig], model);
        }
    }

    Some(orig)
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

    let verdict = match state.sticky_sessions.get_mut(session_key) {
        None => return None,
        Some(mut entry) => {
            // 校验未过期
            if entry.created_at.elapsed().as_secs() >= lb_cfg.sticky.ttl_secs {
                Verdict::Expired
            } else if entry.candidate_index >= candidates.len() {
                // 校验候选索引仍有效（配置变更后候选数可能不同）
                Verdict::Stale
            } else {
                // 校验路由标识未变（配置调整或请求模型不同都会导致索引指向错误路线）
                let expected = route_key_for_candidate(model, &candidates[entry.candidate_index]);
                if entry.route_key != expected {
                    Verdict::Stale
                } else {
                    // 命中即续期（滑动过期）：持续活跃的长会话不会被 TTL 强制
                    // 切换上游；仅连续 ttl_secs 内无请求才真正过期
                    entry.created_at = Instant::now();
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
        // 轮询 → SWRR（所有候选等权）：平滑轮询，分配序列无突发
        crate::proxy::config::LbStrategy::RoundRobin => {
            smooth_weighted_round_robin(state, candidates, model, false)
        }
        // 加权随机 → SWRR（route.weight）：平滑加权轮询，比例精确无方差
        crate::proxy::config::LbStrategy::Weighted => {
            smooth_weighted_round_robin(state, candidates, model, true)
        }
        crate::proxy::config::LbStrategy::Random => random_index(candidates.len()),
        crate::proxy::config::LbStrategy::FastestResponse => {
            fastest_response(state, candidates, model)
        }
        crate::proxy::config::LbStrategy::LeastConnections => {
            least_connections(state, candidates, model)
        }
    }
}

// ─────────────────────────── SWRR 平滑加权轮询 ───────────────────────────

/// 慢启动时长（秒）：熔断恢复后有效权重从 10% 线性爬升到 100% 的时间窗口。
///
/// 内部固定参数（Nginx `slow_start` 同款语义），不对外暴露配置。
pub const SLOW_START_SECS: u64 = 120;
/// 慢启动起始比例（百分比）：恢复瞬间只放 10% 流量
const SLOW_START_MIN_PCT: u64 = 10;
/// 权重放大系数：×10 后做整数运算，让 weight=1 的候选在慢启动期
/// 也能表达 10%~90% 的中间权重（否则 1×10% 向下取整恒为 0）
const WEIGHT_SCALE: i64 = 10;

/// SWRR 平滑加权轮询（Nginx 算法）。
///
/// 每轮：`cw[i] += w_eff[i]`；取 `cw` 最大者（并列取靠前者）；`cw[winner] -= Σw_eff`。
/// - 分配序列均匀无突发：权重 5:1:1 → 每 7 次恰好 5:1:1
/// - 候选集变化时自然过渡：cw 残留值有界，热重载后不会错乱
/// - `w_eff` 叠加慢启动：熔断恢复（`recovered_at` 有值）后权重从 10% 爬升到 100%
///
/// `use_route_weights=false` 时所有候选视为等权（RoundRobin 语义）。
fn smooth_weighted_round_robin(
    state: &AppState,
    candidates: &[Candidate<'_>],
    model: &str,
    use_route_weights: bool,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }

    // 有效权重 = 基础权重 × 慢启动因子（放大 10 倍做整数运算）
    let weights: Vec<i64> = candidates
        .iter()
        .map(|c| {
            let base: i64 = if use_route_weights {
                c.route.map(|r| r.weight).unwrap_or(1).max(1) as i64
            } else {
                1
            };
            effective_weight(state, model, c, base)
        })
        .collect();
    let total: i64 = weights.iter().sum();
    if total <= 0 {
        return random_index(candidates.len());
    }

    // 整轮「累加 → 取最大 → 减 total」在同一把锁内完成，保证并发下仍是
    // 严格 Nginx SWRR 序列（逐键原子会被并发交错打破序列不变量）
    let mut swrr = state.swrr_weights.lock();

    // cw[i] += w[i]，同时取最大者（并列取靠前者，保证确定性）
    let mut winner = 0;
    let mut winner_cw = i64::MIN;
    for (i, c) in candidates.iter().enumerate() {
        let key = route_key_for_candidate(model, c);
        let cw = swrr.entry(key).or_insert(0);
        *cw += weights[i];
        if *cw > winner_cw {
            winner_cw = *cw;
            winner = i;
        }
    }

    // cw[winner] -= total
    let winner_key = route_key_for_candidate(model, &candidates[winner]);
    if let Some(cw) = swrr.get_mut(&winner_key) {
        *cw -= total;
    }

    Some(winner)
}

/// 候选有效权重：基础权重 × 慢启动因子。
///
/// - 熔断器无恢复记录（未熔断 / 手动 reset）→ 全量权重
/// - 恢复后 `t` 秒内 → `pct = 10 + 90×t/120` 线性爬升，封顶 100%
fn effective_weight(state: &AppState, model: &str, c: &Candidate<'_>, base: i64) -> i64 {
    let key = route_key_for_candidate(model, c);
    let recovered = state.breakers.get(&key).and_then(|b| b.recovered_at());
    match recovered {
        None => base * WEIGHT_SCALE,
        Some(at) => {
            let t = at.elapsed().as_secs();
            let pct =
                (SLOW_START_MIN_PCT + (100 - SLOW_START_MIN_PCT) * t / SLOW_START_SECS).min(100);
            // base×10×pct/100：weight=1 起步即 1（满权 10 的 10%）
            base * WEIGHT_SCALE * (pct as i64) / 100
        }
    }
}

/// 均匀随机。
fn random_index(len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(rand::thread_rng().gen_range(0..len))
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

    // 存在无新鲜样本的冷候选时，随机挑一个做探测：基线已保证冷候选优先，
    // 随机化避免多个冷候选按索引顺序被逐个"喂样本"，把探测流量摊开
    let cold: Vec<usize> = fresh
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_none())
        .map(|(i, _)| i)
        .collect();
    if !cold.is_empty() {
        let pick = rand::thread_rng().gen_range(0..cold.len());
        return Some(cold[pick]);
    }

    Some(
        fresh
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| v.unwrap_or(baseline))
            .map(|(i, _)| i)
            .unwrap_or(0),
    )
}

/// 最少在途连接数：选当前并发请求最少的候选。
///
/// 平局（常见于全体在途数为 0）随机打散：避免"请求结束 → 下一个请求
/// 立刻到来"的节奏下流量短期扎堆在列表第一个候选上。
fn least_connections(state: &AppState, candidates: &[Candidate<'_>], model: &str) -> Option<usize> {
    let counts: Vec<u64> = candidates
        .iter()
        .map(|c| {
            let key = route_key_for_candidate(model, c);
            state
                .in_flight
                .get(&key)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0)
        })
        .collect();
    let min = counts.iter().min()?;
    let tied: Vec<usize> = counts
        .iter()
        .enumerate()
        .filter(|(_, v)| *v == min)
        .map(|(i, _)| i)
        .collect();
    let pick = rand::thread_rng().gen_range(0..tied.len());
    Some(tied[pick])
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

    /// 本 guard 对应的 route_key（`model|provider_id|upstream_model`）。
    ///
    /// 流式请求转发时用它回查熔断器（断流记失败），与 route_and_call
    /// 的 key 完全一致，避免按 log 字段重新拼接产生偏差。
    pub fn key(&self) -> &str {
        &self.key
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

/// 启动后台定时任务，每 60 秒清理过期的 sticky 会话条目与失效的 SWRR 条目。
///
/// TTL 与有效 route_key 集合每轮从当前配置动态读取（而非启动时固化），
/// 使 GUI 修改配置后无需重启即可生效。返回的 `JoinHandle` 由调用方在服务关闭时 `abort()`。
pub fn spawn_sticky_cleanup(state: std::sync::Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            // 动态读取当前 TTL 与有效 route_key 集合，配置热重载后立即生效
            let (ttl_secs, valid_keys) = {
                let cfg = state.config.read();
                (cfg.load_balance.sticky.ttl_secs, valid_route_keys(&cfg))
            };
            let now = Instant::now();
            state
                .sticky_sessions
                .retain(|_, entry| now.duration_since(entry.created_at).as_secs() < ttl_secs);

            // 回收失效的 SWRR 条目：route_key 不在当前配置中的直接删除
            // （渠道/路由被删后残留的 current_weight 无意义）；
            // 容量上限保护：极端情况下（如配置解析无法覆盖的组合键）整体清空重建。
            {
                let mut swrr = state.swrr_weights.lock();
                swrr.retain(|k, _| valid_keys.contains(k));
                if swrr.len() > SWRR_MAP_MAX {
                    swrr.clear();
                }
            }
        }
    })
}

/// 当前配置可能产生的全部 route_key 集合（SWRR 条目有效性判定用）。
///
/// 与 [`resolve_candidates`](crate::proxy::config::AppConfig::resolve_candidates)
/// 的解析规则保持一致：
/// - 显式路由：`model | provider_id | upstream_model`
/// - 隐式候选：provider.models 中的模型 → `model | provider_id | ""`
fn valid_route_keys(cfg: &crate::proxy::config::AppConfig) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    for r in &cfg.routes {
        keys.insert(route_key(&r.model, &r.provider, &r.upstream_model));
    }
    for p in &cfg.providers {
        for m in &p.models {
            keys.insert(route_key(m, &p.id, ""));
        }
    }
    keys
}

/// `swrr_weights` 条目数上限；超过后整体清空重建，防止无界增长。
const SWRR_MAP_MAX: usize = 4096;

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

        let i1 = select(&state, &candidates, "m", None, &lb, None).unwrap();
        let i2 = select(&state, &candidates, "m", None, &lb, None).unwrap();
        let i3 = select(&state, &candidates, "m", None, &lb, None).unwrap();
        // SWRR 平滑轮询应该循环
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
        let i1 = select(&state, &candidates, "m", Some("sess-1"), &lb, None).unwrap();
        // 第二次选择（有 sticky 记录）→ 应命中同一索引
        let i2 = select(&state, &candidates, "m", Some("sess-1"), &lb, None).unwrap();
        assert_eq!(i1, i2, "sticky session should hit same upstream");

        // 不同会话 key 可能选到不同上游
        let _i3 = select(&state, &candidates, "m", Some("sess-2"), &lb, None);
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

        let idx = select(&state, &candidates, "m", None, &lb, None).unwrap();
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

        let idx = select(&state, &candidates, "m", None, &lb, None).unwrap();
        assert_eq!(
            candidates[idx].provider.id, "unknown",
            "无样本的候选应获得冷启动机会，而非被 u64::MAX 永久饿死"
        );

        // 冷启动后（"unknown" 产生样本且更慢），流量应回到真正更快的候选
        record_latency(&state, &route_key("m", "unknown", ""), 800);
        let idx2 = select(&state, &candidates, "m", None, &lb, None).unwrap();
        assert_eq!(
            candidates[idx2].provider.id, "known",
            "产生样本后应恢复按实际延迟选择"
        );
    }

    #[test]
    fn swrr_weighted_distribution_exact() {
        // 权重 9:1 → SWRR 每 10 次恰好 9:1（比例精确，无随机方差）
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
        for _ in 0..10 {
            let idx = select(&state, &candidates, "m", None, &lb, None).unwrap();
            if candidates[idx].provider.id == "heavy" {
                heavy += 1;
            }
        }
        assert_eq!(heavy, 9, "SWRR 权重 9:1 应在 10 次内恰好分配 9:1");
    }

    #[test]
    fn swrr_sequence_matches_weights() {
        // 权重 5:1:1 → SWRR 每 7 次恰好 5:1:1，且分布无连续突发
        let mut cfg = AppConfig::default();
        cfg.providers = vec![
            ProviderConfig {
                id: "heavy".into(),
                ..ProviderConfig::demo()
            },
            ProviderConfig {
                id: "l1".into(),
                ..ProviderConfig::demo()
            },
            ProviderConfig {
                id: "l2".into(),
                ..ProviderConfig::demo()
            },
        ];
        cfg.routes = vec![
            crate::proxy::config::RouteConfig {
                model: "m".into(),
                provider: "heavy".into(),
                weight: 5,
                ..Default::default()
            },
            crate::proxy::config::RouteConfig {
                model: "m".into(),
                provider: "l1".into(),
                weight: 1,
                ..Default::default()
            },
            crate::proxy::config::RouteConfig {
                model: "m".into(),
                provider: "l2".into(),
                weight: 1,
                ..Default::default()
            },
        ];
        let state = make_state();
        let candidates = make_candidates(&cfg, "m");
        assert_eq!(candidates.len(), 3);

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::Weighted,
            sticky: StickyConfig {
                enabled: false,
                ..Default::default()
            },
        };

        let mut counts = std::collections::BTreeMap::new();
        for _ in 0..7 {
            let idx = select(&state, &candidates, "m", None, &lb, None).unwrap();
            *counts
                .entry(candidates[idx].provider.id.clone())
                .or_insert(0) += 1;
        }
        assert_eq!(counts.get("heavy"), Some(&5));
        assert_eq!(counts.get("l1"), Some(&1));
        assert_eq!(counts.get("l2"), Some(&1));
    }

    #[test]
    fn slow_start_ramps_recovered_candidate() {
        // A 刚从熔断恢复（recovered_at=now，慢启动 10%），B 全量：
        // 等权下 A 的有效权重 = 满权的 1/10 → 11 次选择中 A 恰好 1 次
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

        // 构造 A 的"刚恢复"熔断器：跳闸 → 到期(open_duration=0) → 半开 → 成功关闭
        let breaker = std::sync::Arc::new(crate::proxy::circuit::CircuitBreaker::new());
        let zero = crate::proxy::config::CircuitBreakerConfig {
            open_duration_secs: 0,
            recovery_success_threshold: 1,
            ..Default::default()
        };
        breaker.trip_now();
        assert!(
            breaker.before_request(&zero).is_ok(),
            "open_duration=0 → 立即到期转半开"
        );
        breaker.on_success(&zero);
        assert_eq!(
            breaker.status().state,
            crate::proxy::circuit::BreakerState::Closed
        );
        assert!(breaker.recovered_at().is_some(), "关闭时应记录恢复时刻");
        state.breakers.insert(route_key("m", "a", ""), breaker);

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::RoundRobin,
            sticky: StickyConfig {
                enabled: false,
                ..Default::default()
            },
        };

        let mut a_count = 0;
        for _ in 0..11 {
            let idx = select(&state, &candidates, "m", None, &lb, None).unwrap();
            if candidates[idx].provider.id == "a" {
                a_count += 1;
            }
        }
        assert_eq!(a_count, 1, "慢启动期 A 只应拿到约 10% 流量（11 次中 1 次）");
    }

    #[test]
    fn select_restricted_to_eligible() {
        // 候选 [a, b, c]，b 已熔断（eligible=[0,2]）→ 选择永远不落到 b
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
            ProviderConfig {
                id: "c".into(),
                models: vec!["m".into()],
                ..ProviderConfig::demo()
            },
        ];
        let state = make_state();
        let candidates = make_candidates(&cfg, "m");
        assert_eq!(candidates.len(), 3);

        // b 的熔断器：跳闸后未到期（默认 3600s）→ 不在 eligible 内
        let breaker = std::sync::Arc::new(crate::proxy::circuit::CircuitBreaker::new());
        breaker.trip_now();
        state.breakers.insert(route_key("m", "b", ""), breaker);

        let lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::RoundRobin,
            sticky: StickyConfig {
                enabled: false,
                ..Default::default()
            },
        };

        let eligible = [0usize, 2];
        for _ in 0..20 {
            let idx = select(&state, &candidates, "m", None, &lb, Some(&eligible)).unwrap();
            assert_ne!(candidates[idx].provider.id, "b", "熔断候选不应被选中");
        }

        // 粘性指向已熔断的 b：命中但不在 eligible 内 → 视为未命中重选
        let sticky_lb = LoadBalanceConfig {
            enabled: true,
            strategy: LbStrategy::RoundRobin,
            sticky: StickyConfig {
                enabled: true,
                session_key_source: SessionKeySource::Header("x-session-id".into()),
                ttl_secs: 1800,
            },
        };
        state.sticky_sessions.insert(
            "sess-b".into(),
            crate::proxy::state::StickyEntry {
                candidate_index: 1,
                route_key: route_key("m", "b", ""),
                created_at: Instant::now(),
            },
        );
        let idx = select(
            &state,
            &candidates,
            "m",
            Some("sess-b"),
            &sticky_lb,
            Some(&eligible),
        )
        .unwrap();
        assert_ne!(candidates[idx].provider.id, "b", "粘性目标已熔断应重选");
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

        let idx = select(&state, &candidates, "m", Some("sess-x"), &lb, None).unwrap();
        // route_key 不匹配 → 粘性失效 → 重新选择并覆盖记录
        let entry = state.sticky_sessions.get("sess-x").unwrap();
        assert_eq!(
            entry.candidate_index, idx,
            "stale sticky entry should be re-selected and overwritten"
        );
        assert_eq!(
            entry.route_key,
            route_key_for_candidate("m", &candidates[idx])
        );
    }

    #[test]
    fn sticky_hit_renews_ttl() {
        // 命中粘性记录应刷新 created_at（滑动过期）：长会话持续活跃时不被 TTL 切走
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

        let old = Instant::now() - std::time::Duration::from_secs(100);
        state.sticky_sessions.insert(
            "sess-r".into(),
            crate::proxy::state::StickyEntry {
                candidate_index: 0,
                route_key: route_key("m", "a", ""),
                created_at: old,
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

        let idx = select(&state, &candidates, "m", Some("sess-r"), &lb, None).unwrap();
        assert_eq!(idx, 0, "未过期且 route_key 匹配应命中");
        let entry = state.sticky_sessions.get("sess-r").unwrap();
        assert!(
            entry.created_at > old,
            "命中后 created_at 应被刷新（滑动过期）"
        );
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
