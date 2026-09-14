use serde::{Deserialize, Serialize};
use std::fs;
use std::net::ToSocketAddrs;
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
    let host = cfg.server.host.trim();
    if host.is_empty() {
        return Err("监听地址不能为空".into());
    }
    // 校验监听地址可解析；避免保存后服务重启失败
    if format!("{host}:{}", cfg.server.port)
        .to_socket_addrs()
        .map(|mut a| a.next().is_none())
        .unwrap_or(true)
    {
        return Err(format!(
            "监听地址 '{host}' 不是合法的 IP 或域名"
        ));
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
    // 用户校验：id 与 key 均需唯一，否则鉴权时会命中歧义条目
    let mut user_ids = std::collections::HashSet::new();
    let mut user_keys = std::collections::HashSet::new();
    for u in &cfg.users {
        if u.id.trim().is_empty() {
            return Err("用户 id 不能为空".into());
        }
        if !user_ids.insert(u.id.clone()) {
            return Err(format!("用户 id '{}' 重复", u.id));
        }
        if u.key.trim().is_empty() {
            return Err(format!("用户 '{}' 的 API Key 不能为空", u.id));
        }
        if !user_keys.insert(u.key.clone()) {
            return Err(format!("用户 '{}' 的 API Key 与其它用户重复", u.id));
        }
        if let Some(0) = u.max_requests {
            return Err(format!(
                "用户 '{}' 的请求上限不能为 0（请留空表示不限，或填正整数）",
                u.id
            ));
        }
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
        // weight = 0 是合法配置：表示该路由不参与常规分发，
        // 仅在上游全部失败时作为 failover 兜底使用（配合 disabled=false）。
        // 这里只拦截超出 u32 范围或明显异常的数值。
        if r.weight > 1_000_000 {
            return Err(format!("路由 '{}' 的 weight 过大（上限 1000000）", r.model));
        }
    }

    // ── 重试与熔断参数边界 ──
    if cfg.retry.max_retries > 10 {
        return Err("最大重试次数不能超过 10".into());
    }
    let cb = &cfg.retry.circuit_breaker;
    if cb.failure_threshold == 0 || cb.failure_threshold > 100 {
        return Err("熔断失败阈值必须在 1-100 之间".into());
    }
    // 0 合法：表示熔断后立即转半开试探（快速恢复场景）
    if cb.open_duration_secs > 7 * 86400 {
        return Err("熔断时长（秒）不能超过 7 天（604800）".into());
    }
    if cb.probe_interval_secs == 0 || cb.probe_interval_secs > 3600 {
        return Err("探活间隔（秒）必须在 1-3600 之间".into());
    }
    if cb.recovery_success_threshold == 0 || cb.recovery_success_threshold > 10 {
        return Err("恢复所需连续成功次数必须在 1-10 之间".into());
    }
    if cb.panic_probe_window_secs > 3600 {
        return Err("Panic 试探窗口（秒）不能超过 3600".into());
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
    /// 服务启动失败的原因（如端口被占用）；正常运行时为 null。
    ///
    /// 加 `default` 是为了兼容旧版前端/配置中不含该字段的场景。
    #[serde(default)]
    pub server_error: Option<String>,
    /// 本机局域网 IPv4 地址（不含回环）；供前端展示局域网访问入口。
    #[serde(default)]
    pub lan_ips: Vec<String>,
}
