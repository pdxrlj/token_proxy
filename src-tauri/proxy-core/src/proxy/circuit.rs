use crate::proxy::config::CircuitBreakerConfig;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

const CLOSED: u8 = 0;
const OPEN: u8 = 1;
const HALF_OPEN: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl From<u8> for BreakerState {
    fn from(v: u8) -> Self {
        match v {
            OPEN => BreakerState::Open,
            HALF_OPEN => BreakerState::HalfOpen,
            _ => BreakerState::Closed,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BreakerStatus {
    pub state: BreakerState,
    pub consecutive_failures: u32,
}

/// 简单熔断器：连续失败达阈值 → 打开 → 冷却后半开 → 试探成功则关闭
///
/// 半开状态仅放行**一个**试探请求（`probing` 竞争探测名额），
/// 探测结果出来前其余请求仍被拦截，避免全部流量涌向尚未确认恢复的上游。
pub struct CircuitBreaker {
    state: AtomicU8,
    failures: AtomicU32,
    opened_at: Mutex<Option<Instant>>,
    /// 半开探测名额：true 表示已有试探请求在途
    probing: AtomicBool,
    cfg: CircuitBreakerConfig,
}

impl Clone for CircuitBreaker {
    fn clone(&self) -> Self {
        Self {
            state: AtomicU8::new(self.state.load(Ordering::Relaxed)),
            failures: AtomicU32::new(self.failures.load(Ordering::Relaxed)),
            opened_at: Mutex::new(*self.opened_at.lock()),
            probing: AtomicBool::new(self.probing.load(Ordering::Relaxed)),
            cfg: self.cfg.clone(),
        }
    }
}

impl CircuitBreaker {
    pub fn new(cfg: CircuitBreakerConfig) -> Self {
        Self {
            state: AtomicU8::new(CLOSED),
            failures: AtomicU32::new(0),
            opened_at: Mutex::new(None),
            probing: AtomicBool::new(false),
            cfg,
        }
    }

    /// 请求前调用；熔断打开时返回 Err
    pub fn before_request(&self) -> Result<(), BreakerOpen> {
        match self.state.load(Ordering::Relaxed) {
            CLOSED => Ok(()),
            // 半开：只放行一个试探请求
            HALF_OPEN => self.acquire_probe(),
            OPEN => {
                let cooled_down = self
                    .opened_at
                    .lock()
                    .map(|t| t.elapsed() >= Duration::from_secs(self.cfg.cooldown_secs))
                    .unwrap_or(false);
                if cooled_down {
                    // 冷却结束 → 半开，竞争探测名额
                    self.state.store(HALF_OPEN, Ordering::Relaxed);
                    self.acquire_probe()
                } else {
                    Err(BreakerOpen)
                }
            }
            _ => Ok(()),
        }
    }

    /// 竞争半开探测名额：仅一个请求可通过，其余继续被拦截
    fn acquire_probe(&self) -> Result<(), BreakerOpen> {
        if self
            .probing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Ok(())
        } else {
            Err(BreakerOpen)
        }
    }

    pub fn on_success(&self) {
        self.failures.store(0, Ordering::Relaxed);
        self.state.store(CLOSED, Ordering::Relaxed);
        self.probing.store(false, Ordering::Relaxed);
        *self.opened_at.lock() = None;
    }

    pub fn on_failure(&self) -> bool {
        let f = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        // 释放探测名额（半开探测失败会随阈值判定重新跳闸）
        self.probing.store(false, Ordering::Relaxed);
        if f >= self.cfg.failure_threshold {
            self.state.store(OPEN, Ordering::Relaxed);
            *self.opened_at.lock() = Some(Instant::now());
            true
        } else {
            false
        }
    }

    pub fn reset(&self) {
        self.failures.store(0, Ordering::Relaxed);
        self.state.store(CLOSED, Ordering::Relaxed);
        self.probing.store(false, Ordering::Relaxed);
        *self.opened_at.lock() = None;
    }

    pub fn status(&self) -> BreakerStatus {
        BreakerStatus {
            state: self.state.load(Ordering::Relaxed).into(),
            consecutive_failures: self.failures.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
pub struct BreakerOpen;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_after_threshold_and_recovers() {
        let b = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 3,
            cooldown_secs: 1,
            enabled: true,
        });
        assert!(b.before_request().is_ok());
        b.on_failure();
        b.on_failure();
        assert!(b.before_request().is_ok()); // 未达阈值
        b.on_failure(); // 达阈值 → open
        assert!(b.before_request().is_err());
        assert_eq!(b.status().state, BreakerState::Open);
        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request().is_ok()); // 冷却结束 → half open
        assert_eq!(b.status().state, BreakerState::HalfOpen);
        b.on_success();
        assert_eq!(b.status().state, BreakerState::Closed);
    }

    #[test]
    fn half_open_allows_single_probe() {
        let b = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 2,
            cooldown_secs: 1,
            enabled: true,
        });
        b.on_failure();
        b.on_failure(); // 达阈值 → open
        assert!(b.before_request().is_err());

        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request().is_ok()); // 冷却结束，首个请求获得探测名额
        assert!(b.before_request().is_err()); // 探测在途，其余请求仍被拦截

        b.on_failure(); // 探测失败 → 重新跳闸
        assert_eq!(b.status().state, BreakerState::Open);
        assert!(b.before_request().is_err());

        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request().is_ok()); // 新一轮探测
        b.on_success(); // 探测成功 → 恢复
        assert_eq!(b.status().state, BreakerState::Closed);
        assert!(b.before_request().is_ok()); // 恢复后正常放行
    }
}
