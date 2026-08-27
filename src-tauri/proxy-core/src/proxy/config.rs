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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            retry: RetryConfig::default(),
            providers: vec![ProviderConfig::demo()],
            routes: vec![],
            load_balance: LoadBalanceConfig::default(),
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8787,
            local_api_key: String::new(),
        }
    }
}

// ─────────────────────────── 重试与熔断配置 ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    pub max_retries: u32,
    /// 指数退避基数（毫秒）
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    /// 可重试的 HTTP 状态码
    pub retry_statuses: Vec<u16>,
    /// 网络层错误（连接失败/超时/断流）也重试
    pub retry_on_network_error: bool,
    pub circuit_breaker: CircuitBreakerConfig,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 500,
            max_delay_ms: 8000,
            retry_statuses: vec![408, 429, 500, 502, 503, 529],
            retry_on_network_error: true,
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }
}

/// 熔断器配置。
///
/// 熔断器像一个电路保险丝：当某个上游连续失败达到 `failure_threshold` 次后，
/// 自动“跳闸”进入 Open 状态，暂停对该上游的请求；经过 `cooldown_secs` 秒冷却后
/// 进入 HalfOpen 半开状态，放行一个试探请求——成功则恢复正常（Closed），
/// 失败则重新跳闸。这样可以快速隔离故障上游，避免持续向故障服务发送请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    /// 连续失败达到该次数后熔断（跳闸），暂停对该上游的请求
    pub failure_threshold: u32,
    /// 熔断冷却时间（秒）；冷却后进入半开状态，放行一个试探请求
    pub cooldown_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: 5,
            cooldown_secs: 30,
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
#[derive(Debug, Clone)]
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
    /// # 返回
    /// 候选列表（可能为空）。同一模型可有多条路由，形成负载均衡候选集。
    pub fn resolve_candidates(&self, model: &str) -> Vec<Candidate<'_>> {
        let mut result = Vec::new();

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
