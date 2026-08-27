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
            if let Some(idx) = lookup_sticky(state, key, candidates, lb_cfg) {
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
) -> Option<usize> {
    let entry = state.sticky_sessions.get(session_key)?;
    // 校验未过期
    if entry.created_at.elapsed().as_secs() >= lb_cfg.sticky.ttl_secs {
        return None;
    }
    // 校验候选索引仍有效（配置变更后候选数可能不同）
    if entry.candidate_index >= candidates.len() {
        return None;
    }
    debug!(
        session_key = %session_key,
        index = entry.candidate_index,
        "sticky session hit"
    );
    Some(entry.candidate_index)
}

/// 将选中结果写入粘性映射。
fn record_sticky(
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
fn round_robin(state: &AppState, model: &str, len: usize) -> Option<usize> {
    let counter = state
        .lb_counters
        .entry(model.to_string())
        .or_insert_with(|| std::sync::atomic::AtomicU64::new(0));
    let val = counter.fetch_add(1, Ordering::Relaxed);
    Some((val as usize) % len)
}

/// 均匀随机：使用系统时间纳秒作为随机源，避免引入 rand 依赖。
fn random_index(len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(system_nanos() as usize % len)
}

/// 加权随机：按 `route.weight` 比例分配概率。
fn weighted_random(candidates: &[Candidate<'_>]) -> Option<usize> {
    let weights: Vec<u32> = candidates
        .iter()
        .map(|c| c.route.map(|r| r.weight).unwrap_or(1))
        .collect();
    let total: u32 = weights.iter().sum();
    if total == 0 {
        return random_index(candidates.len());
    }
    let r = (system_nanos() as u32) % total;
    let mut acc = 0u32;
    for (i, &w) in weights.iter().enumerate() {
        acc += w;
        if r < acc {
            return Some(i);
        }
    }
    Some(0)
}

/// 按响应速度选择：EWMA 延迟最低的优先。
///
/// 无数据时延迟视为 0（优先尝试以快速积累样本）。
fn fastest_response(state: &AppState, candidates: &[Candidate<'_>], model: &str) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .min_by_key(|(_, c)| {
            let key = route_key_for_candidate(model, c);
            state
                .route_latency
                .get(&key)
                .map(|s| s.avg_ms())
                .unwrap_or(0)
        })
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
pub fn dec_in_flight(state: &AppState, key: &str) {
    if let Some(counter) = state.in_flight.get(key) {
        counter.fetch_sub(1, Ordering::Relaxed);
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
pub fn spawn_sticky_cleanup(state: std::sync::Arc<AppState>, ttl_secs: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            interval.tick().await;
            let now = Instant::now();
            state
                .sticky_sessions
                .retain(|_, entry| now.duration_since(entry.created_at).as_secs() < ttl_secs);
        }
    });
}

// ─────────────────────────── 内部工具 ───────────────────────────

/// 获取当前系统时间的纳秒部分（用于轻量随机）。
fn system_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u128)
        .unwrap_or(0)
}

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
