# 计划：多上游负载均衡 + 会话粘性路由

## 目标

当同一个模型名称（如 `glm-5.2`）在多条路由规则中出现、且指向不同上游模型时，
在候选路由间进行负载均衡。支持可配置均衡策略（按响应速度等）。
用户重试/多轮对话时，同一会话的请求应粘在同一个上游模型上，
避免上下文/缓存丢失。

---

## 一、现状分析

### 当前路由逻辑（`config.rs` → `resolve_provider`）

```
resolve_provider(model) → Option<(&ProviderConfig, Option<&RouteConfig>)>
```

- 先在 `routes` 中精确匹配 `model`，取**第一条**命中路由
- 未命中则扫描 `providers[].models`，取**第一个**包含该模型的 provider
- **只能返回单个 provider**，无多候选概念

### 当前重试逻辑（`api.rs` → `route_and_call`）

- 解析出唯一 provider 后，在**同一 provider** 上做指数退避重试
- 熔断打开时直接返回 `CircuitOpen`，**不会 failover 到其他候选**

### 数据结构限制

| 结构          | 字段                                               | 限制                               |
| ------------- | -------------------------------------------------- | ---------------------------------- |
| `RouteConfig` | `model`, `provider`, `upstream_model`, `overrides` | 无 `weight`，无分组标识            |
| `AppConfig`   | `routes: Vec<RouteConfig>`                         | 无负载均衡配置                     |
| `AppState`    | `breakers`, `metrics`                              | 无延迟统计、无会话映射、无在途计数 |

---

## 二、设计总览

```
┌──────────────┐     ┌──────────────────┐     ┌─────────────────┐
│  Client Req  │────▶│  Session Key 提取 │────▶│  候选路由解析    │
│ (model=glm-5.2)│     │ (header/body/hash)│     │ resolve_candidates│
└──────────────┘     └────────┬─────────┘     └────────┬────────┘
                              │                         │
                     ┌────────▼─────────┐     ┌────────▼────────┐
                     │  Sticky Session   │     │  Candidates Vec  │
                     │  Map (有会话?)     │     │  [route0,route1] │
                     └────────┬─────────┘     └────────┬────────┘
                              │                         │
                     ┌────────▼────────────────────────▼────────┐
                     │           LoadBalancer.select()            │
                     │  ┌─────────────────────────────────────┐  │
                     │  │ RoundRobin / FastestResponse /      │  │
                     │  │ Weighted / Random / LeastConnections │  │
                     │  └─────────────────────────────────────┘  │
                     └────────┬──────────────────────────────────┘
                              │
                     ┌────────▼────────┐
                     │  选中 route[i]   │
                     │  → 记录 sticky   │
                     └────────┬────────┘
                              │
                     ┌────────▼────────┐
                     │  call_upstream   │
                     │  + retry loop    │
                     │  (同 provider)   │
                     └────────┬────────┘
                              │
                     ┌────────▼────────┐
                     │  成功→记延迟    │
                     │  失败→failover? │
                     └─────────────────┘
```

---

## 三、详细设计

### 3.1 配置 Schema 变更（`config.rs`）

#### 新增枚举与结构

```rust
/// 负载均衡策略
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LbStrategy {
    /// 轮询（默认）
    RoundRobin,
    /// 按响应速度选择（EWMA 延迟最低优先）
    FastestResponse,
    /// 加权随机（按 route.weight）
    Weighted,
    /// 均匀随机
    Random,
    /// 最少在途连接数
    LeastConnections,
}

impl Default for LbStrategy {
    fn default() -> Self { Self::RoundRobin }
}

/// 会话标识来源
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SessionKeySource {
    /// 从 HTTP header 取值，如 "x-session-id"
    Header(String),
    /// 从请求 body 字段取值，如 "session_id" / "user"
    BodyField(String),
    /// 用第一条 role=user 消息的内容哈希
    FirstUserMessage,
    /// 用全部 messages 的内容哈希
    MessagesHash,
}

impl Default for SessionKeySource {
    fn default() -> Self { Self::FirstUserMessage }
}

/// 会话粘性配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StickyConfig {
    pub enabled: bool,
    pub session_key_source: SessionKeySource,
    /// 粘性 TTL（秒）；超时后重新选择上游
    pub ttl_secs: u64,
}

impl Default for StickyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            session_key_source: SessionKeySource::FirstUserMessage,
            ttl_secs: 1800, // 30 分钟
        }
    }
}

/// 负载均衡配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadBalanceConfig {
    pub enabled: bool,
    pub strategy: LbStrategy,
    pub sticky: StickyConfig,
}

impl Default for LoadBalanceConfig {
    fn default() -> Self {
        Self {
            enabled: false, // 默认关闭，保持向后兼容
            strategy: LbStrategy::RoundRobin,
            sticky: StickyConfig::default(),
        }
    }
}
```

#### `RouteConfig` 新增字段

```rust
pub struct RouteConfig {
    pub model: String,
    pub provider: String,
    #[serde(default)]
    pub upstream_model: String,
    #[serde(default)]
    pub overrides: serde_json::Map<String, serde_json::Value>,
    /// 负载均衡权重（strategy = Weighted 时生效，默认 1）
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 是否禁用此路由参与负载均衡（设为 true 则仅作为 fallback）
    #[serde(default)]
    pub disabled: bool,
}

fn default_weight() -> u32 { 1 }
```

#### `AppConfig` 新增字段

```rust
pub struct AppConfig {
    pub server: ServerConfig,
    pub retry: RetryConfig,
    pub providers: Vec<ProviderConfig>,
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub load_balance: LoadBalanceConfig,
}
```

#### 新增候选解析方法

```rust
/// 一个候选路由（provider + 可选 route）
pub struct Candidate<'a> {
    pub provider: &'a ProviderConfig,
    pub route: Option<&'a RouteConfig>,
    /// 在候选列表中的索引（用于 sticky 记录）
    pub index: usize,
}

impl AppConfig {
    /// 返回模型对应的所有候选路由
    /// 1. 先收集 routes 中 model 精确匹配的（排除 disabled）
    /// 2. 若 routes 无命中，扫描 providers[].models 作为隐式候选
    pub fn resolve_candidates(&self, model: &str) -> Vec<Candidate<'_>> {
        let mut result = Vec::new();

        // 精确路由匹配
        for (i, route) in self.routes.iter().enumerate() {
            if route.model != model || route.disabled {
                continue;
            }
            if let Some(p) = self.providers
                .iter()
                .find(|p| p.id == route.provider && p.enabled)
            {
                result.push(Candidate { provider: p, route: Some(route), index: i });
            }
        }

        // 若无精确路由，扫描 provider 模型表
        if result.is_empty() {
            for p in self.providers.iter().filter(|p| p.enabled) {
                if p.models.iter().any(|m| m == model) {
                    result.push(Candidate { provider: p, route: None, index: result.len() });
                }
            }
        }

        result
    }

    /// 向后兼容：返回首个候选（等价于旧 resolve_provider）
    pub fn resolve_provider(&self, model: &str) -> Option<(&ProviderConfig, Option<&RouteConfig>)> {
        self.resolve_candidates(model)
            .into_iter()
            .next()
            .map(|c| (c.provider, c.route))
    }
}
```

---

### 3.2 运行时状态扩展（`state.rs`）

#### 新增结构

```rust
use std::sync::atomic::AtomicU64;

/// 单条路由的延迟统计（EWMA）
pub struct RouteLatencyStats {
    /// 指数加权移动平均延迟（毫秒）
    ewma_ms: AtomicU64,
    /// 样本数
    samples: AtomicU64,
    /// 最后更新时间
    last_update: Mutex<Instant>,
}

impl RouteLatencyStats {
    pub fn new() -> Self { ... }

    /// 记录一次延迟样本，更新 EWMA
    /// alpha = 0.3（新样本权重 30%）
    pub fn record_latency(&self, latency_ms: u64) {
        let prev = self.ewma_ms.load(Relaxed);
        let new = if prev == 0 {
            latency_ms
        } else {
            // EWMA = alpha * new + (1 - alpha) * old
            ((latency_ms as f64 * 0.3) + (prev as f64 * 0.7)) as u64
        };
        self.ewma_ms.store(new, Relaxed);
        self.samples.fetch_add(1, Relaxed);
    }

    pub fn avg_ms(&self) -> u64 { self.ewma_ms.load(Relaxed) }
    pub fn sample_count(&self) -> u64 { self.samples.load(Relaxed) }
}

/// 会话粘性映射条目
pub struct StickyEntry {
    /// 选中的候选索引
    pub candidate_index: usize,
    /// 路由标识（model|provider_id|upstream_model）用于校验配置未变
    pub route_key: String,
    /// 创建时间
    pub created_at: Instant,
}
```

#### `AppState` 新增字段

```rust
pub struct AppState {
    // ... 现有字段 ...

    /// 路由延迟统计：key = "{model}|{provider_id}|{upstream_model}"
    pub route_latency: DashMap<String, RouteLatencyStats>,
    /// 轮询计数器：key = "{model}" → AtomicU64
    pub lb_counters: DashMap<String, AtomicU64>,
    /// 会话粘性映射：key = session_key → StickyEntry
    pub sticky_sessions: DashMap<String, StickyEntry>,
    /// 在途请求计数：key = "{model}|{provider_id}|{upstream_model}"
    pub in_flight: DashMap<String, AtomicU64>,
}
```

#### 路由标识 key 生成函数

```rust
/// 生成路由唯一标识，用于延迟统计和粘性校验
fn route_key(model: &str, provider_id: &str, upstream_model: &str) -> String {
    format!("{model}|{provider_id}|{upstream_model}")
}
```

---

### 3.3 负载均衡器模块（新文件 `lb.rs`）

```rust
//! 负载均衡选择器：从候选路由中按策略选一个

use crate::proxy::config::{Candidate, LbStrategy, LoadBalanceConfig};
use crate::proxy::state::AppState;
use std::sync::atomic::Ordering;

/// 从候选列表中选择一个索引
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
    if candidates.len() == 1 {
        return Some(0);
    }

    // 1. 会话粘性优先
    if lb_cfg.sticky.enabled {
        if let Some(key) = session_key {
            if let Some(entry) = state.sticky_sessions.get(key) {
                // 校验未过期
                if entry.created_at.elapsed().as_secs() < lb_cfg.sticky.ttl_secs {
                    // 校验候选索引仍有效
                    if entry.candidate_index < candidates.len() {
                        return Some(entry.candidate_index);
                    }
                }
            }
        }
    }

    // 2. 按策略选择
    let idx = match lb_cfg.strategy {
        LbStrategy::RoundRobin => round_robin(state, model, candidates.len()),
        LbStrategy::Random => random_index(candidates.len()),
        LbStrategy::Weighted => weighted_random(candidates),
        LbStrategy::FastestResponse => fastest_response(state, candidates, model),
        LbStrategy::LeastConnections => least_connections(state, candidates, model),
    };

    // 3. 记录粘性
    if lb_cfg.sticky.enabled {
        if let Some(key) = session_key {
            if let Some(i) = idx {
                let candidate = &candidates[i];
                let key_str = route_key(
                    model,
                    &candidate.provider.id,
                    candidate.route.map(|r| r.upstream_model.as_str()).unwrap_or(""),
                );
                state.sticky_sessions.insert(key.to_string(), StickyEntry {
                    candidate_index: i,
                    route_key: key_str,
                    created_at: Instant::now(),
                });
            }
        }
    }

    idx
}

fn round_robin(state: &AppState, model: &str, len: usize) -> Option<usize> {
    let counter = state.lb_counters
        .entry(model.to_string())
        .or_insert(AtomicU64::new(0));
    let val = counter.fetch_add(1, Ordering::Relaxed);
    Some((val as usize) % len)
}

fn fastest_response(state: &AppState, candidates: &[Candidate], model: &str) -> Option<usize> {
    // 选 EWMA 延迟最低的；无数据时视为延迟 0（优先尝试）
    candidates.iter().enumerate()
        .min_by_key(|(_, c)| {
            let key = route_key(model, &c.provider.id, ...);
            state.route_latency.get(&key)
                .map(|s| s.avg_ms())
                .unwrap_or(0)
        })
        .map(|(i, _)| i)
}

fn least_connections(state: &AppState, candidates: &[Candidate], model: &str) -> Option<usize> {
    candidates.iter().enumerate()
        .min_by_key(|(_, c)| {
            let key = route_key(model, &c.provider.id, ...);
            state.in_flight.get(&key)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0)
        })
        .map(|(i, _)| i)
}

fn weighted_random(candidates: &[Candidate]) -> Option<usize> {
    let weights: Vec<u32> = candidates.iter()
        .map(|c| c.route.map(|r| r.weight).unwrap_or(1))
        .collect();
    let total: u32 = weights.iter().sum();
    if total == 0 { return random_index(candidates.len()); }
    let r = rand::random::<u32>() % total;
    let mut acc = 0;
    for (i, &w) in weights.iter().enumerate() {
        acc += w;
        if r < acc { return Some(i); }
    }
    Some(0)
}
```

---

### 3.4 会话 Key 提取（`api.rs` 或 `lb.rs`）

```rust
/// 从请求中提取会话标识
pub fn extract_session_key(
    headers: &HeaderMap,
    body: &serde_json::Value,
    source: &SessionKeySource,
) -> Option<String> {
    match source {
        SessionKeySource::Header(name) => {
            headers.get(name)
                .and_then(|v| v.to_str().ok())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        }
        SessionKeySource::BodyField(field) => {
            body.get(field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        }
        SessionKeySource::FirstUserMessage => {
            body.get("messages")
                .and_then(|m| m.as_array())
                .and_then(|msgs| {
                    msgs.iter()
                        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                })
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str().or_else(|| {
                    // content 可能是数组（多模态），取文本部分
                    c.as_array().and_then(|arr| {
                        arr.iter().find_map(|p| p.get("text").and_then(|t| t.as_str()))
                    })
                }))
                .map(|s| hash_str(s))
        }
        SessionKeySource::MessagesHash => {
            body.get("messages")
                .map(|m| hash_str(&m.to_string()))
        }
    }
}

fn hash_str(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
```

---

### 3.5 修改 `route_and_call`（`api.rs`）

核心改动流程：

```rust
async fn route_and_call(
    state: &Arc<AppState>,
    path: &str,
    body: &serde_json::Value,
    log: &mut RequestLog,
) -> Result<UpstreamOutcome, ProxyError> {
    let model = body.get("model")...;

    // ── 1. 解析配置 ──
    let (lb_cfg, retry_cfg, breaker_cfg, candidates) = {
        let cfg = state.config.read();
        let candidates = cfg.resolve_candidates(model);
        if candidates.is_empty() {
            return Err(ProxyError::RouteNotFound(model.to_string()));
        }
        (
            cfg.load_balance.clone(),
            cfg.retry.clone(),
            cfg.retry.circuit_breaker.clone(),
            candidates.into_iter()
                .map(|c| (c.provider.clone(), c.route.cloned()))
                .collect::<Vec<_>>(),
        )
    };

    // ── 2. 提取会话 Key ──
    let session_key = if lb_cfg.sticky.enabled {
        extract_session_key(headers, body, &lb_cfg.sticky.session_key_source)
    } else {
        None
    };

    // ── 3. 负载均衡选择 ──
    let selected_idx = if lb_cfg.enabled && candidates.len() > 1 {
        lb::select(state, &candidates, model, session_key.as_deref(), &lb_cfg)
            .unwrap_or(0)
    } else {
        0
    };

    // ── 4. 在选中的候选上执行请求（含重试 + failover）──
    let total_candidates = candidates.len();
    let mut current_idx = selected_idx;

    loop {
        let (provider, route) = &candidates[current_idx];
        let body = apply_overrides(body, route.as_ref());

        // 熔断检查
        let breaker = state.breakers
            .entry(provider.id.clone())
            .or_insert_with(|| Arc::new(CircuitBreaker::new(breaker_cfg.clone())))
            .clone();

        if breaker_cfg.enabled && breaker.before_request().is_err() {
            // 熔断打开 → failover 到下一个候选
            if lb_cfg.enabled && total_candidates > 1 {
                current_idx = (current_idx + 1) % total_candidates;
                continue;
            }
            return Err(ProxyError::CircuitOpen(provider.id.clone()));
        }

        // 在途计数 +1
        let inflight_key = route_key(model, &provider.id, ...);
        inc_inflight(state, &inflight_key);

        let started = Instant::now();
        match call_upstream(state, provider, path, body.clone()).await {
            Ok(outcome) => {
                breaker.on_success();
                dec_inflight(state, &inflight_key);
                // 记录延迟样本
                record_latency(state, &inflight_key, started.elapsed().as_millis() as u64);
                log.provider = Some(provider.name.clone());
                log.retries = attempt;
                return Ok(outcome);
            }
            Err(e) if e.retriable(&retry_cfg) && attempt < retry_cfg.max_retries => {
                breaker.on_failure();
                dec_inflight(state, &inflight_key);
                attempt += 1;
                // 同 provider 重试（保持上下文）
                tokio::time::sleep(backoff_delay(attempt, &retry_cfg)).await;
            }
            Err(e) => {
                breaker.on_failure();
                dec_inflight(state, &inflight_key);
                // 不可重试 → 尝试 failover
                if lb_cfg.enabled && total_candidates > 1
                    && current_idx != selected_idx.wrapping_sub(1) % total_candidates
                {
                    current_idx = (current_idx + 1) % total_candidates;
                    continue;
                }
                return Err(e);
            }
        }
    }
}
```

**关键设计点：**

| 场景         | 行为                                      |
| ------------ | ----------------------------------------- |
| 首次请求     | LB 策略选择候选，记录 sticky              |
| 同会话重试   | sticky 命中 → 同一候选                    |
| 同请求内重试 | 同一 provider 指数退避（不变候选）        |
| 熔断打开     | failover 到下一候选                       |
| 不可重试错误 | failover 到下一候选（已遍历完则返回错误） |

---

### 3.6 粘性会话清理

后台定时任务清理过期 sticky 条目：

```rust
/// 每 60 秒清理过期的 sticky 会话
pub fn spawn_sticky_cleanup(state: Arc<AppState>, ttl_secs: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let now = Instant::now();
            state.sticky_sessions.retain(|_, entry| {
                now.duration_since(entry.created_at).as_secs() < ttl_secs
            });
        }
    });
}
```

在 `serve()` 启动时调用。

---

### 3.7 前端变更

#### `types.ts` 新增类型

```typescript
export type LbStrategy =
  | "round_robin"
  | "fastest_response"
  | "weighted"
  | "random"
  | "least_connections";

export type SessionKeySource =
  | { type: "header"; value: string }
  | { type: "body_field"; value: string }
  | { type: "first_user_message" }
  | { type: "messages_hash" };

export interface StickyConfig {
  enabled: boolean;
  session_key_source: SessionKeySource;
  ttl_secs: number;
}

export interface LoadBalanceConfig {
  enabled: boolean;
  strategy: LbStrategy;
  sticky: StickyConfig;
}

// RouteConfig 新增:
export interface RouteConfig {
  model: string;
  provider: string;
  upstream_model?: string;
  overrides: Record<string, unknown>;
  weight?: number; // 新增
  disabled?: boolean; // 新增
}

// AppConfig 新增:
export interface AppConfig {
  server: ServerConfig;
  retry: RetryConfig;
  providers: ProviderConfig[];
  routes: RouteConfig[];
  load_balance: LoadBalanceConfig; // 新增
}
```

#### Settings 页面新增 UI

- 负载均衡开关
- 策略下拉选择
- 粘性会话开关 + TTL + Key 来源选择
- 路由编辑器中每条路由增加 weight 输入框

---

## 四、实施步骤

| 步骤 | 文件             | 内容                                                                                                                                                                                | 依赖     |
| ---- | ---------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------- |
| 1    | `config.rs`      | 新增 `LbStrategy`/`LoadBalanceConfig`/`StickyConfig`/`SessionKeySource`/`Candidate`，`RouteConfig` 加 `weight`/`disabled`，`AppConfig` 加 `load_balance`，新增 `resolve_candidates` | 无       |
| 2    | `state.rs`       | 新增 `RouteLatencyStats`/`StickyEntry`，`AppState` 加 4 个 DashMap                                                                                                                  | 步骤 1   |
| 3    | `lb.rs` (新)     | 负载均衡选择器 + 会话 Key 提取 + 路由 key 工具函数                                                                                                                                  | 步骤 1,2 |
| 4    | `mod.rs`         | 注册 `pub mod lb`                                                                                                                                                                   | 步骤 3   |
| 5    | `api.rs`         | 改造 `route_and_call`：多候选 + LB 选择 + sticky + failover + 延迟记录                                                                                                              | 步骤 1-3 |
| 6    | `api.rs`         | `chat_completions`/`fallback_proxy` 传入 headers 给 `route_and_call`                                                                                                                | 步骤 5   |
| 7    | `store.rs`       | `validate` 增加路由 weight/disabled 校验                                                                                                                                            | 步骤 1   |
| 8    | `lib.rs`/`serve` | 启动 sticky 清理后台任务                                                                                                                                                            | 步骤 2   |
| 9    | `types.ts`       | 新增 TS 类型定义                                                                                                                                                                    | 步骤 1   |
| 10   | `Settings.tsx`   | 负载均衡配置 UI                                                                                                                                                                     | 步骤 9   |
| 11   | 路由编辑器       | weight 输入框                                                                                                                                                                       | 步骤 9   |
| 12   | 单元测试         | `resolve_candidates`、`lb::select` 各策略、sticky 命中/过期                                                                                                                         | 步骤 1-5 |

---

## 五、配置示例

```json
{
  "load_balance": {
    "enabled": true,
    "strategy": "fastest_response",
    "sticky": {
      "enabled": true,
      "session_key_source": { "type": "first_user_message" },
      "ttl_secs": 1800
    }
  },
  "providers": [
    { "id": "zhipu-a", "name": "智谱A", "base_url": "https://open.bigmodel.cn/api/paas/v4", ... },
    { "id": "zhipu-b", "name": "智谱B", "base_url": "https://open.bigmodel.cn/api/paas/v4", ... },
    { "id": "siliconflow", "name": "硅基流动", "base_url": "https://api.siliconflow.cn/v1", ... }
  ],
  "routes": [
    { "model": "glm-5.2", "provider": "zhipu-a", "upstream_model": "glm-4-plus", "weight": 3 },
    { "model": "glm-5.2", "provider": "zhipu-b", "upstream_model": "glm-4-plus", "weight": 1 },
    { "model": "glm-5.2", "provider": "siliconflow", "upstream_model": "zhipuai/glm-4-plus", "weight": 2 }
  ]
}
```

**行为说明：**

- 3 条路由都暴露为 `glm-5.2`，但上游模型名各不同
- `fastest_response` 策略：自动选延迟最低的上游
- 同一会话（首条 user 消息相同）30 分钟内粘在同一上游
- 某上游熔断时自动 failover 到其他候选

---

## 六、风险与注意事项

| 风险                            | 缓解                                                                     |
| ------------------------------- | ------------------------------------------------------------------------ |
| sticky 会话内存泄漏             | 后台定时清理 + TTL 过期 + DashMap retain                                 |
| EWMA 冷启动偏差（新路由无数据） | 无数据时延迟视为 0，优先尝试新路由以快速积累样本                         |
| 配置热重载后 sticky 索引失效    | StickyEntry 记录 route_key，选择时校验候选索引有效性                     |
| failover 导致上下文丢失         | failover 仅在熔断/不可重试错误时触发；同请求内重试不变候选               |
| `rand` 依赖                     | Cargo.toml 添加 `rand = "0.8"`                                           |
| 向后兼容                        | `load_balance.enabled` 默认 `false`，旧配置无此字段时 serde default 生效 |
