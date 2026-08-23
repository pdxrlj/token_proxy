use serde::{Deserialize, Serialize};

/// 顶层应用配置。存为 JSON，GUI 编辑后写回并热重载。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub retry: RetryConfig,
    pub providers: Vec<ProviderConfig>,
    pub routes: Vec<RouteConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            retry: RetryConfig::default(),
            providers: vec![ProviderConfig::demo()],
            routes: vec![],
        }
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    pub enabled: bool,
    /// 连续失败达到该次数后熔断
    pub failure_threshold: u32,
    /// 熔断冷却时间（秒）
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

/// 模型 → 上游的路由规则
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
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            provider: String::new(),
            upstream_model: String::new(),
            overrides: serde_json::Map::new(),
        }
    }
}

impl AppConfig {
    /// 找到一个模型对应的 Provider（先精确路由，再扫 Provider 模型表）
    pub fn resolve_provider(&self, model: &str) -> Option<(&ProviderConfig, Option<&RouteConfig>)> {
        if let Some(route) = self.routes.iter().find(|r| r.model == model) {
            if let Some(p) = self
                .providers
                .iter()
                .find(|p| p.id == route.provider && p.enabled)
            {
                return Some((p, Some(route)));
            }
        }
        self.providers
            .iter()
            .filter(|p| p.enabled)
            .find(|p| p.models.iter().any(|m| m == model))
            .map(|p| (p, None))
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
