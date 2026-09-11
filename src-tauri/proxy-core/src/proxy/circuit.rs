use crate::proxy::config::CircuitBreakerConfig;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// 熔断器：CLOSED →（503 单次 / 连续失败达阈值）→ OPEN →（持续 `open_duration_secs`）
/// → HALF_OPEN（单名额试探，连续成功达恢复阈值）→ CLOSED（记录 `recovered_at`，慢启动用）。
///
/// # 设计要点
/// - **配置不驻留**：阈值/时长/窗口全部在方法调用时传入（动态生效，GUI 改参无需重建熔断器）
/// - **半开单名额**：`probing` 竞争探测名额，试探结果出来前其余请求仍被拦截
/// - **恢复阈值**：半开期需连续成功 `recovery_success_threshold` 次才关闭，流量一粒一粒放行，天然平滑
/// - **防抖动**：半开试探失败 → 重新 OPEN 完整时长重计；恢复后叠加慢启动（见 `lb.rs`）
/// - **Panic 名额**：全部候选熔断时按窗口限速放行试探（`try_claim_panic_probe`）
pub struct CircuitBreaker {
    state: AtomicU8,
    failures: AtomicU32,
    opened_at: Mutex<Option<Instant>>,
    /// 半开探测名额：true 表示已有试探请求在途
    probing: AtomicBool,
    /// 半开期连续成功计数（恢复阈值判定用）
    half_open_successes: AtomicU32,
    /// 最近一次转 Closed 的时刻（恢复慢启动用；手动 reset 后为 None = 立即全量）
    recovered_at: Mutex<Option<Instant>>,
    /// panic 名额最近被占用的时间戳（epoch 毫秒；0 表示从未占用）
    panic_probe_ms: AtomicI64,
}

// 注意：故意**不**为 CircuitBreaker 实现 Clone。
//
// 熔断器状态必须全局唯一共享（通过 `Arc<CircuitBreaker>`）。若允许 Clone，
// 得到的将是状态副本：`on_success()` / `on_failure()` 的写入不会反映到其它副本，
// 熔断机制会静默失效且编译器不会产生任何警告。
// 不实现 Clone 可以让编译器强制调用方只能通过 Arc 共享实例。

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(CLOSED),
            failures: AtomicU32::new(0),
            opened_at: Mutex::new(None),
            probing: AtomicBool::new(false),
            half_open_successes: AtomicU32::new(0),
            recovered_at: Mutex::new(None),
            panic_probe_ms: AtomicI64::new(0),
        }
    }

    /// 请求前调用；熔断打开且未到期时返回 `Err`。
    ///
    /// OPEN 到期后自动转入 HALF_OPEN 并竞争单探测名额。
    pub fn before_request(&self, cfg: &CircuitBreakerConfig) -> Result<(), BreakerOpen> {
        match self.state.load(Ordering::Relaxed) {
            CLOSED => Ok(()),
            // 半开：只放行一个试探请求
            HALF_OPEN => self.acquire_probe(),
            OPEN => {
                if self.is_open_expired(cfg) {
                    // 熔断期满 → 半开，竞争探测名额
                    self.state.store(HALF_OPEN, Ordering::Relaxed);
                    self.half_open_successes.store(0, Ordering::Relaxed);
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

    /// 候选是否可参与本轮选择：熔断器缺失/CLOSED/HALF_OPEN/OPEN-已到期（会转半开）。
    ///
    /// 供负载均衡在 eligible 子集上选择；CLOSED 恒为 true。
    pub fn is_eligible(&self, cfg: &CircuitBreakerConfig) -> bool {
        match self.state.load(Ordering::Relaxed) {
            OPEN => self.is_open_expired(cfg),
            _ => true,
        }
    }

    /// 503 专用：单次立即跳闸（不管当前状态、不管失败计数）。
    ///
    /// 已处于 OPEN 时保持原 `opened_at`（在途请求的迟到 503 不无限推后恢复时间）。
    pub fn trip_now(&self) {
        self.probing.store(false, Ordering::Relaxed);
        self.half_open_successes.store(0, Ordering::Relaxed);
        *self.recovered_at.lock() = None;
        if self.state.swap(OPEN, Ordering::Relaxed) != OPEN {
            *self.opened_at.lock() = Some(Instant::now());
        }
    }

    /// 成功上报：
    /// - CLOSED：清零失败计数
    /// - HALF_OPEN：连续成功 +1，达 `recovery_success_threshold` 才关闭并记 `recovered_at`
    /// - OPEN（panic 试探成功）：转 HALF_OPEN 并计入恢复计数（同样受恢复阈值约束）
    pub fn on_success(&self, cfg: &CircuitBreakerConfig) {
        match self.state.load(Ordering::Relaxed) {
            HALF_OPEN => {
                self.probing.store(false, Ordering::Relaxed);
                let n = self.half_open_successes.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= cfg.recovery_success_threshold.max(1) {
                    self.failures.store(0, Ordering::Relaxed);
                    self.half_open_successes.store(0, Ordering::Relaxed);
                    self.state.store(CLOSED, Ordering::Relaxed);
                    *self.recovered_at.lock() = Some(Instant::now());
                }
                // 未达阈值：保持 HALF_OPEN，释放名额让下一个试探请求进入
            }
            OPEN => {
                // panic 试探成功：OPEN → HALF_OPEN，计入恢复计数（count=1）
                self.state.store(HALF_OPEN, Ordering::Relaxed);
                self.probing.store(false, Ordering::Relaxed);
                self.half_open_successes.store(1, Ordering::Relaxed);
                if cfg.recovery_success_threshold.max(1) <= 1 {
                    self.failures.store(0, Ordering::Relaxed);
                    self.half_open_successes.store(0, Ordering::Relaxed);
                    self.state.store(CLOSED, Ordering::Relaxed);
                    *self.recovered_at.lock() = Some(Instant::now());
                }
            }
            _ => {
                // CLOSED：正常成功，清零失败计数
                self.failures.store(0, Ordering::Relaxed);
            }
        }
    }

    /// 失败上报（非 503 路径）：
    /// - CLOSED：连续失败 +1，达阈值跳闸
    /// - HALF_OPEN：试探失败 → 重新 OPEN，完整时长重计（防抖动）
    /// - OPEN：在途请求的迟到失败，不重置时钟
    ///
    /// 返回是否因此次失败而跳闸。
    pub fn on_failure(&self, cfg: &CircuitBreakerConfig) -> bool {
        self.probing.store(false, Ordering::Relaxed);
        match self.state.load(Ordering::Relaxed) {
            OPEN => true, // 已熔断：不动时钟
            HALF_OPEN => {
                self.retrip();
                true
            }
            _ => {
                let f = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
                if f >= cfg.failure_threshold.max(1) {
                    self.retrip();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// 重新跳闸：完整时长重计
    fn retrip(&self) {
        self.state.store(OPEN, Ordering::Relaxed);
        *self.opened_at.lock() = Some(Instant::now());
        self.half_open_successes.store(0, Ordering::Relaxed);
        *self.recovered_at.lock() = None;
    }

    /// 抢占 Panic 试探名额：`window_secs` 窗口内同一熔断器最多放行 1 个请求。
    ///
    /// 全部候选渠道熔断时的兜底：让"最早熔断"的渠道以受控频率放行真实请求，
    /// 既避免单次偶发 503 把唯一渠道硬拒整个熔断时长，又不会打死尚未恢复的上游。
    pub fn try_claim_panic_probe(&self, window_secs: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let last = self.panic_probe_ms.load(Ordering::Acquire);
        if now.saturating_sub(last) < (window_secs as i64) * 1000 {
            return false;
        }
        self.panic_probe_ms
            .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// 手动重置（GUI）：立即恢复全量流量，不触发慢启动。
    pub fn reset(&self) {
        self.failures.store(0, Ordering::Relaxed);
        self.half_open_successes.store(0, Ordering::Relaxed);
        self.state.store(CLOSED, Ordering::Relaxed);
        self.probing.store(false, Ordering::Relaxed);
        *self.opened_at.lock() = None;
        *self.recovered_at.lock() = None;
    }

    pub fn status(&self) -> BreakerStatus {
        BreakerStatus {
            state: self.state.load(Ordering::Relaxed).into(),
            consecutive_failures: self.failures.load(Ordering::Relaxed),
        }
    }

    /// 跳闸时刻（Panic 模式选"最早熔断者"用）；未熔断时为 `None`
    pub fn opened_at(&self) -> Option<Instant> {
        *self.opened_at.lock()
    }

    /// 最近一次恢复（HALF_OPEN→CLOSED）时刻，恢复慢启动用；
    /// 手动 reset 后为 `None`（立即全量恢复）
    pub fn recovered_at(&self) -> Option<Instant> {
        *self.recovered_at.lock()
    }

    /// OPEN 状态且熔断时长已到（可转半开试探）
    fn is_open_expired(&self, cfg: &CircuitBreakerConfig) -> bool {
        self.state.load(Ordering::Relaxed) == OPEN
            && self
                .opened_at
                .lock()
                .map(|t| t.elapsed() >= Duration::from_secs(cfg.open_duration_secs))
                .unwrap_or(false)
    }

    /// 熔断冷却剩余秒数。
    ///
    /// - 未熔断 / 已到期 → `Some(0)`
    /// - 处于 Open 且仍在冷却中 → `Some(剩余秒数)`
    /// - 熔断未启用 → `None`（无冷却概念）
    pub fn cooldown_remaining_secs(&self, cfg: &CircuitBreakerConfig) -> Option<u64> {
        if !cfg.enabled {
            return None;
        }
        if self.state.load(Ordering::Relaxed) != OPEN {
            return Some(0);
        }
        let elapsed = self.opened_at.lock().map(|t| t.elapsed());
        match elapsed {
            Some(e) => {
                let total = Duration::from_secs(cfg.open_duration_secs);
                let remaining = total.saturating_sub(e);
                // 向上取整：刚跳闸时显示完整秒数，而非因微秒级耗时少 1
                Some(remaining.as_secs() + if remaining.subsec_nanos() > 0 { 1 } else { 0 })
            }
            None => Some(0),
        }
    }

    // ── 探活任务专用 ──

    /// 探活任务入口：OPEN-到期或 HALF_OPEN 时抢占探测名额（CLOSED 不需要探活）。
    ///
    /// 返回 `true` 表示抢占成功，探活方获得一次试探机会（结果通过
    /// `on_success` / `probe_failed` 上报）。
    pub fn try_acquire_probe_slot(&self, cfg: &CircuitBreakerConfig) -> bool {
        if self.state.load(Ordering::Relaxed) == CLOSED {
            return false;
        }
        self.before_request(cfg).is_ok()
    }

    /// 探活失败：退回 OPEN 但**保持原熔断时刻**（不重置时钟），
    /// 下一轮探活可立即重试，符合"到期后反复探活直到成功"的语义。
    pub fn probe_failed(&self) {
        self.probing.store(false, Ordering::Relaxed);
        if self.state.load(Ordering::Relaxed) == HALF_OPEN {
            self.state.store(OPEN, Ordering::Relaxed);
            // opened_at 不动：保持原计时
            self.half_open_successes.store(0, Ordering::Relaxed);
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct BreakerOpen;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(threshold: u32, open_secs: u64, recovery: u32) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            enabled: true,
            failure_threshold: threshold,
            open_duration_secs: open_secs,
            probe_interval_secs: 30,
            recovery_success_threshold: recovery,
            panic_probe_window_secs: 60,
        }
    }

    #[test]
    fn trip_now_opens_immediately() {
        // 503 语义：单次立即跳闸，无论失败计数
        let b = CircuitBreaker::new();
        let c = cfg(5, 3600, 2);
        assert!(b.before_request(&c).is_ok());
        b.trip_now();
        assert_eq!(b.status().state, BreakerState::Open);
        assert!(b.before_request(&c).is_err());
        assert_eq!(b.cooldown_remaining_secs(&c), Some(3600));
    }

    #[test]
    fn opens_after_threshold_and_recovers() {
        let b = CircuitBreaker::new();
        let c = cfg(3, 1, 1);
        b.on_failure(&c);
        b.on_failure(&c);
        assert!(b.before_request(&c).is_ok()); // 未达阈值
        b.on_failure(&c); // 达阈值 → open
        assert!(b.before_request(&c).is_err());
        assert_eq!(b.status().state, BreakerState::Open);
        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request(&c).is_ok()); // 到期 → half open
        assert_eq!(b.status().state, BreakerState::HalfOpen);
        b.on_success(&c);
        assert_eq!(b.status().state, BreakerState::Closed);
    }

    #[test]
    fn recovery_requires_consecutive_successes() {
        // 恢复阈值 = 2：半开后需连续成功 2 次才关闭
        let b = CircuitBreaker::new();
        let c = cfg(2, 1, 2);
        b.on_failure(&c);
        b.on_failure(&c); // → open
        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request(&c).is_ok()); // 半开，拿到名额
        b.on_success(&c); // 第 1 次成功：仍半开
        assert_eq!(b.status().state, BreakerState::HalfOpen);
        assert!(b.before_request(&c).is_ok()); // 名额已释放，可再试探
        b.on_success(&c); // 第 2 次成功：关闭 + 记 recovered_at
        assert_eq!(b.status().state, BreakerState::Closed);
        assert!(
            b.recovered_at().is_some(),
            "关闭时应记录恢复时刻（慢启动用）"
        );
    }

    #[test]
    fn half_open_failure_retrips_full_duration() {
        // 半开试探失败 → 重新 OPEN 且完整时长重计（防抖动）
        let b = CircuitBreaker::new();
        let c = cfg(2, 1, 2);
        b.on_failure(&c);
        b.on_failure(&c); // → open
        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request(&c).is_ok()); // 半开
        b.on_success(&c); // count=1，仍半开
        b.on_failure(&c); // 试探失败 → 重新跳闸，时钟重置
        assert_eq!(b.status().state, BreakerState::Open);
        assert_eq!(b.cooldown_remaining_secs(&c), Some(1), "失败后完整重计");
        assert!(b.before_request(&c).is_err());
    }

    #[test]
    fn half_open_allows_single_probe() {
        let b = CircuitBreaker::new();
        let c = cfg(2, 1, 1);
        b.on_failure(&c);
        b.on_failure(&c); // → open
        assert!(b.before_request(&c).is_err());

        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request(&c).is_ok()); // 首个请求获得探测名额
        assert!(b.before_request(&c).is_err()); // 探测在途，其余请求仍被拦截

        b.on_failure(&c); // 探测失败 → 重新跳闸
        assert_eq!(b.status().state, BreakerState::Open);
        assert!(b.before_request(&c).is_err());

        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request(&c).is_ok()); // 新一轮探测
        b.on_success(&c); // 探测成功 → 恢复
        assert_eq!(b.status().state, BreakerState::Closed);
        assert!(b.before_request(&c).is_ok()); // 恢复后正常放行
    }

    #[test]
    fn panic_probe_window_blocks_second_claim() {
        let b = CircuitBreaker::new();
        assert!(b.try_claim_panic_probe(60), "首次抢占应成功");
        assert!(!b.try_claim_panic_probe(60), "窗口内二次抢占应被拒（限速）");
    }

    #[test]
    fn panic_success_transitions_to_half_open() {
        // panic 试探成功（绕过 before_request，熔断器仍 OPEN）：
        // OPEN → HALF_OPEN 且计入恢复计数
        let b = CircuitBreaker::new();
        let c = cfg(5, 3600, 2);
        b.trip_now();
        b.on_success(&c); // panic 成功：count=1，仍半开
        assert_eq!(b.status().state, BreakerState::HalfOpen);
        b.on_success(&c); // count=2 ≥ 阈值 → 关闭
        assert_eq!(b.status().state, BreakerState::Closed);
    }

    #[test]
    fn dynamic_config_takes_effect_immediately() {
        // 熔断器不持有配置副本：改 open_duration / panic_window 参数即时生效
        let b = CircuitBreaker::new();
        let one_hour = cfg(2, 3600, 1);
        let one_sec = cfg(2, 1, 1);
        b.on_failure(&one_sec);
        b.on_failure(&one_sec); // → open

        // 同一实例：1 小时时长下未到期，1 秒时长下已到期
        assert!(b.before_request(&one_hour).is_err());
        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.before_request(&one_sec).is_ok());
        b.on_success(&one_sec);
        assert_eq!(b.status().state, BreakerState::Closed);

        // panic 窗口同理：60s 窗口内被拒后，改用 0 窗口立即放行
        b.trip_now();
        assert!(b.try_claim_panic_probe(60));
        assert!(!b.try_claim_panic_probe(60));
        assert!(b.try_claim_panic_probe(0), "窗口参数动态生效");
    }

    #[test]
    fn probe_slot_and_probe_failed() {
        // 探活任务：CLOSED 不需要探活；OPEN-未到期抢不到名额；
        // 到期抢到后失败 → 退回 OPEN 且不重置时钟（下轮可立即重试）
        let b = CircuitBreaker::new();
        let c = cfg(2, 1, 1);
        assert!(!b.try_acquire_probe_slot(&c), "CLOSED 不探活");

        b.on_failure(&c);
        b.on_failure(&c); // → open
        assert!(!b.try_acquire_probe_slot(&c), "未到期抢不到");

        std::thread::sleep(Duration::from_millis(1100));
        assert!(b.try_acquire_probe_slot(&c), "到期抢占成功");
        assert!(!b.try_acquire_probe_slot(&c), "名额被占，二次抢占失败");

        b.probe_failed(); // 探活失败：退回 OPEN，保持原时钟
        assert_eq!(b.status().state, BreakerState::Open);
        assert!(
            b.try_acquire_probe_slot(&c),
            "未重置时钟 → 仍视为到期，下轮立即可再试"
        );
    }
}
