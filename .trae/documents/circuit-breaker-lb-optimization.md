# 熔断与负载均衡优化：503 熔断 + 探活恢复 + SWRR + 慢启动（v2）

## Context（背景与目标）

当前体系的问题：

1. **503 不触发熔断**：仅是可重试状态码之一，需连续失败 5 次才跳闸
2. **熔断配置不动态**：`CircuitBreaker` 创建时克隆配置，GUI 改参数对已存在的熔断器不生效
3. **重试配置过度暴露**，但状态码语义混杂（503 既重试又计数）
4. **前端熔断监控失效**：`get_breaker_status` / `reset_breaker` 按 provider_id 查找，而熔断器键已是路由维度（`model|provider|upstream_model`），显示恒为空
5. **轮询/加权随机粗糙**：朴素计数器取模 + 随机加权，分配有突发、热重载候选变化时错乱
6. **熔断恢复不平滑**：30 秒冷却后单请求试探即全量恢复，易抖动

用户已确认的设计决策（两轮问答结果）：

- 503 **单次立即熔断**，与 retry_statuses 配置解耦
- 探活方式：**POST /chat/completions 试探**（max_tokens=1，故障路由的 upstream_model）
- 全熔断时：**Panic 模式**（最早熔断渠道限速放行试探）
- 重试：GUI 保留 **max_retries + retry_statuses**；退避基数等其余内部固定
- 熔断新增可调：**探活间隔、恢复阈值、panic 窗口**
- LB 升级：**SWRR 平滑加权轮询**（替换朴素轮询+加权随机）+ **恢复慢启动 slow_start**
- 告警形态：**全局告警栏**（顶部常驻"N 个渠道已熔断"，点击跳转渠道页）

参考成熟方案：Nginx（SWRR、slow_start）、Envoy（outlier detection、panic threshold）、Hystrix/resilience4j（half-open 状态机）。

---

## 一、熔断状态机（重写 `circuit.rs`）

### 1.1 触发条件（详细定义）

| 触发源 | 条件 | 动作 |
|--------|------|------|
| 上游返回 **503** | **单次即触发，与 retry_statuses 无关** | `trip_now()`：立即跳闸；**同渠道绝不重试**，直接 failover 到其他候选 |
| 网络错误/超时/408/429/500/502/529 | 连续失败达 `failure_threshold`（默认 5） | 跳闸 |
| 非可重试错误（400/401/404 等） | 计入连续失败（保留现行为） | 达阈值跳闸 |

**503 与重试配置的关系（语义澄清）**：`retry_statuses` 只管其他状态码的**同渠道退避重试**；503 由熔断器接管，列表里有没有 503 都不影响——有 503 也一样立即跳闸。默认值从列表中移除 503，GUI 标注"503 已由熔断器接管"。

### 1.2 状态机

```
                 503（单次） 或 连续失败≥阈值
   CLOSED ──────────────────────────────▶ OPEN
     ▲                                     │  持续 open_duration_secs（默认 3600s，可配）
     │ 恢复阈值：连续成功 recovery_success_threshold 次（默认 2，可配）
     └──────────── HALF_OPEN ◀──── 到期自动转入（单名额放行试探）
                      │
                      │ 试探/探活失败
                      └──▶ OPEN（opened_at=now，完整时长重新计时）
```

- **CLOSED**：正常放行；成功清零 `failures`
- **OPEN**：`before_request()` 拒绝；到期转 HALF_OPEN
- **HALF_OPEN**：单探测名额（CAS，现有机制）；连续成功达 `recovery_success_threshold` 才 CLOSE → 恢复期流量一粒一粒放行，天然平滑；任一失败 → 重新 OPEN 完整重计 → 防抖动
- **CLOSED 后叠加慢启动**（见三.2）：SWRR 权重从 10% 爬升到 100%，双层平滑

### 1.3 Panic 模式（全部候选均熔断）

- 某模型全部候选 OPEN 且未到期 → 选 `opened_at` 最早者，原子抢占 panic 名额（CAS 时间戳）
- `panic_probe_window_secs`（默认 60，可配）：同一熔断器窗口内最多放行 1 个 panic 请求
- 抢到 → 绕过 `before_request()` 发真实请求；成功 → `on_success()`；失败 → **不动熔断器**（不重置时钟），错误原样返回客户端
- 全部抢不到 → 快速失败 `CircuitOpen`
- 兜底"单次偶发 503 把唯一渠道硬拒 1 小时"，同时限速避免打死上游

### 1.4 动态配置

熔断器不再持有配置副本；状态留在实例内，阈值/时长/窗口由参数传入：

```rust
pub fn before_request(&self, cfg: &CircuitBreakerConfig) -> Result<(), BreakerOpen>
pub fn on_failure(&self, cfg: &CircuitBreakerConfig) -> bool
pub fn trip_now(&self)
pub fn on_success(&self)                    // OPEN→HALF_OPEN(count=1)；HALF_OPEN count≥阈值→CLOSED+记 recovered_at
pub fn try_claim_panic_probe(&self, window_secs: u64) -> bool
pub fn opened_at(&self) -> Option<Instant>
pub fn recovered_at(&self) -> Option<Instant>   // 慢启动用
pub fn cooldown_remaining_secs(&self, cfg: &CircuitBreakerConfig) -> Option<u64>
```

---

## 二、探活机制（新文件 `probe.rs`）

### 2.1 主动探活任务

`spawn_probe_task(state) -> JoinHandle<()>`（serve 两处启动/abort，与 sticky 清理同模式）：

1. 每轮间隔 = `probe_interval_secs`（默认 30，**可配**，动态读取）
2. 遍历 `state.breakers`：OPEN 且 `elapsed >= open_duration_secs` 的熔断器：
   - 解析 route_key → 找启用的 provider；provider 已删 → 清理该熔断器条目
   - 发试探请求（见 2.2）；**2xx → `on_success()`**（计入恢复阈值）；否则保持 OPEN（不重置时钟），下轮再试
3. `enabled=false` 时跳过

### 2.2 试探请求

```json
POST {provider.base_url}/chat/completions
{ "model": "<upstream_model 或 model>", "messages": [{"role":"user","content":"ping"}], "max_tokens": 1 }
```

- 复用 `headers::build_upstream_headers(&HeaderMap::new(), provider)`（渠道鉴权/自定义头）
- 超时内部固定 15s；判定：2xx=健康，其余=未恢复
- 已知限制（文档注明）：embeddings 等非对话路由试探可能 404 → 依赖被动半开恢复；个别上游拒绝 max_tokens=1 → 同样回落被动恢复

### 2.3 被动探活（保留增强）

OPEN 到期后第一个真实请求作试探（`before_request` 转 HALF_OPEN），成功计入恢复阈值；与主动探活共用状态机。

---

## 三、负载均衡升级（`lb.rs` + `state.rs`）

### 3.1 SWRR 平滑加权轮询（Nginx 算法）

**替换**现有朴素轮询（计数器取模）与加权随机两个实现：

- 状态：`state.swrr_weights: DashMap<String, AtomicI64>`（route_key → current_weight）；删除 `lb_counters`
- 选择：`cw[i] += w[i]`；取最大者；`cw[winner] -= Σw` —— 分配序列均匀无突发、权重感知、候选变化时自然过渡
- 策略映射（枚举值不变，向后兼容）：
  - `RoundRobin` → SWRR（所有候选 weight=1）
  - `Weighted` → SWRR（`route.weight`）
  - `Random` / `FastestResponse` / `LeastConnections` 不变
- sticky 命中时不更新 SWRR 状态（未发生选择）
- 过期条目由清理任务回收（route_key 不在当前配置即删，附带容量上限保护）

### 3.2 恢复慢启动 slow_start（Nginx 同款语义）

- 熔断器记录 `recovered_at`（HALF_OPEN→CLOSED 时设置；手动 reset 不设置=立即全量）
- SWRR 选择时有效权重：`w_eff = w × ramp(t)`，`t` 为恢复后经过时间：

```
ramp(t) = clamp(t / SLOW_START_SECS, 0.10, 1.0)   // 10% 起步，线性爬到 100%
SLOW_START_SECS = 120（内部常量）
```

- 作用域：SWRR 两策略（round_robin / weighted）；`FastestResponse` 由 EWMA 乐观基线、`LeastConnections` 由在途计数天然具备恢复特性，不需要额外处理（文档注明）
- 与半开阈值形成双层平滑：半开期一粒一粒放 → 关闭后权重爬坡

### 3.3 熔断状态与 LB 选择集成（`api.rs`）

`resolve_routing_context` 解析候选后按熔断状态分类：

- **eligible** = 熔断器缺失 / CLOSED / HALF_OPEN / OPEN-已到期（会转半开）
- eligible 非空 → LB（含 sticky）只在 eligible 子集上选择；sticky 目标若熔断 → route_key 校验自然失效 → 重选；failover 循环也只在 eligible 内轮转
- **eligible 为空** → Panic：取 `opened_at` 最早者，`try_claim_panic_probe(window)`；抢到 → `panic_mode=true` 执行；抢不到 → 返回 `CircuitOpen`

`route_and_call` 错误分支：

```
503 分支（最高优先级）:
    非 panic → breaker.trip_now()（warn 日志）
    立即 failover 下一候选（同渠道不退避重试）；无可切 → 503 原样返回客户端

可重试分支（408/429/500/502/529/网络/超时，且 attempt < max_retries）:
    breaker.on_failure(&cfg) → 同渠道退避重试

不可重试/耗尽分支:
    breaker.on_failure(&cfg) → failover

panic_mode 下的失败: 不调 on_failure / trip_now（不重置熔断时钟）
```

成功分支统一 `on_success()`（内含阈值逻辑）。

---

## 四、重试策略简化（`config.rs` + `retry.rs`）

### 4.1 配置面

```rust
pub struct RetryConfig {
    pub max_retries: u32,                  // 默认 3，校验 0..=5（GUI 保留）
    pub retry_statuses: Vec<u16>,          // 保留用户可配；默认 [408, 429, 500, 502, 529]，移除 503
    pub circuit_breaker: CircuitBreakerConfig,
}

pub struct CircuitBreakerConfig {
    pub enabled: bool,                              // 默认 true
    pub failure_threshold: u32,                     // 非 503 失败跳闸阈值，默认 5
    #[serde(alias = "cooldown_secs")]
    pub open_duration_secs: u64,                    // 熔断时长，默认 3600
    pub probe_interval_secs: u64,                    // 探活间隔，默认 30
    pub recovery_success_threshold: u32,            // 恢复所需连续成功次数，默认 2
    pub panic_probe_window_secs: u64,                // panic 试探窗口，默认 60
}
```

旧配置：`cooldown_secs` alias 无损读入；被删字段（base_delay_ms/max_delay_ms/retry_on_network_error）serde 忽略；旧列表里的 503 无害（由熔断器接管）。

### 4.2 内部固定参数（不暴露）

```rust
pub const RETRY_BACKOFF_BASE_MS: u64 = 500;   // 固定退避基数
pub const RETRY_BACKOFF_MAX_MS: u64 = 8000;
// 网络错误/超时始终重试；503 不走重试（熔断接管）
// SLOW_START_SECS = 120；PROBE_TIMEOUT_SECS = 15
```

### 4.3 退避算法

```
delay(attempt) = min(500 × 2^(attempt-1), 8000)   // 500ms → 1s → 2s → 4s → 8s 封顶，无抖动
```

### 4.4 超时限制

- 单次请求：`provider.timeout_secs`（校验加界 30..=600）
- 重试×failover 总时限：`TOTAL_REQUEST_TIMEOUT_SECS = 120`（不变）

### 4.5 签名简化

`retriable(&self) -> bool`、`backoff_delay(attempt) -> Duration`（去 cfg 参数；`is_503()` 辅助判断供 503 分支用）。

---

## 五、监控与告警

### 5.1 日志（tracing）

| 事件 | 级别 |
|------|------|
| 503 跳闸（route_key、模型、渠道、预计恢复时间） | warn |
| 阈值跳闸（连续失败数） | warn |
| 探活尝试/结果 | info |
| 熔断恢复（半开关闭、慢启动开始） | info |
| Panic 试探 | warn |

请求级：`RequestLog.error_detail` 继续携带 `cooldown_remaining_secs` / `consecutive_failures`；`CircuitOpen` hint 文案更新（1 小时熔断 + 自动探活 + panic 语义）。

### 5.2 后端状态接口（修复 + 增强，`src-tauri/src/lib.rs`）

- `get_breaker_status`：按 provider **聚合**路由维度熔断器（open > half_open > closed 取最差），返回 `{provider_id: {state, consecutive_failures, cooldown_remaining_secs}}` → 前端现有按 `p.id` 取值的显示即刻修复
- `reset_breaker(provider_id)`：前缀匹配 `|{provider_id}|` 重置该渠道所有路由熔断器

### 5.3 前端

- **全局告警栏**（`AppShell.tsx`）：任一渠道熔断时顶部常驻横幅"N 个渠道已熔断·预计 X 后自动探活"，点击跳转渠道页；全部恢复后消失
- `Settings.tsx`：
  - 熔断卡片：开关 + 5 个输入（失败阈值/熔断时长(分钟)/探活间隔(秒)/恢复阈值/panic 窗口(秒)），文案说明 503 立即熔断 + 探活恢复流程
  - 重试卡片：最大重试次数（上限 5）+ 状态码 chips（保留，503 标灰注明"由熔断器接管"）；删除退避基数/最大退避/网络重试开关
  - 均衡策略下拉文案：轮询→平滑轮询 (SWRR)、加权随机→加权轮询 (Smooth Weighted)
- `types.ts` / `mock.ts`：新配置结构 + `BreakerStatus` 增 `cooldown_remaining_secs` + mock 告警数据
- `Overview.tsx` / `Providers.tsx` / `LogDetail.tsx`：键修复后自然恢复，仅核对

---

## 六、文件改动清单

| 文件 | 改动 |
|------|------|
| `proxy-core/src/proxy/circuit.rs` | 状态机重写：cfg 参数化、trip_now、恢复阈值、panic 名额、recovered_at；测试 |
| `proxy-core/src/proxy/config.rs` | RetryConfig/CircuitBreakerConfig 新结构 + alias + 默认值；测试 |
| `proxy-core/src/proxy/retry.rs` | 内部常量、退避/可重试简化、503 语义；测试 |
| `proxy-core/src/proxy/probe.rs`（新） | 主动探活任务 |
| `proxy-core/src/proxy/api.rs` | eligible 过滤、panic 分支、503 分支、cfg 传递 |
| `proxy-core/src/proxy/lb.rs` | SWRR 替换 round_robin/weighted；慢启动权重；删 lb_counters |
| `proxy-core/src/proxy/state.rs` | 增 `swrr_weights`，删 `lb_counters` |
| `proxy-core/src/proxy/mod.rs` | `pub mod probe`；serve 两处 spawn/abort |
| `proxy-core/src/proxy/store.rs` | 校验：open_duration/probe_interval/recovery_threshold/panic_window/max_retries/timeout_secs 边界 |
| `proxy-core/src/proxy/smoke_tests.rs` | 更新既有用例 + 新增：503 熔断 failover、panic 恢复、探活恢复、SWRR 分布、慢启动 |
| `src-tauri/src/lib.rs` | get_breaker_status 聚合 + reset_breaker 前缀匹配 |
| `src/lib/types.ts`、`src/lib/mock.ts` | 类型/默认值/mock 同步 |
| `src/pages/Settings.tsx` | 熔断/重试卡片重构、策略文案 |
| `src/components/AppShell.tsx` | 全局告警栏 |
| `docs/circuit-breaker.md`（新） | 完整设计文档（用户要求的交付物） |

复用：`headers::build_upstream_headers`、`lb::route_key`、`InFlightGuard`、`spawn_sticky_cleanup` 任务模式、smoke mock 框架。

---

## 七、测试与验证

单元测试：

1. `circuit.rs`：503 立即跳闸；阈值跳闸；恢复阈值（2 次）才关闭；试探失败完整重计；panic 窗口（二次抢占被拒）；动态配置（不同 open_duration/panic_window 参数即时生效）
2. `retry.rs`：退避序列 500/1000/2000/4000/8000 封顶；503 不可同渠道重试；列表内状态码可重试
3. `lb.rs`：SWRR 分配序列（权重 5:1:1 的循环分布）；权重 9:1 采样比例；慢启动期有效权重从 10% 爬升
4. `config.rs`：旧配置（cooldown_secs + 已删字段 + 列表含 503）无损加载
5. `store.rs`：新校验边界
6. `probe.rs`：试探请求体构造；判定映射（2xx→on_success）

冒烟测试（mock 上游）：

7. 渠道 A 恒 503、B 正常 → failover 从 B 成功，A 的熔断器 OPEN
8. 单渠道先 503 后恢复 → panic 放行试探成功后关闭
9. 探活轮函数直接调用对 OPEN-到期熔断器恢复

验证命令：

```powershell
cargo test    # src-tauri/proxy-core
cargo clippy
npm run build # 根目录，前端类型检查
```

---

## 八、兼容性与风险

| 项 | 处理 |
|----|------|
| 旧配置 | alias + 忽略未知字段无损加载；旧 30s 冷却保留为用户值 |
| 单次 503 即熔断的激进性 | panic 模式兜底（窗口限速）+ GUI 手动重置 + 文档说明 |
| 旧配置 retry_statuses 含 503 | 无害：503 由熔断器接管，列表对 503 不生效，GUI 标注 |
| embeddings 路由探活 404 | 依赖被动半开恢复，文档注明 |
| SWRR 状态膨胀 | route_key 维度 DashMap，清理任务回收失效条目 |
| sticky 与慢启动 | sticky 命中绕过权重选择（粘性优先，语义不变），文档注明 |
