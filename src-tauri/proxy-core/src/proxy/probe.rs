//! 主动探活任务：熔断到期后自动试探上游，无需等待真实请求。
//!
//! 熔断打开的渠道在冷却结束后不会自动关闭——需要"试探成功"才能恢复。
//! 被动方式是等第一个真实请求充当试探（见 [`crate::proxy::circuit`] 的半开状态），
//! 但低流量渠道可能长时间没有请求，恢复被无限推迟。
//! 本任务按 `probe_interval_secs` 轮询所有到期熔断器，主动发试探请求；
//! 与被动半开共用同一状态机（单探测名额 CAS 保证同一时刻只有一个试探在途）。
//!
//! # 已知限制（依赖被动半开兜底）
//! - embeddings 等非对话路由的试探可能 404 → 保持熔断，等真实请求半开恢复
//! - 个别上游拒绝 `max_tokens=1` → 同样保持熔断

use crate::proxy::circuit::{BreakerState, CircuitBreaker};
use crate::proxy::config::ProviderConfig;
use crate::proxy::headers::build_upstream_headers;
use crate::proxy::state::AppState;
use axum::http::HeaderMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// 试探请求超时（秒）——内部固定参数，不对外暴露配置。
///
/// 试探请求 `max_tokens=1`，正常应在数秒内返回；15s 足以覆盖冷启动的
/// 慢上游，又不至于让探活任务在一轮里卡太久。
pub const PROBE_TIMEOUT_SECS: u64 = 15;

/// 启动后台探活任务（与 sticky 清理同模式：serve 处 spawn，服务关闭时 abort）。
///
/// 探活间隔每轮动态读取（GUI 修改 `probe_interval_secs` 后无需重启生效）。
pub fn spawn_probe_task(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // 探活间隔动态读取；钳到 ≥5s，防止误配 0/1 把试探流量放大成压力
            let interval_secs = state
                .config
                .read()
                .retry
                .circuit_breaker
                .probe_interval_secs
                .max(5);
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            let recovered = probe_round(&state).await;
            if recovered > 0 {
                info!(recovered, "probe round recovered breakers");
            }
        }
    })
}

/// 一轮探活：遍历所有可试探的熔断器（OPEN 已到期 / HALF_OPEN），逐个发试探请求。
///
/// 返回本轮促成恢复（转 Closed）的熔断器数量（测试断言用）。
///
/// # 行为
/// - 熔断未启用（`enabled=false`）→ 直接跳过
/// - CLOSED / OPEN 未到期 → `try_acquire_probe_slot` 返回 false，跳过
/// - 抢到探测名额 → 发试探请求；2xx → `on_success`（计入恢复阈值，连续成功达
///   阈值才关闭）；非 2xx / 网络错误 → `probe_failed`（退回 OPEN 但不重置时钟，
///   下轮探活可立即再试）
/// - route_key 解析不出启用的 provider（渠道已删/禁用）→ 清理该熔断器条目
pub async fn probe_round(state: &Arc<AppState>) -> usize {
    let (enabled, cb_cfg, providers) = {
        let cfg = state.config.read();
        (
            cfg.retry.circuit_breaker.enabled,
            cfg.retry.circuit_breaker.clone(),
            cfg.providers.clone(),
        )
    };
    if !enabled {
        return 0;
    }

    // 先做快照再逐个处理，避免遍历期间长时间持有 dashmap 分片锁
    let snapshot: Vec<(String, Arc<CircuitBreaker>)> = state
        .breakers
        .iter()
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();

    let mut recovered = 0;
    for (route_key, breaker) in snapshot {
        // CLOSED 不需要探活；OPEN 未到期抢不到名额——都返回 false
        if !breaker.try_acquire_probe_slot(&cb_cfg) {
            continue;
        }

        // route_key = model|provider_id|upstream_model
        let Some((model, provider_id, upstream_model)) = parse_route_key(&route_key) else {
            continue;
        };

        // 渠道已删除/禁用：清理残留的熔断器条目
        let Some(provider) = providers
            .iter()
            .find(|p| p.id == provider_id && p.enabled)
        else {
            state.breakers.remove(&route_key);
            continue;
        };

        // 试探模型名：显式路由的 upstream_model，否则原模型名
        let probe_model = if upstream_model.is_empty() {
            model
        } else {
            upstream_model
        };

        let before = breaker.status().state;
        match send_probe(state, provider, probe_model).await {
            Ok(()) => {
                breaker.on_success(&cb_cfg);
                let after = breaker.status().state;
                if after == BreakerState::Closed && before != BreakerState::Closed {
                    recovered += 1;
                    info!(route_key = %route_key, "探活成功，熔断器已恢复");
                } else {
                    info!(route_key = %route_key, "探活成功（计入恢复阈值）");
                }
            }
            Err(reason) => {
                // 不重置熔断时钟：opened_at 保持原值，下一轮探活可立即再试
                breaker.probe_failed();
                info!(route_key = %route_key, reason = %reason, "探活失败，保持熔断");
            }
        }
    }
    recovered
}

/// 发送试探请求：`POST {base_url}/chat/completions`，`max_tokens=1`。
///
/// 返回 `Ok(())` 表示 2xx（健康）；`Err(reason)` 表示未恢复（非 2xx 或网络错误）。
async fn send_probe(
    state: &Arc<AppState>,
    provider: &ProviderConfig,
    model: &str,
) -> Result<(), String> {
    let url = provider.endpoint("chat/completions");
    let body = probe_body(model);
    let headers = build_upstream_headers(&HeaderMap::new(), provider);

    let mut req = state
        .http
        .post(&url)
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS));
    for (name, value) in headers {
        req = req.header(name, value);
    }

    match req.json(&body).send().await {
        Ok(resp) => {
            if resp.status().is_success() {
                Ok(())
            } else {
                Err(format!("probe got HTTP {}", resp.status().as_u16()))
            }
        }
        Err(e) => Err(format!("probe network error: {e}")),
    }
}

/// 构造试探请求体。
fn probe_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
    })
}

/// 解析 route_key：`model|provider_id|upstream_model`。
///
/// 返回 `None` 表示格式不合法（不应出现，防御式处理）。
fn parse_route_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.splitn(3, '|');
    let model = parts.next()?;
    let provider = parts.next()?;
    let upstream = parts.next().unwrap_or("");
    if model.is_empty() || provider.is_empty() {
        return None;
    }
    Some((model, provider, upstream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_body_shape() {
        let b = probe_body("glm-4-plus");
        assert_eq!(b["model"], "glm-4-plus");
        assert_eq!(b["max_tokens"], 1);
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["messages"][0]["content"], "ping");
        // 试探请求必须极小：不带 stream，避免误触流式路径
        assert!(b.get("stream").is_none());
    }

    #[test]
    fn route_key_parsing() {
        // 显式路由：upstream_model 非空
        let (m, p, u) = parse_route_key("gpt-4o|openai|gpt-4o-2024").unwrap();
        assert_eq!((m, p, u), ("gpt-4o", "openai", "gpt-4o-2024"));
        // 隐式候选：upstream_model 为空
        let (m, p, u) = parse_route_key("glm-4|zhipu|").unwrap();
        assert_eq!((m, p, u), ("glm-4", "zhipu", ""));
        // 不合法：缺 provider
        assert!(parse_route_key("only-model|").is_none());
        assert!(parse_route_key("").is_none());
    }
}
