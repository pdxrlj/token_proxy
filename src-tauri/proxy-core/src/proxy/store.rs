use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use super::config::AppConfig;

/// 配置文件路径（桌面应用配置目录 /config.json）
pub fn config_path() -> PathBuf {
    let dir = directories::ProjectDirs::from("dev", "tokenproxy", "token-proxy")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("config.json")
}

/// 读取配置；不存在时写默认配置并返回
pub fn load_config(path: &PathBuf) -> Result<AppConfig, String> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let cfg = AppConfig::default();
        save_config(path, &cfg)?;
        return Ok(cfg);
    }
    let text = fs::read_to_string(path).map_err(|e| format!("read config: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse config: {e}"))
}

/// 原子写配置（临时文件 + rename）
pub fn save_config(path: &PathBuf, cfg: &AppConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("write config: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename config: {e}"))?;
    Ok(())
}

/// 配置校验（GUI 保存前调用）
pub fn validate(cfg: &AppConfig) -> Result<(), String> {
    if !(1..=65535).contains(&cfg.server.port) {
        return Err("端口必须在 1-65535 之间".into());
    }
    let mut ids = std::collections::HashSet::new();
    for p in &cfg.providers {
        if p.id.trim().is_empty() {
            return Err("Provider id 不能为空".into());
        }
        if !ids.insert(p.id.clone()) {
            return Err(format!("Provider id '{}' 重复", p.id));
        }
        if p.base_url.trim().is_empty() {
            return Err(format!("Provider '{}' 的 base_url 不能为空", p.name));
        }
        if !p.base_url.starts_with("http://") && !p.base_url.starts_with("https://") {
            return Err(format!(
                "Provider '{}' 的 base_url 必须是 http(s):// 开头",
                p.name
            ));
        }
        // 常见误填：把完整端点当成 base_url
        for bad in [
            "/chat/completions",
            "/v1/models",
            "/embeddings",
            "/completions",
        ] {
            if p.base_url.ends_with(bad) {
                return Err(format!(
                    "Provider '{}' 的 base_url 应填服务商根地址（如 https://api.openai.com/v1），不要带端点路径（{bad}）",
                    p.name
                ));
            }
        }
    }
    // 重试次数上限：防止配置过大导致重试风暴（单请求阻塞数十分钟）
    if cfg.retry.max_retries > 30 {
        return Err(format!(
            "最大重试次数 {} 过大，建议不超过 30（当前值会导致单请求最多重试 {} 次）",
            cfg.retry.max_retries, cfg.retry.max_retries
        ));
    }
    // 熔断阈值上限：阈值过大时熔断器形同虚设
    if cfg.retry.circuit_breaker.enabled && cfg.retry.circuit_breaker.failure_threshold > 50 {
        return Err(format!(
            "熔断失败阈值 {} 过大，建议不超过 50（阈值过大时熔断器几乎不会触发）",
            cfg.retry.circuit_breaker.failure_threshold
        ));
    }
    for r in &cfg.routes {
        if r.model.trim().is_empty() {
            return Err("路由 model 不能为空".into());
        }
        if !cfg.providers.iter().any(|p| p.id == r.provider) {
            return Err(format!(
                "路由 '{}' 指向不存在的 provider '{}'",
                r.model, r.provider
            ));
        }
        if r.weight == 0 {
            return Err(format!(
                "路由 '{}' 的 weight 不能为 0（负载均衡加权策略下会被忽略）",
                r.model
            ));
        }
    }
    Ok(())
}

/// 状态快照（供 GUI 展示）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusDto {
    pub running: bool,
    pub host: String,
    pub port: u16,
    pub config_path: String,
    pub uptime_secs: u64,
    pub providers: usize,
    pub routes: usize,
    pub total_requests: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::{AppConfig, ProviderConfig, RouteConfig};

    fn base_cfg() -> AppConfig {
        AppConfig {
            server: crate::proxy::config::ServerConfig::default(),
            providers: vec![ProviderConfig {
                id: "p1".into(),
                name: "P1".into(),
                base_url: "https://api.example.com/v1".into(),
                api_key: "sk-x".into(),
                timeout_secs: 60,
                enabled: true,
                models: vec![],
            }],
            routes: vec![RouteConfig {
                model: "m1".into(),
                provider: "p1".into(),
                upstream_model: String::new(),
                overrides: serde_json::Map::new(),
                weight: 1,
                disabled: false,
            }],
            retry: Default::default(),
            load_balance: Default::default(),
        }
    }

    #[test]
    fn validate_rejects_excessive_max_retries() {
        let mut cfg = base_cfg();
        cfg.retry.max_retries = 300;
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.contains("最大重试次数"),
            "应拒绝过大的 max_retries: {err}"
        );
    }

    #[test]
    fn validate_rejects_excessive_failure_threshold() {
        let mut cfg = base_cfg();
        cfg.retry.circuit_breaker.enabled = true;
        cfg.retry.circuit_breaker.failure_threshold = 300;
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.contains("熔断失败阈值"),
            "应拒绝过大的 failure_threshold: {err}"
        );
    }

    #[test]
    fn validate_accepts_boundary_values() {
        let mut cfg = base_cfg();
        cfg.retry.max_retries = 30;
        cfg.retry.circuit_breaker.enabled = true;
        cfg.retry.circuit_breaker.failure_threshold = 50;
        validate(&cfg).expect("边界值 30/50 应通过校验");
    }

    #[test]
    fn validate_skips_threshold_check_when_breaker_disabled() {
        let mut cfg = base_cfg();
        cfg.retry.circuit_breaker.enabled = false;
        cfg.retry.circuit_breaker.failure_threshold = 9999;
        validate(&cfg).expect("熔断器未启用时不应校验阈值");
    }
}
