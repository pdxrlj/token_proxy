use serde::{Deserialize, Serialize};

// ─────────────────────────── 负载均衡配置 ───────────────────────────

/// 负载均衡策略。
///
/// 决定当同一模型存在多个上游候选时如何选择。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LbStrategy {
    /// 轮询（默认）：按顺序循环分配
    RoundRobin,
    /// 按响应速度选择：EWMA 延迟最低优先
    FastestResponse,
    /// 加权随机：按 `route.weight` 比例分配
    Weighted,
    /// 均匀随机
    Random,
    /// 最少在途连接数：选当前并发请求最少的候选
    LeastConnections,
}

impl Default for LbStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
}

/// 会话标识来源（用于粘性路由，确保重试命中同一上游）。
///
/// 采用内部标签序列化：`{"type": "header", "value": "x-session-id"}`
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
    fn default() -> Self {
        Self::FirstUserMessage
    }
}

/// 会话粘性配置。
///
/// 启用后，同一会话的请求始终路由到首次选中的上游，
/// 避免重试/多轮对话时上下文丢失或上游缓存失效。
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
            ttl_secs: 1800,
        }
    }
}

/// 负载均衡配置。
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
            enabled: false,
            strategy: LbStrategy::RoundRobin,
            sticky: StickyConfig::default(),
        }
    }
}

// ─────────────────────────── 客户端兼容层配置 ───────────────────────────

/// Claude Code / Codex 等客户端的 API 兼容配置。
///
/// 代理在 `/v1/messages`（Anthropic API，Claude Code）与 `/v1/responses`
/// （OpenAI Responses API，Codex）上提供协议翻译：请求先翻译成
/// chat completions 格式再走正常的路由 / 负载均衡 / 熔断 / 重试链路，
/// 响应再翻译回对应协议。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompatConfig {
    /// Claude Code 请求的模型（如 `claude-sonnet-4-5-20250929`）未命中任何路由时，
    /// 改写为该模型再走正常路由解析——让 `claude-*` 模型名透明落到代理已配置的
    /// 模型上。空字符串表示不改写（未命中即返回 404）。
    pub claude_code_default_model: String,
    /// Codex（Responses API）同上：请求模型未命中路由时的兜底模型。
    pub codex_default_model: String,
    /// Claude Code 的模型上下文窗口（tokens），一键接入时写为
    /// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 环境变量。
    ///
    /// Claude Code 对不在其内置模型目录里的模型（如 glm-*）按 200k 假设并在该处
    /// 自动压缩；模型实际窗口更大时在此填写，避免过早压缩。0 = 不写（沿用 200k）。
    pub claude_code_max_context_tokens: u64,
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            claude_code_default_model: String::new(),
            codex_default_model: String::new(),
            claude_code_max_context_tokens: 0,
        }
    }
}

// ─────────────────────────── 顶层配置 ───────────────────────────

/// 顶层应用配置。存为 JSON，GUI 编辑后写回并热重载。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub retry: RetryConfig,
    pub providers: Vec<ProviderConfig>,
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub load_balance: LoadBalanceConfig,
    /// 客户端兼容层（Claude Code / Codex）配置
    #[serde(default)]
    pub compat: CompatConfig,
    /// 用户列表（每人持有一个 API Key，可选携带；用于配额限制）
    ///
    /// `alias = "api_keys"` 用于读取改名前的旧配置文件，避免升级后用户数据丢失。
    #[serde(default, alias = "api_keys")]
    pub users: Vec<UserConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            retry: RetryConfig::default(),
            providers: vec![ProviderConfig::demo()],
            routes: vec![],
            load_balance: LoadBalanceConfig::default(),
            compat: CompatConfig::default(),
            users: vec![],
        }
    }
}

// ─────────────────────────── 客户端 API Key 配置 ───────────────────────────

/// 一个用户（User）。
///
/// 每个用户持有一个 API Key。客户端调用代理时**可以**在
/// `Authorization: Bearer <key>` 中携带它（非强制）。
/// 携带了已登记 Key 的请求会按 `max_requests` 做配额限制；
/// 未携带 Key 的请求不限次（除非开启了 `server.require_api_key`）。
///
/// 注意：已用次数不存放在这里，而是运行时维护在 [`crate::proxy::state::AppState`] 中，
/// 避免 GUI 保存配置时把计数覆盖回旧值（前端提交的 config 是此前获取的快照）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    /// 唯一标识（用于配额计数与界面操作）
    pub id: String,
    /// 用户备注名，便于识别归属
    pub name: String,
    /// 该用户的 API Key；客户端以 Bearer 方式携带
    pub key: String,
    /// 是否启用；停用后该用户的 Key 视为无效
    pub enabled: bool,
    /// 最大请求数；`None` 表示不限制
    #[serde(default)]
    pub max_requests: Option<u64>,
}

impl UserConfig {
    /// 该用户是否有配额上限
    pub fn is_limited(&self) -> bool {
        self.max_requests.is_some()
    }
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            key: String::new(),
            enabled: true,
            max_requests: None,
        }
    }
}

// ─────────────────────────── 服务端配置 ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// 可选的本地访问令牌。为空则本机无需鉴权。
    pub local_api_key: String,
    /// 是否强制要求请求携带已配置的 API Key。
    ///
    /// 默认 `false`。关闭时，未携带 Key 或携带了未登记 Key 的请求会被当作
    /// 匿名请求放行——这是为了兼容「客户端总是自带 Authorization 头」的现实情况
    /// （多数 OpenAI SDK 即使连本地代理也会带上自己的 key）。
    ///
    /// 但这也意味着：一旦配置了带配额的 API Key，客户端只要改发一个随机 Key
    /// 就能绕过配额。因此**要让 API Key 的配额真正生效，必须开启本项**。
    #[serde(default)]
    pub require_api_key: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8787,
            local_api_key: String::new(),
            require_api_key: false,
        }
    }
}

// ─────────────────────────── 重试与熔断配置 ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    /// 同渠道退避重试的最大次数（不含首次）；0 表示失败立即 failover
    pub max_retries: u32,
    /// 可重试的 HTTP 状态码（同渠道退避重试）。
    ///
    /// 注意：**503 不在此列**——503 由熔断器接管（单次立即跳闸 + 换渠道），
    /// 无论本列表是否包含 503 都不影响该行为；列表只管其余状态码。
    pub retry_statuses: Vec<u16>,
    pub circuit_breaker: CircuitBreakerConfig,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_statuses: vec![408, 429, 500, 502, 529],
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }
}

/// 熔断器配置。
///
/// 熔断器像一个电路保险丝：
/// - 上游返回 **503** → **单次立即跳闸**（与 `retry_statuses` 无关，由熔断器接管）；
/// - 其它错误连续失败达 `failure_threshold` 次 → 跳闸；
/// - 跳闸后持续 `open_duration_secs`（默认 1 小时），期间新请求绕开该上游；
/// - 期满后进入 HalfOpen 半开状态：主动探活任务与真实请求逐个试探，
///   连续成功 `recovery_success_threshold` 次才完全恢复（Closed），并叠加慢启动；
/// - 全部候选渠道均熔断时进入 Panic 模式：最早熔断的渠道按
///   `panic_probe_window_secs` 窗口限速放行试探请求，避免把唯一渠道硬拒整个熔断时长。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    /// 非 503 错误的连续失败跳闸阈值
    pub failure_threshold: u32,
    /// 熔断持续时间（秒），默认 3600（1 小时）。
    ///
    /// 旧配置字段名 `cooldown_secs` 通过 serde alias 无损读入。
    #[serde(alias = "cooldown_secs")]
    pub open_duration_secs: u64,
    /// 主动探活任务的轮询间隔（秒）
    pub probe_interval_secs: u64,
    /// 半开状态恢复（转 Closed）所需的连续成功次数
    pub recovery_success_threshold: u32,
    /// Panic 试探窗口（秒）：同一熔断器在该窗口内最多放行 1 个 panic 请求
    pub panic_probe_window_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: 5,
            open_duration_secs: 3600,
            probe_interval_secs: 30,
            recovery_success_threshold: 2,
            panic_probe_window_secs: 60,
        }
    }
}

// ─────────────────────────── Provider 配置 ───────────────────────────

/// 上游模型服务商
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    /// OpenAI 兼容的 base url，如 https://api.openai.com/v1
    pub base_url: String,
    pub api_key: String,
    /// 单次请求超时（秒）
    pub timeout_secs: u64,
    pub enabled: bool,
    pub models: Vec<String>,
    /// 模型类别缓存（模型 id → `text` / `multimodal` / `image`）。
    ///
    /// 「获取模型列表」时从上游模态字段推断并随配置保存，让界面在
    /// 不重新拉取的情况下也能展示类别；仅用于展示，路由与转发不使用。
    /// 手动添加的模型无条目；未获取过时为空表。
    #[serde(default)]
    pub model_categories: std::collections::BTreeMap<String, String>,
    /// 转发到该上游时使用的 User-Agent。
    ///
    /// 为空则表示"跟随客户端"：客户端带了就用客户端的，没带则回落到
    /// [`headers::DEFAULT_USER_AGENT`](crate::proxy::headers::DEFAULT_USER_AGENT)。
    #[serde(default)]
    pub user_agent: String,
    /// 附加到上游请求的自定义头（键不区分大小写）。
    ///
    /// 用于满足特定上游的要求（如某些中转要求 `HTTP-Referer`），
    /// 也可手工伪装/补齐任意头；优先级高于客户端透传头。
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl ProviderConfig {
    pub fn demo() -> Self {
        Self {
            id: "openai".into(),
            name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            timeout_secs: 300,
            enabled: true,
            models: vec!["gpt-4o".into(), "gpt-4o-mini".into(), "gpt-4.1".into()],
            model_categories: std::collections::BTreeMap::new(),
            user_agent: String::new(),
            headers: std::collections::BTreeMap::new(),
        }
    }

    /// 拼接端点：base_url + "/chat/completions"（容忍 path 带/不带前导斜杠；
    /// 若 base_url 已含完整端点路径，直接使用，兼容误填场景）
    pub fn endpoint(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let p = path.trim_start_matches('/');
        if base.ends_with(&format!("/{p}")) {
            return base.to_string();
        }
        format!("{base}/{p}")
    }
}

// ─────────────────────────── 路由配置 ───────────────────────────

/// 模型 → 上游的路由规则。
///
/// 同一 `model` 可配置多条路由（指向不同 provider 或不同 upstream_model），
/// 由负载均衡器在它们之间分配请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    pub model: String,
    pub provider: String,
    /// 转发到上游时使用的模型名（可选）；为空则不改写，沿用客户端请求的 model
    #[serde(default)]
    pub upstream_model: String,
    /// 参数改写（可选）：temperature / max_tokens 等，合并进请求体
    #[serde(default)]
    pub overrides: serde_json::Map<String, serde_json::Value>,
    /// 负载均衡权重（strategy = Weighted 时生效，默认 1）
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 是否禁用此路由参与负载均衡（设为 true 则仅作为 fallback）
    #[serde(default)]
    pub disabled: bool,
}

fn default_weight() -> u32 {
    1
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            provider: String::new(),
            upstream_model: String::new(),
            overrides: serde_json::Map::new(),
            weight: 1,
            disabled: false,
        }
    }
}

// ─────────────────────────── 候选路由解析 ───────────────────────────

/// 一个候选路由（provider + 可选 route），用于负载均衡选择。
///
/// 全字段 Copy（两个引用 + 索引），便于在 eligible 子集上按值传递。
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub provider: &'a ProviderConfig,
    pub route: Option<&'a RouteConfig>,
    /// 在候选列表中的索引（用于 sticky 记录）
    pub index: usize,
}

impl AppConfig {
    /// 返回模型对应的所有候选路由（用于负载均衡）。
    ///
    /// # 解析顺序
    /// 1. **精确路由匹配**：收集 `routes` 中 `model` 精确匹配的条目（排除 `disabled`）
    /// 2. **隐式候选回退**：若无精确路由命中，扫描 `providers[].models` 作为隐式候选
    ///
    /// # 去重
    /// route_key（`model|provider_id|upstream_model`，与 `lb::route_key` 一致）
    /// 相同的候选只保留首条：SWRR current_weight、熔断器、在途计数、延迟统计
    /// 全部以 route_key 为键，重复候选会共享同一份状态——SWRR 的并列取靠后
    /// 会使权重完全反转、流量塌缩到最后一条上。
    ///
    /// # 返回
    /// 候选列表（可能为空）。同一模型可有多条路由，形成负载均衡候选集。
    pub fn resolve_candidates(&self, model: &str) -> Vec<Candidate<'_>> {
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // 精确路由匹配
        for (i, route) in self.routes.iter().enumerate() {
            if route.model != model || route.disabled {
                continue;
            }
            if let Some(p) = self
                .providers
                .iter()
                .find(|p| p.id == route.provider && p.enabled)
            {
                let key = format!("{model}|{}|{}", p.id, route.upstream_model);
                if !seen.insert(key.clone()) {
                    tracing::debug!(model, provider = %p.id, upstream_model = %route.upstream_model, "duplicate route_key, keeping first route only");
                    continue;
                }
                result.push(Candidate {
                    provider: p,
                    route: Some(route),
                    index: i,
                });
            }
        }

        // 若无精确路由，扫描 provider 模型表
        if result.is_empty() {
            for p in self.providers.iter().filter(|p| p.enabled) {
                if p.models.iter().any(|m| m == model) {
                    let key = format!("{model}|{}|", p.id);
                    if !seen.insert(key.clone()) {
                        tracing::debug!(model, provider = %p.id, "duplicate implicit candidate, keeping first only");
                        continue;
                    }
                    result.push(Candidate {
                        provider: p,
                        route: None,
                        index: result.len(),
                    });
                }
            }
        }

        result
    }

    /// 找到一个模型对应的 Provider（先精确路由，再扫 Provider 模型表）。
    ///
    /// 向后兼容方法：返回首个候选。新代码应优先使用 [`resolve_candidates`]。
    pub fn resolve_provider(&self, model: &str) -> Option<(&ProviderConfig, Option<&RouteConfig>)> {
        self.resolve_candidates(model)
            .into_iter()
            .next()
            .map(|c| (c.provider, c.route))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_route_exact() {
        let cfg = AppConfig {
            providers: vec![
                ProviderConfig {
                    id: "a".into(),
                    models: vec!["m1".into()],
                    ..ProviderConfig::demo()
                },
                ProviderConfig {
                    id: "b".into(),
                    models: vec!["m2".into()],
                    ..ProviderConfig::demo()
                },
            ],
            routes: vec![RouteConfig {
                model: "m1".into(),
                provider: "b".into(),
                upstream_model: String::new(),
                overrides: serde_json::Map::new(),
                weight: 1,
                disabled: false,
            }],
            ..AppConfig::default()
        };
        let (p, r) = cfg.resolve_provider("m1").unwrap();
        assert_eq!(p.id, "b");
        assert!(r.is_some());

        // 未配置路由时按 provider.models 找
        let (p2, r2) = cfg.resolve_provider("m2").unwrap();
        assert_eq!(p2.id, "b");
        assert!(r2.is_none());

        assert!(cfg.resolve_provider("nope").is_none());
    }

    #[test]
    fn resolve_candidates_multiple_routes() {
        let cfg = AppConfig {
            providers: vec![
                ProviderConfig {
                    id: "zhipu-a".into(),
                    name: "智谱A".into(),
                    ..ProviderConfig::demo()
                },
                ProviderConfig {
                    id: "zhipu-b".into(),
                    name: "智谱B".into(),
                    ..ProviderConfig::demo()
                },
                ProviderConfig {
                    id: "siliconflow".into(),
                    name: "硅基流动".into(),
                    ..ProviderConfig::demo()
                },
            ],
            routes: vec![
                RouteConfig {
                    model: "glm-5.2".into(),
                    provider: "zhipu-a".into(),
                    upstream_model: "glm-4-plus".into(),
                    ..Default::default()
                },
                RouteConfig {
                    model: "glm-5.2".into(),
                    provider: "zhipu-b".into(),
                    upstream_model: "glm-4-plus".into(),
                    ..Default::default()
                },
                RouteConfig {
                    model: "glm-5.2".into(),
                    provider: "siliconflow".into(),
                    upstream_model: "zhipuai/glm-4-plus".into(),
                    ..Default::default()
                },
            ],
            ..AppConfig::default()
        };

        let candidates = cfg.resolve_candidates("glm-5.2");
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].provider.id, "zhipu-a");
        assert_eq!(candidates[1].provider.id, "zhipu-b");
        assert_eq!(candidates[2].provider.id, "siliconflow");
        // upstream_model 各不同
        assert_eq!(
            candidates[2].route.unwrap().upstream_model,
            "zhipuai/glm-4-plus"
        );
    }

    #[test]
    fn resolve_candidates_skips_disabled() {
        let cfg = AppConfig {
            providers: vec![
                ProviderConfig {
                    id: "a".into(),
                    ..ProviderConfig::demo()
                },
                ProviderConfig {
                    id: "b".into(),
                    ..ProviderConfig::demo()
                },
            ],
            routes: vec![
                RouteConfig {
                    model: "m1".into(),
                    provider: "a".into(),
                    disabled: false,
                    ..Default::default()
                },
                RouteConfig {
                    model: "m1".into(),
                    provider: "b".into(),
                    disabled: true, // 被禁用
                    ..Default::default()
                },
            ],
            ..AppConfig::default()
        };

        let candidates = cfg.resolve_candidates("m1");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider.id, "a");
    }

    #[test]
    fn resolve_candidates_dedups_identical_route_keys() {
        // 两条 model+provider+upstream_model 完全相同、仅权重不同的路由：
        // SWRR current_weight / 熔断器 / 在途计数都以 route_key 为键，
        // 重复候选共享状态会导致权重完全反转、流量塌缩——解析期去重保留首条。
        let cfg = AppConfig {
            providers: vec![ProviderConfig {
                id: "p".into(),
                ..ProviderConfig::demo()
            }],
            routes: vec![
                RouteConfig {
                    model: "m".into(),
                    provider: "p".into(),
                    upstream_model: "u".into(),
                    weight: 9,
                    ..Default::default()
                },
                RouteConfig {
                    model: "m".into(),
                    provider: "p".into(),
                    upstream_model: "u".into(),
                    weight: 1,
                    ..Default::default()
                },
            ],
            ..AppConfig::default()
        };

        let candidates = cfg.resolve_candidates("m");
        assert_eq!(candidates.len(), 1, "重复 route_key 的候选应被去重");
        assert_eq!(candidates[0].route.unwrap().weight, 9, "应保留首条路由");

        // upstream_model 不同的路由不算重复，仍然都保留
        let cfg2 = AppConfig {
            routes: vec![
                RouteConfig {
                    model: "m".into(),
                    provider: "p".into(),
                    upstream_model: "u1".into(),
                    ..Default::default()
                },
                RouteConfig {
                    model: "m".into(),
                    provider: "p".into(),
                    upstream_model: "u2".into(),
                    ..Default::default()
                },
            ],
            ..cfg
        };
        assert_eq!(cfg2.resolve_candidates("m").len(), 2);
    }

    #[test]
    fn legacy_config_still_loads() {
        // 改名前导出的配置文件：字段叫 api_keys；retry 含已删除字段
        // （base_delay_ms/max_delay_ms/retry_on_network_error）；熔断时长字段叫 cooldown_secs。
        // 这些都必须仍能读回，否则老用户升级一次就会丢配置。
        let legacy = r#"{
            "server": { "host": "127.0.0.1", "port": 8787, "local_api_key": "" },
            "retry": {
                "max_retries": 3, "base_delay_ms": 500, "max_delay_ms": 8000,
                "retry_statuses": [429, 503], "retry_on_network_error": true,
                "circuit_breaker": { "enabled": true, "failure_threshold": 5, "cooldown_secs": 30 }
            },
            "providers": [],
            "routes": [],
            "load_balance": {
                "enabled": false, "strategy": "round_robin",
                "sticky": { "enabled": true, "session_key_source": { "type": "first_user_message" }, "ttl_secs": 1800 }
            },
            "api_keys": [
                { "id": "alice", "name": "Alice", "key": "sk-tp-abc", "enabled": true, "max_requests": 100 }
            ]
        }"#;
        let cfg: AppConfig = serde_json::from_str(legacy).expect("legacy config must parse");
        assert_eq!(cfg.users.len(), 1, "旧字段 api_keys 应被读入 users");
        assert_eq!(cfg.users[0].id, "alice");
        assert_eq!(cfg.users[0].max_requests, Some(100));
        // cooldown_secs 通过 alias 读入 open_duration_secs；用户旧值原样保留
        assert_eq!(cfg.retry.circuit_breaker.open_duration_secs, 30);
        // 新增字段缺失时取默认值
        assert_eq!(cfg.retry.circuit_breaker.probe_interval_secs, 30);
        assert_eq!(cfg.retry.circuit_breaker.recovery_success_threshold, 2);
        assert_eq!(cfg.retry.circuit_breaker.panic_probe_window_secs, 60);
        // 旧列表里的 503 无害保留（由熔断器接管，不参与同渠道重试判定）
        assert!(cfg.retry.retry_statuses.contains(&503));
    }

    #[test]
    fn endpoint_joins_and_tolerates_full_url() {
        let p = ProviderConfig {
            base_url: "https://api.example.com/v1".into(),
            ..ProviderConfig::demo()
        };
        assert_eq!(
            p.endpoint("chat/completions"),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            p.endpoint("/chat/completions"),
            p.endpoint("chat/completions")
        );

        // 误填完整端点时不再重复拼接
        let p2 = ProviderConfig {
            base_url: "https://token.example.cn/v1/chat/completions".into(),
            ..ProviderConfig::demo()
        };
        assert_eq!(
            p2.endpoint("chat/completions"),
            "https://token.example.cn/v1/chat/completions"
        );
    }
}
