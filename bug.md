# 负载均衡模块 Bug 清单

> 审查范围：`src-tauri/proxy-core/src/proxy/` 下的负载均衡相关代码
> 涉及文件：`lb.rs`、`api.rs`、`config.rs`、`state.rs`、`circuit.rs`、`store.rs`、`mod.rs`
> 审查日期：2026-08-28
> **状态：全部 11 项已修复并通过测试（18/18）**

---

## 目录

- [P0 严重 Bug](#p0-严重-bug)
  - [BUG-01 流式响应时 in_flight 计数不归还，least_connections 策略完全失效](#bug-01-流式响应时-in_flight-计数不归还least_connections-策略完全失效)
  - [BUG-02 FastestResponse 策略饥饿，新上游永远无法冷启动](#bug-02-fastestresponse-策略饥饿新上游永远无法冷启动)
  - [BUG-03 weighted_random 随机源劣质且权重累加 u32 溢出](#bug-03-weighted_random-随机源劣质且权重累加-u32-溢出)
- [P1 逻辑错误](#p1-逻辑错误)
  - [BUG-04 熔断器按 provider.id 共享，跨 model 误伤](#bug-04-熔断器按-providerid-共享跨-model-误伤)
  - [BUG-05 failover 重置重试计数，单请求最坏挂起约 100 秒](#bug-05-failover-重置重试计数单请求最坏挂起约-100-秒)
  - [BUG-06 CircuitBreaker::clone 深拷贝陷阱](#bug-06-circuitbreakerclone-深拷贝陷阱)
  - [BUG-07 round_robin 计数器按 model 共享，候选数变化时分配错乱](#bug-07-round_robin-计数器按-model-共享候选数变化时分配错乱)
- [P2 次要问题](#p2-次要问题)
  - [BUG-08 weight = 0 校验提示与代码实际行为矛盾](#bug-08-weight--0-校验提示与代码实际行为矛盾)
  - [BUG-09 sticky 清理任务 TTL 固化，改配置不生效且无法退出](#bug-09-sticky-清理任务-ttl-固化改配置不生效且无法退出)
  - [BUG-10 sticky_sessions 过期条目延迟回收，内存可能膨胀](#bug-10-sticky_sessions-过期条目延迟回收内存可能膨胀)
  - [BUG-11 dec_in_flight 无下溢保护](#bug-11-dec_in_flight-无下溢保护)
- [附录：修复优先级建议](#附录修复优先级建议)

---

## P0 严重 Bug

### BUG-01 流式响应时 in_flight 计数不归还，least_connections 策略完全失效

**位置**：`src-tauri/proxy-core/src/proxy/api.rs:487-510`、`lb.rs:221-234`、`lb.rs:291-304`

**问题描述**

`route_and_call` 在调用上游前 `inc_in_flight`，在 `call_upstream` 返回后立刻 `dec_in_flight`。但流式（`stream: true`）场景下，`call_upstream` 在**收到响应头时**就返回 `UpstreamOutcome::Stream(resp)`，此时 SSE 流仍在持续传输（可达数十秒），而计数已经在匹配分支中减掉了。

后续流由 `forward_sse` 接管，整个流式生命周期内 in_flight 恒为 0。

**相关代码**

```483:497:src-tauri/proxy-core/src/proxy/api.rs
        let inflight_key = lb::route_key(model, &provider.id, upstream_model_str);
        lb::inc_in_flight(state, &inflight_key);
        let call_started = Instant::now();

        match call_upstream(state, provider, path, body.clone()).await {
            Ok(outcome) => {
                // 成功：记录延迟样本，返回结果
                breaker.on_success();
                lb::dec_in_flight(state, &inflight_key);
                lb::record_latency(
                    state,
                    &inflight_key,
                    call_started.elapsed().as_millis() as u64,
                );
```

```221:234:src-tauri/proxy-core/src/proxy/lb.rs
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
```

**影响**

- `least_connections` 策略下所有候选的 in_flight 恒为 0，`min_by_key` 永远返回索引 0 → **退化为"永远选第一个候选"**，完全丧失负载均衡能力。
- 该策略在流式为主的实际使用场景中（LLM 对话几乎全是流式）等于完全不可用。

**修复建议**

将 `dec_in_flight` 推迟到流真正消费结束之后执行：

1. 在 `Ok(UpstreamOutcome::Stream(_))` 分支中**不调用** `dec_in_flight`；
2. 把 `inflight_key` 传入 `forward_sse`，在流结束（或异常终止）的清理路径上调用 `dec_in_flight`；
3. 确保流式路径的所有早退分支（客户端断连、上游错误）都有对应的归还逻辑，推荐使用 RAII guard 结构体在 `Drop` 中归还。

---

### BUG-02 FastestResponse 策略饥饿，新上游永远无法冷启动

**位置**：`src-tauri/proxy-core/src/proxy/lb.rs:205-218`、`state.rs:237-274`

**问题描述**

`fastest_response` 用 EWMA 延迟做 `min_by_key`，无样本的候选延迟视为 `u64::MAX`。这带来两个互斥的严重问题：

1. **新上游饿死**：新加入的上游从未成功过 → EWMA 为 `u64::MAX` → 永远不会被选中 → 永远拿不到样本 → **永远无法冷启动**，即使它是当前唯一健康的候选。
2. **旧样本冻结**：一旦某候选成为最快，它独吞所有流量并持续刷新自己的 EWMA；其它候选因断流，EWMA 永久冻结在陈旧值上。当"最快"的候选变慢后，系统可能错误地切到一个状态未知的陈旧候选。

**相关代码**

```205:218:src-tauri/proxy-core/src/proxy/lb.rs
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
                .unwrap_or(u64::MAX)
        })
        .map(|(i, _)| i)
}
```

```252:263:src-tauri/proxy-core/src/proxy/state.rs
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
```

注意 `RouteLatencyStats` 已有 `samples` 字段和 `sample_count()` 方法，但 `fastest_response` **完全没有使用它**，冷启动问题本可缓解却未处理。

**影响**

- 新增/恢复的上游无法自动接入流量，需要重启进程清空 `route_latency`。
- 上游性能变化后，负载均衡决策基于陈旧数据，可能长期错误。

**修复建议**

1. 给 `RouteLatencyStats` 增加 `last_sample_at: AtomicU64`（秒级时间戳）；
2. `fastest_response` 中：无样本或样本过久未更新（如超过 5 分钟）的候选，按**乐观初始值**处理（例如取所有已知延迟的中位数，或一个较小的默认基线），而非 `u64::MAX`；
3. 引入少量探测流量（如 1% 概率随机选非最优候选），保证 EWMA 持续刷新。

---

### BUG-03 weighted_random 随机源劣质且权重累加 u32 溢出

**位置**：`src-tauri/proxy-core/src/proxy/lb.rs:181-199`、`lb.rs:334-339`

**问题描述**

三个独立缺陷叠加在同一个函数：

1. **随机源退化**：`system_nanos()` 只返回 `subsec_nanos()`，取值范围 `0..1_000_000_000`。在时间分辨率不足（Windows 上常见，时钟粒度约 15.6ms）或高并发场景下，多个请求会取到**完全相同的随机值** → 加权分布严重退化，极端情况下连续 N 个请求全部打到同一个上游。
2. **u32 溢出**：`let total: u32 = weights.iter().sum();` 使用 `u32` 累加，权重较大或候选较多时会**静默溢出**（release 模式 wrap，debug 模式 panic）。
3. **截断降质**：`(system_nanos() as u128) as u32` 的 `as` 转换直接丢弃高位，进一步破坏随机性。

**相关代码**

```181:199:src-tauri/proxy-core/src/proxy/lb.rs
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
```

```334:339:src-tauri/proxy-core/src/proxy/lb.rs
fn system_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u128)
        .unwrap_or(0)
}
```

**影响**

- 加权策略的实际流量分布严重偏离配置权重，无法达到预期的分流比例。
- 高权重配置下可能 panic 或行为完全错乱。

**修复建议**

1. 引入 `rand`（或轻量 `fastrand`）crate 替代 `system_nanos()`，`random_index` 一并替换；
2. `total` 改用 `u64` 累加：`let total: u64 = weights.iter().map(|&w| w as u64).sum();`；
3. 随机数生成改用 `rng.gen_range(0..total)`。

---

## P1 逻辑错误

### BUG-04 熔断器按 provider.id 共享，跨 model 误伤

**位置**：`src-tauri/proxy-core/src/proxy/api.rs:456-460`、`state.rs:303-304`

**问题描述**

熔断器的 `DashMap` key 仅为 `provider.id`。若某个 provider 在 model X 上连续失败达到 `failure_threshold` 次而跳闸，那么**该 provider 上的所有其它 model（Y、Z...）会被一起熔断**，即使它们完全健康。

同时 `breakers` map 也**永不清理**，条目数量随 provider 数增长（虽然 provider 数量通常有限，影响较小）。

**相关代码**

```456:460:src-tauri/proxy-core/src/proxy/api.rs
        let breaker = state
            .breakers
            .entry(provider.id.clone())
            .or_insert_with(|| Arc::new(CircuitBreaker::new(breaker_cfg.clone())))
            .clone();
```

若同一 provider 通过不同 `upstream_model` 配置了多条路由，问题会进一步加剧（同一 model 的不同路由也共享熔断器）。

**影响**

- 单一模型故障会拖垮整个 provider 上的所有模型，故障域被不必要地放大。
- 与"多路由负载均衡"的设计初衷冲突：本应故障隔离，实际却耦合。

**修复建议**

将 key 改为路由维度，与 `lb::route_key` 保持一致：

```rust
let breaker_key = lb::route_key(model, &provider.id, upstream_model_str);
let breaker = state
    .breakers
    .entry(breaker_key)
    .or_insert_with(|| Arc::new(CircuitBreaker::new(breaker_cfg.clone())))
    .clone();
```

若希望保留"provider 级整体故障"的检测能力，可采用两级熔断器（provider 级 + route 级），但需注意两者状态同步。

---

### BUG-05 failover 重置重试计数，单请求最坏挂起约 100 秒

**位置**：`src-tauri/proxy-core/src/proxy/api.rs:463-476`、`api.rs:533-552`

**问题描述**

每次 failover 到下一候选时都执行 `attempt = 0`，重置重试计数。配置 `max_retries = 3`（默认）时：

- 每个候选最多尝试 `max_retries + 1 = 4` 次；
- 3 个候选最坏情况：`3 × 4 = 12` 次上游调用；
- 每次重试有指数退避，`base_delay_ms = 500`、`max_delay_ms = 8000`，退避序列约 `0.5s + 1s + 2s = 3.5s`/候选 → 退避总计约 `10.5s`；
- 叠加每次上游调用本身的超时（`timeout_secs = 300` 默认），**单请求理论最坏耗时可达数分钟**。

**相关代码**

```539:552:src-tauri/proxy-core/src/proxy/api.rs
                if candidates_tried < total_candidates {
                    warn!(
                        provider = %provider.id,
                        model = %model,
                        error = %e,
                        "exhausted retries, failing over to next candidate"
                    );
                    current_idx = (current_idx + 1) % total_candidates;
                    candidates_tried += 1;
                    attempt = 0;
                    continue;
                }
                return Err(e);
```

```156:165:src-tauri/proxy-core/src/proxy/config.rs
impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 500,
            max_delay_ms: 8000,
```

**影响**

- 客户端（如 Claude Code、各类 IDE 插件）通常有自己的超时（30-60s），代理长时间挂起会导致客户端先超时断开，代理侧的工作全部白费。
- 上游全面故障时，大量请求堆积在重试/退避中，占用连接与内存资源。

**修复建议**

1. 在 `route_and_call` 入口记录 `deadline = Instant::now() + total_timeout`，每次重试/failover 前检查是否超时，超时立即返回错误；
2. 或改为"全局总尝试次数上限"，而非"每候选独立计数"；
3. 考虑将 failover 时的退避与重试退避区分开（failover 到健康候选时无需长时间等待）。

---

### BUG-06 CircuitBreaker::clone 深拷贝陷阱

**位置**：`src-tauri/proxy-core/src/proxy/circuit.rs:46-56`、`state.rs:303-304`

**问题描述**

`CircuitBreaker` 手写实现了 `Clone`，将 `state` / `failures` / `probing` / `opened_at` 的**当前值快照**拷贝进新对象，得到的是一个**状态副本**。

而 `state.rs:303` 的注释声称：

```303:304:src-tauri/proxy-core/src/proxy/state.rs
    /// 每个 Provider 一个熔断器；Arc 共享实例，避免 clone 深拷贝导致状态不同步
    pub breakers: Arc<DashMap<String, Arc<CircuitBreaker>>>,
```

**当前运行时行为是正确的** —— `api.rs:459` 的 `.clone()` 作用于 `Arc<CircuitBreaker>`，是浅拷贝（引用计数 +1），所有请求确实共享同一实例。

但手写的 `Clone` impl 是一个**危险的陷阱**：将来任何人直接 clone `CircuitBreaker` 值（而非 `Arc`）就会得到独立副本，`on_failure()` / `on_success()` 的写入不会反映到其它副本，熔断机制静默失效，且**编译器不会给出任何警告**。

**相关代码**

```46:56:src-tauri/proxy-core/src/proxy/circuit.rs
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
```

**影响**

- 当前无实际故障，属潜在风险。
- 一旦有人在重构中改为 `DashMap<String, CircuitBreaker>`（去掉 `Arc`），熔断会静默失效且极难排查。

**修复建议**

删除手写的 `Clone` impl（也**不要**加 `#[derive(Clone)]`），让编译器强制调用方只能通过 `Arc` 共享。若某些场景确实需要快照语义，改为显式的 `fn snapshot(&self) -> BreakerSnapshot` 方法。

---

### BUG-07 round_robin 计数器按 model 共享，候选数变化时分配错乱

**位置**：`src-tauri/proxy-core/src/proxy/lb.rs:163-170`、`state.rs:314-315`

**问题描述**

1. **模数变化导致错乱**：计数器 key 仅为 `model`，每次调用 `fetch_add(1) % len`。配置热重载后若某 model 的候选数从 3 变为 2，计数器的模数随之改变，轮询序列瞬间错乱；若候选列表**顺序**在 reload 后变化（路由增删改序），轮询会跳到任意位置，短时间内分配严重不均。
2. **内存永不释放**：`lb_counters` 中的条目**没有任何清理逻辑**，每个使用过的 model 都会留下一个 `DashMap` 条目，长期运行随 model 数量单调增长。

**相关代码**

```163:170:src-tauri/proxy-core/src/proxy/lb.rs
fn round_robin(state: &AppState, model: &str, len: usize) -> Option<usize> {
    let counter = state
        .lb_counters
        .entry(model.to_string())
        .or_insert_with(|| std::sync::atomic::AtomicU64::new(0));
    let val = counter.fetch_add(1, Ordering::Relaxed);
    Some((val as usize) % len)
}
```

**影响**

- 配置热重载（GUI 保存后立即生效）后，轮询分配可能短期失衡。
- `lb_counters` 无界增长（每个条目仅几十字节，实际影响有限，但属于明确的资源泄漏）。

**修复建议**

1. 计数器 key 加入候选数：`format!("{model}#{len}")`，候选数变化时自然切换到新序列；
2. 或改用平滑加权轮询（SWRR），对权重和候选变化更鲁棒；
3. 在 sticky 清理任务中一并清理长时间未更新的 `lb_counters` 条目（可记录 `last_used`）。

---

## P2 次要问题

### BUG-08 weight = 0 校验提示与代码实际行为矛盾

**位置**：`src-tauri/proxy-core/src/proxy/store.rs:88-93`、`lb.rs:181-185`

**问题描述**

校验代码拒绝 `weight == 0`，提示"负载均衡加权策略下会被忽略"。但 `weighted_random` 的实现中，weight = 0 的候选**并非"被忽略"**，而是选中概率为 0（累加区间为空），效果上确实等同跳过——但提示语不准确，且这阻止了用户配置"仅作为备用、不参与常规流量"的路由这一合理需求。

此外，隐式候选（`resolve_candidates` 中 route 为 `None` 的情况）的 weight 被**硬编码为 1**，用户无法通过配置调整。

**相关代码**

```88:93:src-tauri/proxy-core/src/proxy/store.rs
        if r.weight == 0 {
            return Err(format!(
                "路由 '{}' 的 weight 不能为 0（负载均衡加权策略下会被忽略）",
                r.model
            ));
        }
```

```182:185:src-tauri/proxy-core/src/proxy/lb.rs
    let weights: Vec<u32> = candidates
        .iter()
        .map(|c| c.route.map(|r| r.weight).unwrap_or(1))
        .collect();
```

**影响**

- 功能限制：无法配置"纯备用"路由。
- 文案误导用户。

**修复建议**

允许 `weight == 0`（表示不参与常规分发，仅在 failover 时作为兜底），并修正校验提示文案；或保持禁止但将提示改为"weight 为 0 的路由将永远不会被加权策略选中"。

---

### BUG-09 sticky 清理任务 TTL 固化，改配置不生效且无法退出

**位置**：`src-tauri/proxy-core/src/proxy/mod.rs:36-45`、`lb.rs:318-329`

**问题描述**

1. **TTL 固化**：`serve()` 启动时读取一次 `sticky.ttl_secs` 传给清理任务。之后 GUI 修改 TTL 并热重载配置，清理任务仍使用**启动时的旧 TTL**，导致 UI 显示与实际行为不一致。
2. **无退出机制**：`tokio::spawn` 的 `loop` 没有 `CancellationToken` 或 `JoinHandle` 保存，服务器关闭后任务仍继续运行（虽然是后台任务，在进程退出时会被终止，但在同一进程内重启服务的场景下会残留多个清理任务）。

**相关代码**

```36:45:src-tauri/proxy-core/src/proxy/mod.rs
    let (host, port, sticky_ttl) = {
        let cfg = state.config.read();
        (
            cfg.server.host.clone(),
            cfg.server.port,
            cfg.load_balance.sticky.ttl_secs,
        )
    };
    // 启动 sticky 会话清理后台任务
    lb::spawn_sticky_cleanup(state.clone(), sticky_ttl);
```

```318:329:src-tauri/proxy-core/src/proxy/lb.rs
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
```

**影响**

- 修改粘性 TTL 后不生效（除非重启应用），与"所有修改即时生效"的 UI 承诺冲突。
- 重复启动服务会累积清理任务。

**修复建议**

1. 清理任务内每次循环从 `state.config.read()` 动态读取当前 TTL；
2. 保存 `JoinHandle`，在服务关闭时 `abort()`；或传入 `CancellationToken` 控制退出。

---

### BUG-10 sticky_sessions 过期条目延迟回收，内存可能膨胀

**位置**：`src-tauri/proxy-core/src/proxy/lb.rs:93-120`、`lb.rs:318-329`

**问题描述**

`lookup_sticky` 检测到条目过期时仅 `return None`，**不删除条目**；过期条目要等到 60 秒一次的清理任务才被回收。

在高并发 + 短会话场景下（例如 `session_key_source = messages_hash`，每轮对话的 messages 都不同 → 每个请求一个唯一 key），`sticky_sessions` 会以请求速率持续膨胀，60 秒内可积累大量条目。

**相关代码**

```100:113:src-tauri/proxy-core/src/proxy/lb.rs
    let entry = state.sticky_sessions.get(session_key)?;
    // 校验未过期
    if entry.created_at.elapsed().as_secs() >= lb_cfg.sticky.ttl_secs {
        return None;
    }
    // 校验候选索引仍有效（配置变更后候选数可能不同）
    if entry.candidate_index >= candidates.len() {
        return None;
    }
    // 校验路由标识未变（配置调整或请求模型不同都会导致索引指向错误路线）
    let expected = route_key_for_candidate(model, &candidates[entry.candidate_index]);
    if entry.route_key != expected {
        return None;
    }
```

**影响**

- 内存随请求速率膨胀（每条 `StickyEntry` 含一个 `String` route_key + `Instant`，约 60-80 字节）。
- 60 秒窗口内在极端 QPS 下可能积累数十万条目。

**修复建议**

在 `lookup_sticky` 的三个失效分支中调用 `state.sticky_sessions.remove(session_key)` 立即回收。注意 DashMap 的 `get` 返回引用期间不可直接 `remove`（会死锁），需先 `drop(entry)` 再 remove，或改用 `remove_if`。

---

### BUG-11 dec_in_flight 无下溢保护

**位置**：`src-tauri/proxy-core/src/proxy/lb.rs:300-304`

**问题描述**

`fetch_sub(1)` 无下溢保护。若因异常路径或未来的代码改动导致多减一次（如 BUG-01 修复时重复归还），`AtomicU64` 会 wrap 成 `u64::MAX`，该候选在 `least_connections` 中永远被视为"最忙"，被永久排除。

**相关代码**

```300:304:src-tauri/proxy-core/src/proxy/lb.rs
pub fn dec_in_flight(state: &AppState, key: &str) {
    if let Some(counter) = state.in_flight.get(key) {
        counter.fetch_sub(1, Ordering::Relaxed);
    }
}
```

**影响**

- 当前路径下不易触发（`inc` 与 `dec` 配对），属防御性问题。
- 一旦触发，后果严重且难以排查（该候选被静默永久跳过）。

**修复建议**

改用饱和减法：

```rust
pub fn dec_in_flight(state: &AppState, key: &str) {
    if let Some(counter) = state.in_flight.get(key) {
        counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            Some(v.saturating_sub(1))
        }).ok();
    }
}
```

---

## 附录 A-2：请求日志错误详情增强

**问题**：日志详情弹窗只展示一句扁平文本（如 `circuit breaker open for provider '日日新2'`），
用户无法判断"为什么会熔断、要等多久、该改哪里"。

**方案**：新增结构化错误详情，把错误拆解为四部分。

### 后端

- `retry.rs` 新增 `ErrorKind`（错误大类）、`ErrorDetail`（结构化详情）、`ErrorContext`（请求上下文）
- `ProxyError` 增加三个方法：
  - `kind()` — 错误分类（前端据此选标题与配色）
  - `summary()` — 一句话说明"发生了什么"
  - `hint()` — 面向用户的排查建议（按状态码细分：401/403 查 Key、429 限流、5xx 上游异常…）
  - `raw_detail()` — 原始错误，配合 `format_upstream_body()` 从各家厂商的错误体中提取 `message`/`type`/`code`
- `RequestLog` 新增 `error_detail` 字段（`Option<ErrorDetail>`，`None` 时不序列化）
- `CircuitBreaker::cooldown_remaining_secs()` — 报告熔断冷却剩余秒数
- `api.rs` 在每个失败出口收集上下文（渠道名/ID、上游模型、端点、已试渠道数、重试次数、连续失败数、冷却剩余）

携带的上下文字段：

| 字段 | 说明 |
|------|------|
| `kind` / `summary` / `detail` / `hint` | 分类、摘要、原始错误、建议 |
| `provider` / `provider_id` / `upstream_model` / `endpoint` | 出错位置 |
| `upstream_status` | 上游 HTTP 状态码 |
| `candidates_tried` / `total_candidates` / `retries` | failover 与重试过程 |
| `consecutive_failures` / `cooldown_remaining_secs` | 熔断状态与恢复时间 |

另外，`RouteNotFound` 现在区分"完全没配这个模型"与"配了但渠道被禁用/路由被禁用"，提示更精确。

### 前端

- `types.ts` 新增 `ErrorKind` / `ErrorDetail`，`RequestLog` 增加 `error_detail`
- `LogDetail.tsx` 新增 `ErrorPanel` 组件，分块渲染：
  - 头部：错误大类标题 + 复制按钮（复制的是格式化后的完整诊断信息）
  - 摘要
  - 原始错误（等宽字体、可滚动、自动换行）
  - 上下文网格（只渲染有值的行，避免满屏「—」）
  - 排查建议（灯泡图标 + 弱化底色）
- 兼容旧数据：无 `error_detail` 时回退到原来的扁平字符串展示

### 测试

- `error_detail_carries_context` — 熔断详情带渠道/连续失败/冷却剩余
- `error_detail_extracts_upstream_status_and_message` — 从上游错误体提取 message 与 code
- `format_upstream_body_handles_shapes` — OpenAI 风格 / error 为字符串 / 顶层 message / 兜底 JSON
- `smoke_full_chain` 增加端到端断言：熔断请求的日志确实带有结构化详情

---

## 附录 A：修复记录

全部 11 项已修复，`cargo test` 18/18 通过，`cargo clippy` 无新增告警。

| 编号 | 修复内容 | 改动文件 |
|------|---------|---------|
| BUG-01 | 新增 `InFlightGuard` RAII guard；流式请求经 `UpstreamOutcome::StreamWithGuard` 把 guard 转移给 `forward_sse`，在流结束时归还。非流式路径提前 `release()` | `lb.rs`、`api.rs`、`upstream.rs` |
| BUG-02 | `RouteLatencyStats` 增加 `last_sample_at` + `is_fresh()`（TTL 300s）；无新鲜样本的候选改用"乐观基线"（最新鲜最小延迟 × 0.8）而非 `u64::MAX`；全部无样本时随机选择 | `lb.rs`、`state.rs` |
| BUG-03 | 引入 `rand` crate 替换 `system_nanos()`；权重累加改 `u64`；`random_index` / `weighted_random` 均用 `thread_rng().gen_range()` | `lb.rs`、`Cargo.toml` |
| BUG-04 | 熔断 key 由 `provider.id` 改为 `lb::route_key(model, provider, upstream_model)` | `api.rs`、`smoke_tests.rs` |
| BUG-05 | 新增 `TOTAL_REQUEST_TIMEOUT_SECS = 120` 全局 deadline；重试前预估退避后是否超时，failover 前检查剩余时间，超时返回 `ProxyError::Timeout` | `api.rs` |
| BUG-06 | 删除 `CircuitBreaker` 的手写 `Clone` impl，改为注释说明为何不实现 | `circuit.rs` |
| BUG-07 | 轮询 key 改为 `{model}#{len}`（候选数变化自动切换序列）；清理任务中附带回收 `lb_counters`（上限 1024） | `lb.rs` |
| BUG-08 | 允许 `weight = 0`（纯备用/failover 兜底路由），仅拦截 `weight > 1_000_000` | `store.rs` |
| BUG-09 | `spawn_sticky_cleanup` 去掉 TTL 参数，每轮从 `state.config` 动态读取；返回 `JoinHandle` 供服务退出时 `abort()`；`serve` 与 `serve_cancellable` 均已接入 | `lb.rs`、`mod.rs` |
| BUG-10 | `lookup_sticky` 三个失效分支改为立即 `remove` 回收（先判定再删除，避免 DashMap 持锁死锁） | `lb.rs` |
| BUG-11 | `dec_in_flight` 改用 `fetch_update` + `saturating_sub(1)` | `lb.rs` |

**新增测试**：

- `fastest_response_allows_cold_start` — 无样本候选能获得冷启动机会，产生样本后恢复按延迟选择
- `weighted_random_respects_weights` — 权重 9:1 下 4000 次采样，比例落在 0.85~0.95
- `dec_in_flight_never_underflows` — 重复归还不下溢

**顺带修复**：`src-tauri/examples/proxy-server.rs` 缺少 `weight` / `disabled` 字段导致的编译失败（既有问题，与本次改动无关）。

---

## 附录 B：修复优先级建议

| 编号 | 优先级 | 问题 | 修复复杂度 | 影响面 |
|------|--------|------|-----------|--------|
| BUG-01 | **P0** | 流式 in_flight 不归还 | 中（需改 `forward_sse` 签名） | `least_connections` 完全失效 |
| BUG-02 | **P0** | FastestResponse 饥饿 | 中（需扩展 `RouteLatencyStats`） | 新上游无法接入、决策基于陈旧数据 |
| BUG-03 | **P0** | weighted_random 随机源 + 溢出 | 低（引入 `rand` crate） | 分流比例失真、可能 panic |
| BUG-04 | P1 | 熔断器跨 model 误伤 | 低（改 key 即可） | 故障域被放大 |
| BUG-05 | P1 | failover 重置重试计数 | 中（需引入 deadline） | 单请求挂起数分钟 |
| BUG-06 | P1 | CircuitBreaker::clone 陷阱 | 低（删除 impl） | 潜在静默失效 |
| BUG-07 | P1 | round_robin 计数器 | 低 | 热重载后短期失衡、条目泄漏 |
| BUG-08 | P2 | weight=0 校验矛盾 | 低 | 功能限制、文案误导 |
| BUG-09 | P2 | sticky TTL 固化 | 低 | 改配置不生效 |
| BUG-10 | P2 | sticky 内存膨胀 | 低 | 高 QPS 下内存增长 |
| BUG-11 | P2 | dec_in_flight 下溢 | 低 | 防御性 |

**建议首批修复**：BUG-01、BUG-02、BUG-03、BUG-04（均为生产环境会实际触发的功能性故障，且修复范围可控）。

**建议补充的单元测试**：

1. 流式请求期间 `in_flight` 计数正确性（模拟 SSE 传输中检查计数）；
2. `fastest_response` 在"一个候选无样本 + 一个有样本"场景下选中无样本候选（冷启动）；
3. `weighted_random` 在 10000 次采样下的分布误差 < 5%；
4. 熔断 key 隔离：同 provider 不同 model 熔断互不影响；
5. `dec_in_flight` 连续调用两次不产生下溢。
