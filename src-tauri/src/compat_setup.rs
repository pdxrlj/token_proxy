//! 一键接入 Claude Code / Codex（cc-switch 同款思路：直接替用户改写 CLI 的
//! 本地配置文件，把 API 地址指向本代理，免去手动设环境变量 / 编辑 TOML）。
//!
//! # 写入内容
//! - **Claude Code**：`~/.claude/settings.json` 的 `env` 块合并
//!   `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` 两个键，其余配置原样保留
//! - **Codex**：`~/.codex/config.toml` 写入 `model_provider = "tokenproxy"` 与
//!   `[model_providers.tokenproxy]`（`name` / `base_url` / `wire_api = "responses"`），
//!   用 toml_edit 保格式改写，用户已有的注释与其它配置不受影响；
//!   `~/.codex/auth.json` 写 `{"OPENAI_API_KEY": "<令牌>"}` 供 Codex 读取鉴权
//!
//! # 安全性
//! - 首次接入前把原文件快照到 `<file>.tokenproxy.bak`（已存在则不覆盖，
//!   始终保留用户最早的状态）；文件原本不存在时不建快照
//! - 断开时优先从快照恢复（恢复后删除快照）；无快照时仅移除本代理写入的键，
//!   不碰用户其它内容
//! - 全部原子写（临时文件 + rename），进程中断不会留下半截文件
//!
//! 修改对 CLI 的生效时机与 cc-switch 一致：**重启 Claude Code / Codex 后生效**。

use serde::Serialize;
use std::path::{Path, PathBuf};

/// 写入 CLI 配置时使用的 provider id / 来源标识
const PROVIDER_ID: &str = "tokenproxy";
/// 本地访问令牌为空时的占位值（代理侧未启用鉴权时任意义字符串均可）
pub const PLACEHOLDER_TOKEN: &str = "tokenproxy";

/// modelPicker 行的 behavesAs 目标：Claude Code 目录内的通用档位模型 ID。
///
/// 其 prompt profile / 能力 / effort 默认值套用到代理的自定义模型上；
/// 模型目录是版本内固定的已知 ID 集合（从本机 v2.1.283 二进制提取确认）。
const BEHAVES_AS_MODEL: &str = "claude-sonnet-4-6";

/// 早期版本接入写入的模型槽位 env（现役接入不再生成）。
///
/// 这些键会让 Claude Code 在 /model 选择器里各生成一行 Custom
/// Opus/Sonnet/Haiku model，并按模型 ID 去重吞掉同模型的 modelPicker 行
/// （如兜底模型 glm-5.2 被折进 Custom Opus 行，列表里就不再单独出现），
/// 因此接入/刷新与断开清理都要移除它们。
const LEGACY_CLAUDE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
];

// ─────────────────────────── 路径 ───────────────────────────

/// 用户主目录（Windows = %USERPROFILE%，Unix = $HOME）
pub fn home_dir() -> Result<PathBuf, String> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .map_err(|_| "无法定位用户主目录（USERPROFILE / HOME 均未设置）".to_string())
}

pub fn claude_settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

pub fn codex_config_path(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

pub fn codex_auth_path(home: &Path) -> PathBuf {
    home.join(".codex").join("auth.json")
}

/// 快照路径：`settings.json` → `settings.json.tokenproxy.bak`（同目录）
fn backup_path(p: &Path) -> PathBuf {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    p.with_file_name(format!("{name}.{PROVIDER_ID}.bak"))
}

// ─────────────────────────── 状态 ───────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    /// CLI 配置文件路径（前端展示用）
    pub path: String,
    /// 是否已指向本代理（base_url 与当前监听地址一致）
    pub integrated: bool,
    /// CLI 当前指向的 API 地址（未配置时为 null）
    pub base_url: Option<String>,
    /// 是否存在接入前的快照（可一键恢复）
    pub has_backup: bool,
}

/// 读取 Claude Code 的接入状态
pub fn claude_status(home: &Path, our_base_url: &str) -> ToolStatus {
    let path = claude_settings_path(home);
    let current = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v.get("env")?
                .get("ANTHROPIC_BASE_URL")?
                .as_str()
                .map(str::to_string)
        });
    ToolStatus {
        path: path.display().to_string(),
        integrated: current.as_deref() == Some(our_base_url),
        base_url: current,
        has_backup: backup_path(&path).exists(),
    }
}

/// 读取 Codex 的接入状态
pub fn codex_status(home: &Path, our_base_url: &str) -> ToolStatus {
    let path = codex_config_path(home);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let current = text.parse::<toml_edit::DocumentMut>().ok().and_then(|doc| {
        if doc.get("model_provider").and_then(|i| i.as_str()) != Some(PROVIDER_ID) {
            return None;
        }
        doc.get("model_providers")
            .and_then(|i| i.as_table_like())
            .and_then(|t| t.get(PROVIDER_ID))
            .and_then(|i| i.as_table_like())
            .and_then(|t| t.get("base_url"))
            .and_then(|i| i.as_str())
            .map(str::to_string)
    });
    ToolStatus {
        path: path.display().to_string(),
        integrated: current.as_deref() == Some(our_base_url),
        base_url: current,
        has_backup: backup_path(&path).exists(),
    }
}

// ─────────────────────────── Claude Code ───────────────────────────

/// 一键接入 Claude Code：合并配置到 `~/.claude/settings.json`。
///
/// 写入三类内容：
/// - env：`ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN`；
///   `ANTHROPIC_MODEL` = Default 使用的模型（兜底模型，未配置时取代理第一个模型）；
///   模型窗口配置过时写 `CLAUDE_CODE_MAX_CONTEXT_TOKENS`
/// - `modelPicker.options`：把代理的全部模型注册进 `/model` 选择器（schema 取自
///   本机 Claude Code v2.1.283 的 Zod 定义：行 = { model, label?, description?,
///   behavesAs? }）。`behavesAs` 指向一个该版本认识的模型 ID，其客户端处理
///   （prompt profile / 能力 / effort 默认值）套用到自定义模型上——这是
///   "isn't described by this version's model catalog" 启动警告的正式消除方式；
/// - 早期版本写入的 `ANTHROPIC_DEFAULT_OPUS/SONNET/HAIKU_MODEL` env 槽位不再
///   生成，且每次接入/刷新都会主动移除（含 `_NAME` 显示名变体）：它们会在
///   选择器里生成 Custom Opus/Sonnet/Haiku 行并按模型 ID 吞掉同模型的
///   modelPicker 行，残留时兜底模型反而在列表里看不到。
///
/// 注意：`modelPicker` 在 Claude Code 侧不跨来源合并（用户设置整体生效），
/// 若用户手工维护过自己的 modelPicker，接入会整体覆盖（快照可恢复）。
///
/// 返回写入的文件路径。文件不存在时创建（含父目录）。
pub fn integrate_claude(
    home: &Path,
    base_url: &str,
    token: &str,
    models: &[String],
    default_model: &str,
    max_context_tokens: u64,
) -> Result<PathBuf, String> {
    let path = claude_settings_path(home);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
    let mut v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("{} 不是合法的 JSON，请手工检查后再试：{e}", path.display()))?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| format!("{} 顶层必须是 JSON 对象", path.display()))?;

    // 刷新场景（文件已是本代理接入过的配置）不得再快照：快照必须始终指向
    // 用户接入前的原始状态，否则「刷新 → 断开」会把我们生成的配置当原始配置恢复
    let already_integrated = obj
        .get("env")
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .is_some();
    if !already_integrated {
        snapshot_if_needed(&path)?;
    }

    // ── env 块（作用域限制 env 的可变借用，块结束后才能写顶层 modelPicker）──
    {
        let env = obj
            .entry("env")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let env_obj = env
            .as_object_mut()
            .ok_or_else(|| format!("{} 的 env 必须是对象", path.display()))?;
        env_obj.insert(
            "ANTHROPIC_BASE_URL".to_string(),
            serde_json::Value::String(base_url.to_string()),
        );
        env_obj.insert(
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            serde_json::Value::String(token.to_string()),
        );
        if let Some(m0) = models.first().filter(|m| !m.is_empty()) {
            let default = match default_model.trim() {
                "" => m0.as_str(),
                d => d,
            };
            env_obj.insert("ANTHROPIC_MODEL".into(), serde_json::json!(default));
        }
        // 模型不在 Claude Code 内置目录时的上下文窗口声明；0 = 移除（沿用其 200k 假设），
        // 保证「填了再清空 → 刷新」能把旧值从 settings.json 里去掉
        if max_context_tokens > 0 {
            env_obj.insert(
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(),
                serde_json::json!(max_context_tokens.to_string()),
            );
        } else {
            env_obj.remove("CLAUDE_CODE_MAX_CONTEXT_TOKENS");
        }
        // 旧版接入的模型槽位（含 _NAME 显示名变体）按模型 ID 吞掉同模型的
        // modelPicker 行，刷新接入时一并清掉，保证选择器只走 modelPicker 一条路
        for k in LEGACY_CLAUDE_ENV_KEYS {
            env_obj.remove(*k);
        }
    }

    // ── modelPicker 行：代理模型全部注册进 /model 选择器（上限 20 行防失控）──
    // behavesAs 用 claude-sonnet-4-6：v2.1.x 目录内稳定的通用档位。
    let mut picker_models: Vec<String> = Vec::new();
    if !default_model.trim().is_empty() {
        picker_models.push(default_model.trim().to_string());
    }
    for m in models {
        if !m.is_empty() && !picker_models.iter().any(|x| x == m) {
            picker_models.push(m.clone());
        }
    }
    if !picker_models.is_empty() {
        let rows: Vec<serde_json::Value> = picker_models
            .iter()
            .take(20)
            .map(|m| {
                serde_json::json!({
                    "model": m,
                    "description": "Token Proxy",
                    "behavesAs": BEHAVES_AS_MODEL,
                })
            })
            .collect();
        obj.insert(
            "modelPicker".to_string(),
            serde_json::json!({ "options": rows }),
        );
    }

    let out = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    atomic_write(&path, &out)?;
    Ok(path)
}

/// 本代理写入 Claude Code 的全部 env 键（断开时按此清理；旧版槽位一并清）
const CLAUDE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
];

/// 断开 Claude Code 接入：优先恢复快照；无快照时仅移除本代理写入的 env 键。
///
/// 返回是否发生了文件修改。
pub fn disconnect_claude(home: &Path) -> Result<bool, String> {
    let path = claude_settings_path(home);
    if restore_if_snapshotted(&path)? {
        return Ok(true);
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false); // 文件不存在，无从断开
    };
    let mut v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let Some(env) = v.get_mut("env").and_then(|e| e.as_object_mut()) else {
        return Ok(false);
    };
    for k in CLAUDE_ENV_KEYS.iter().chain(LEGACY_CLAUDE_ENV_KEYS) {
        env.remove(*k);
    }
    let empty = env.is_empty();
    let obj = v
        .as_object_mut()
        .ok_or_else(|| "settings.json 顶层必须是对象".to_string())?;
    if empty {
        obj.remove("env");
    }
    obj.remove("modelPicker");
    let out = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
    atomic_write(&path, &out)?;
    Ok(true)
}

// ─────────────────────────── Codex ───────────────────────────

/// 一键接入 Codex：写 `config.toml`（保格式）与 `auth.json`。
///
/// `default_model` 非空（或代理有已配置模型时取第一个）会写入顶层 `model`，
/// 作为 Codex 启动时的默认模型；代理的兼容层也会把其它 gpt-* 模型名兜底改写。
///
/// 返回 (config.toml 路径, auth.json 路径)。文件不存在时创建（含父目录）。
pub fn integrate_codex(
    home: &Path,
    base_url: &str,
    api_key: &str,
    default_model: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let config = codex_config_path(home);
    let auth = codex_auth_path(home);
    let text = std::fs::read_to_string(&config).unwrap_or_default();

    // 快照规则同 integrate_claude：已是本代理接入标记的文件不快照
    let already_ours = text
        .parse::<toml_edit::DocumentMut>()
        .ok()
        .map(|d| d.get("model_provider").and_then(|i| i.as_str()) == Some(PROVIDER_ID))
        .unwrap_or(false);
    if !already_ours {
        snapshot_if_needed(&config)?;
    }
    // auth.json 若为本代理生成（仅 OPENAI_API_KEY、无 OAuth tokens）不快照
    let auth_ours = std::fs::read_to_string(&auth)
        .ok()
        .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
        .map(|val| val.get("OPENAI_API_KEY").is_some() && val.get("tokens").is_none())
        .unwrap_or(false);
    if !auth_ours {
        snapshot_if_needed(&auth)?;
    }

    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("{} 不是合法的 TOML，请手工检查后再试：{e}", config.display()))?;
    // 切换语义：model_provider 指向本代理（快照已保存原值，断开可恢复）
    doc["model_provider"] = toml_edit::value(PROVIDER_ID);
    if !default_model.trim().is_empty() {
        doc["model"] = toml_edit::value(default_model.trim());
    }
    if doc.get("model_providers").is_none() {
        let mut t = toml_edit::Table::new();
        // 隐式表：只渲染 [model_providers.tokenproxy]，不产生空的 [model_providers] 头
        t.set_implicit(true);
        doc.insert("model_providers", toml_edit::Item::Table(t));
    }
    let Some(mp) = doc
        .get_mut("model_providers")
        .and_then(|i| i.as_table_like_mut())
    else {
        return Err(format!(
            "{} 的 model_providers 必须是 TOML 表",
            config.display()
        ));
    };
    let mut pt = toml_edit::Table::new();
    pt.insert("name", toml_edit::value("Token Proxy"));
    pt.insert("base_url", toml_edit::value(base_url));
    pt.insert("wire_api", toml_edit::value("responses"));
    mp.insert(PROVIDER_ID, toml_edit::Item::Table(pt));

    atomic_write(&config, &doc.to_string())?;

    let auth_json = serde_json::json!({ "OPENAI_API_KEY": api_key });
    atomic_write(&auth, &serde_json::to_string_pretty(&auth_json).map_err(|e| e.to_string())?)?;
    Ok((config, auth))
}

/// 断开 Codex 接入：优先恢复快照（config.toml 与 auth.json 各自独立恢复）；
/// 无快照时仅移除本代理写入的键。
///
/// 返回是否发生了文件修改。
pub fn disconnect_codex(home: &Path) -> Result<bool, String> {
    let config = codex_config_path(home);
    let auth = codex_auth_path(home);
    let config_restored = restore_if_snapshotted(&config)?;
    if config_restored {
        // 完整恢复语义：接入时新建的 auth.json（无快照 = 原本不存在）一并移除，
        // 让 Codex 回到接入前的状态
        if !backup_path(&auth).exists() && auth.exists() {
            std::fs::remove_file(&auth).map_err(|e| format!("移除 auth.json 失败：{e}"))?;
        }
        return Ok(true);
    }
    if restore_if_snapshotted(&auth)? {
        return Ok(true);
    }

    // 无快照：仅移除我们写入的 model_provider / model / provider 表
    let Ok(text) = std::fs::read_to_string(&config) else {
        return Ok(false);
    };
    let mut doc = match text.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(_) => return Ok(false), // 用户手工改坏的文件不代改
    };
    let mut changed = false;
    if doc.get("model_provider").and_then(|i| i.as_str()) == Some(PROVIDER_ID) {
        doc.as_table_mut().remove("model_provider");
        changed = true;
    }
    // model 键无法区分是否用户自设，仅随 provider 一并移除（无快照 = 文件为本代理生成）
    if changed && doc.get("model").and_then(|i| i.as_str()).is_some() {
        doc.as_table_mut().remove("model");
    }
    if let Some(mp) = doc.get_mut("model_providers") {
        if let Some(t) = mp.as_table_like_mut() {
            if t.remove(PROVIDER_ID).is_some() {
                changed = true;
            }
        }
    }
    if changed {
        atomic_write(&config, &doc.to_string())?;
    }
    // auth.json 若为本代理生成（仅 OPENAI_API_KEY、无 OAuth tokens），一并移除，
    // 避免残留的占位密钥被 Codex 用于官方 API
    if auth.exists() {
        let looks_ours = std::fs::read_to_string(&auth)
            .ok()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
            .map(|val| val.get("OPENAI_API_KEY").is_some() && val.get("tokens").is_none())
            .unwrap_or(false);
        if looks_ours {
            std::fs::remove_file(&auth).map_err(|e| format!("移除 auth.json 失败：{e}"))?;
            changed = true;
        }
    }
    Ok(changed)
}

// ─────────────────────────── 文件操作原语 ───────────────────────────

/// 首次修改前快照原文件；快照已存在则跳过（保留用户最早的状态）
fn snapshot_if_needed(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let bak = backup_path(path);
    if bak.exists() {
        return Ok(());
    }
    std::fs::copy(path, &bak).map_err(|e| format!("备份 {} 失败：{e}", path.display()))?;
    Ok(())
}

/// 从快照恢复文件；快照不存在时不动文件。返回是否发生了恢复。
fn restore_if_snapshotted(path: &Path) -> Result<bool, String> {
    let bak = backup_path(path);
    if !bak.exists() {
        return Ok(false);
    }
    let content = std::fs::read(&bak).map_err(|e| format!("读取快照失败：{e}"))?;
    atomic_write(path, &String::from_utf8_lossy(&content))?;
    std::fs::remove_file(&bak).map_err(|e| format!("清理快照失败：{e}"))?;
    Ok(true)
}

/// 原子写（临时文件 + rename），与 store.rs 的配置写入同款
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, content).map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("替换 {} 失败：{e}", path.display()))?;
    Ok(())
}

// ─────────────────────────── 单元测试 ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试用独立的临时主目录
    fn temp_home(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tokenproxy-compat-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn claude_integrate_creates_and_merges() {
        let home = temp_home("cl-new");
        let models = vec!["glm-5.2".to_string(), "qwen3.7-plus".to_string()];
        let path =
            integrate_claude(&home, "http://127.0.0.1:8787", "tok-1", &models, "glm-5.2", 0).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(v["env"]["ANTHROPIC_AUTH_TOKEN"], "tok-1");
        // Default 模型 = 兜底模型
        assert_eq!(v["env"]["ANTHROPIC_MODEL"], "glm-5.2");
        assert!(v["env"].get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none(), "env 槽位不再写入（避免与 modelPicker 重复）");

        // modelPicker 行：兜底模型 + 代理模型列表，每行带 behavesAs 消除目录警告
        let options = v["modelPicker"]["options"].as_array().unwrap();
        let names: Vec<&str> = options.iter().map(|r| r["model"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["glm-5.2", "qwen3.7-plus"], "rows: {options:?}");
        assert_eq!(options[0]["behavesAs"], "claude-sonnet-4-6");
        assert_eq!(options[0]["description"], "Token Proxy");

        // 状态：已接入
        let st = claude_status(&home, "http://127.0.0.1:8787");
        assert!(st.integrated);
        assert!(st.has_backup == false, "原文件不存在时不建快照");
    }

    #[test]
    fn claude_integrate_removes_legacy_slot_envs() {
        // 回归：旧版接入写入的 Opus/Sonnet/Haiku 槽位（含 _NAME 显示名变体）
        // 会在 /model 选择器里生成 Custom Opus/Sonnet/Haiku 行，并按模型 ID
        // 吞掉同模型的 modelPicker 行（glm-5.2 被折进 Custom Opus 行）。
        // 刷新接入必须把它们清掉，选择器才只走 modelPicker 一条路。
        let home = temp_home("cl-legacy");
        let path = claude_settings_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"env": {"ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2", "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "qwen3.7-plus", "ANTHROPIC_DEFAULT_SONNET_MODEL": "deepseek-v4-flash", "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "qwen3.7-max", "ANTHROPIC_DEFAULT_HAIKU_MODEL": "kimi-k3", "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "kimi-k3", "KEEP": "1"}}"#,
        )
        .unwrap();

        integrate_claude(&home, "http://127.0.0.1:8787", "tok", &["glm-5.2".to_string()], "glm-5.2", 0)
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for k in LEGACY_CLAUDE_ENV_KEYS {
            assert!(v["env"].get(*k).is_none(), "{k} 应在接入时被移除");
        }
        assert_eq!(v["env"]["ANTHROPIC_MODEL"], "glm-5.2");
        assert_eq!(v["env"]["KEEP"], "1", "非本代理的 env 键不得误删");
        let options = v["modelPicker"]["options"].as_array().unwrap();
        assert_eq!(options[0]["model"], "glm-5.2", "兜底模型仍是第一行");
    }

    #[test]
    fn claude_integrate_preserves_existing_config() {
        let home = temp_home("cl-merge");
        let path = claude_settings_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
  "model": "claude-sonnet-4-5",
  "permissions": { "allow": ["Bash(ls:*)"] },
  "env": { "CUSTOM_VAR": "keep-me" }
}"#,
        )
        .unwrap();

        integrate_claude(&home, "http://127.0.0.1:8787", "tok", &["m-a".to_string()], "", 0).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // 保留用户已有配置
        assert_eq!(v["model"], "claude-sonnet-4-5");
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls:*)");
        assert_eq!(v["env"]["CUSTOM_VAR"], "keep-me");
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        // 兜底模型为空 → Default 取代理第一个模型
        assert_eq!(v["env"]["ANTHROPIC_MODEL"], "m-a");

        // 快照 = 用户最初状态；重复接入不覆盖快照
        let bak = std::fs::read_to_string(backup_path(&path)).unwrap();
        assert!(bak.contains("CUSTOM_VAR"));
        assert!(!bak.contains("ANTHROPIC_BASE_URL"));
        integrate_claude(&home, "http://127.0.0.1:9999", "tok", &["m-a".to_string()], "", 0).unwrap();
        let bak2 = std::fs::read_to_string(backup_path(&path)).unwrap();
        assert_eq!(bak, bak2, "重复接入不得覆盖最初快照");
    }

    #[test]
    fn claude_disconnect_restores_snapshot() {
        let home = temp_home("cl-disc");
        let original = r#"{"model": "claude-opus-4", "env": {"OTHER": "x"}}"#;
        let path = claude_settings_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, original).unwrap();

        integrate_claude(&home, "http://127.0.0.1:8787", "tok", &["glm-5.2".to_string()], "", 0).unwrap();
        assert!(claude_status(&home, "http://127.0.0.1:8787").integrated);

        assert!(disconnect_claude(&home).unwrap());
        let restored = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&restored).unwrap();
        assert_eq!(v["model"], "claude-opus-4");
        assert_eq!(v["env"]["OTHER"], "x");
        assert!(v["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert!(v["env"].get("ANTHROPIC_MODEL").is_none(), "模型槽位应随快照一并恢复");
        assert!(!backup_path(&path).exists(), "恢复后应清理快照");
        assert!(!claude_status(&home, "http://127.0.0.1:8787").integrated);
    }

    #[test]
    fn claude_disconnect_without_snapshot_removes_keys() {
        let home = temp_home("cl-disc-nobak");
        let path = claude_settings_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // 直接写入已接入状态（模拟快照丢失）
        std::fs::write(
            &path,
            r#"{"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:8787", "ANTHROPIC_AUTH_TOKEN": "t", "ANTHROPIC_MODEL": "glm-5.2", "ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2", "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "qwen3.7-plus", "KEEP": "1"}}"#,
        )
        .unwrap();

        assert!(disconnect_claude(&home).unwrap());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for k in ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME"] {
            assert!(v["env"].get(k).is_none(), "{k} 应被移除");
        }
        assert_eq!(v["env"]["KEEP"], "1", "其它 env 键不得误删");
    }

    #[test]
    fn codex_integrate_writes_provider_and_auth() {
        let home = temp_home("cx-new");
        let (config, auth) =
            integrate_codex(&home, "http://127.0.0.1:8787/v1", "sk-tok", "glm-5.2").unwrap();
        assert!(std::fs::read_to_string(&config).unwrap().contains("model = \"glm-5.2\""));
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("model_provider = \"tokenproxy\""), "config: {text}");
        assert!(text.contains("[model_providers.tokenproxy]"));
        assert!(text.contains("base_url = \"http://127.0.0.1:8787/v1\""));
        assert!(text.contains("wire_api = \"responses\""));
        // 必须仍是合法 TOML 且可回读
        let st = codex_status(&home, "http://127.0.0.1:8787/v1");
        assert!(st.integrated, "status: {st:?}");

        let av: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
        assert_eq!(av["OPENAI_API_KEY"], "sk-tok");
    }

    #[test]
    fn codex_integrate_preserves_user_toml() {
        let home = temp_home("cx-merge");
        let config = codex_config_path(&home);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let original = "# 我的 Codex 配置\nmodel = \"gpt-5-codex\"\n\n[mcp_servers.files]\ncommand = \"mcp-fs\"\n";
        std::fs::write(&config, original).unwrap();

        integrate_codex(&home, "http://127.0.0.1:8787/v1", "sk", "").unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        // 用户已有内容（含注释）必须原样保留
        assert!(text.contains("# 我的 Codex 配置"));
        assert!(text.contains("model = \"gpt-5-codex\""), "兜底为空时不得动用户的 model 键");
        assert!(text.contains("[mcp_servers.files]"));
        // 且整体仍是合法 TOML
        text.parse::<toml_edit::DocumentMut>().expect("合法 TOML");

        // 快照 = 用户最初内容
        let bak = std::fs::read_to_string(backup_path(&config)).unwrap();
        assert_eq!(bak, original, "快照应是接入前的原始内容");
    }

    #[test]
    fn codex_disconnect_restores_snapshot() {
        let home = temp_home("cx-disc");
        let original = "model = \"gpt-5\"\n";
        let config = codex_config_path(&home);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, original).unwrap();

        integrate_codex(&home, "http://127.0.0.1:8787/v1", "sk-old", "").unwrap();
        assert!(codex_status(&home, "http://127.0.0.1:8787/v1").integrated);

        assert!(disconnect_codex(&home).unwrap());
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            original,
            "应从快照恢复原始 config.toml"
        );
        assert!(!codex_auth_path(&home).exists(), "接入前不存在的 auth.json 应被移除");
        assert!(!codex_status(&home, "http://127.0.0.1:8787/v1").integrated);
    }

    #[test]
    fn codex_disconnect_without_snapshot_removes_keys_only() {
        let home = temp_home("cx-disc-nobak");
        let config = codex_config_path(&home);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            "model_provider = \"tokenproxy\"\n\n[model_providers]\n\n[model_providers.tokenproxy]\nname = \"Token Proxy\"\nbase_url = \"http://127.0.0.1:8787/v1\"\nwire_api = \"responses\"\n\n[model_providers.other]\nname = \"Other\"\n",
        )
        .unwrap();

        assert!(disconnect_codex(&home).unwrap());
        let text = std::fs::read_to_string(&config).unwrap();
        let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(doc.get("model_provider").is_none(), "model_provider 应被移除");
        assert!(doc.get("model").is_none(), "model 应随 provider 一并移除");
        let mp = doc.get("model_providers").unwrap();
        assert!(mp.get("tokenproxy").is_none(), "tokenproxy 表应被移除");
        assert!(mp.get("other").is_some(), "用户的其它 provider 不得误删");
        assert!(!codex_status(&home, "http://127.0.0.1:8787/v1").integrated);
    }

    #[test]
    fn claude_max_context_tokens_written_and_cleaned() {
        let home = temp_home("cl-maxctx");
        integrate_claude(
            &home,
            "http://127.0.0.1:8787",
            "tok",
            &["glm-5.2".to_string()],
            "",
            200_000,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude_settings_path(&home)).unwrap(),
        )
        .unwrap();
        assert_eq!(v["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "200000");

        // 0 = 不写（且重复接入时更新为 0 会移除旧值）
        integrate_claude(
            &home,
            "http://127.0.0.1:8787",
            "tok",
            &["glm-5.2".to_string()],
            "",
            0,
        )
        .unwrap();
        let v2: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude_settings_path(&home)).unwrap(),
        )
        .unwrap();
        assert!(v2["env"].get("CLAUDE_CODE_MAX_CONTEXT_TOKENS").is_none());

        // 断开清理列表包含该键（无快照路径）
        std::fs::write(
            claude_settings_path(&home),
            r#"{"env": {"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "200000", "KEEP": "1"}}"#,
        )
        .unwrap();
        disconnect_claude(&home).unwrap();
        let v3: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude_settings_path(&home)).unwrap(),
        )
        .unwrap();
        assert!(v3["env"].get("CLAUDE_CODE_MAX_CONTEXT_TOKENS").is_none());
        assert_eq!(v3["env"]["KEEP"], "1");
    }

    #[test]
    fn claude_refresh_then_disconnect_fully_restores() {
        // 回归：首次接入（原文件不存在 → 无快照）后点「刷新」，不得把已接入
        // 的内容当作原始状态快照，否则断开会"恢复"成接入态
        let home = temp_home("cl-refresh");
        integrate_claude(&home, "http://127.0.0.1:8787", "tok", &["glm-5.2".to_string()], "", 0)
            .unwrap();
        integrate_claude(&home, "http://127.0.0.1:8787", "tok", &["glm-5.2".to_string()], "", 0)
            .unwrap();
        assert!(
            !backup_path(&claude_settings_path(&home)).exists(),
            "刷新不得产生快照"
        );

        assert!(disconnect_claude(&home).unwrap());
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude_settings_path(&home)).unwrap(),
        )
        .unwrap();
        assert!(
            v["env"].get("ANTHROPIC_BASE_URL").is_none(),
            "断开后应真正移除接入"
        );
        assert!(!claude_status(&home, "http://127.0.0.1:8787").integrated);
    }

    #[test]
    fn codex_refresh_then_disconnect_fully_restores() {
        let home = temp_home("cx-refresh");
        integrate_codex(&home, "http://127.0.0.1:8787/v1", "tok", "glm-5.2").unwrap();
        integrate_codex(&home, "http://127.0.0.1:8787/v1", "tok", "glm-5.2").unwrap();
        assert!(!backup_path(&codex_config_path(&home)).exists(), "刷新不得产生快照");

        assert!(disconnect_codex(&home).unwrap());
        assert!(
            !codex_status(&home, "http://127.0.0.1:8787/v1").integrated,
            "断开后应真正移除接入"
        );
        assert!(
            !codex_auth_path(&home).exists(),
            "本代理生成的 auth.json 应被移除"
        );
    }

    #[test]
    fn claude_modelPicker_overwritten_and_restored() {
        // 用户自建的 modelPicker：接入覆盖（快照保存原状），断开从快照恢复
        let home = temp_home("cl-picker");
        let path = claude_settings_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"modelPicker": {"options": [{"model": "opus", "label": "My Opus"}]}}"#,
        )
        .unwrap();

        integrate_claude(&home, "http://127.0.0.1:8787", "tok", &["glm-5.2".to_string()], "", 0)
            .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let options = v["modelPicker"]["options"].as_array().unwrap();
        assert_eq!(options[0]["model"], "glm-5.2", "接入覆盖用户 modelPicker");

        // 断开 → 快照恢复用户的 modelPicker
        disconnect_claude(&home).unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            v2["modelPicker"]["options"][0]["label"], "My Opus",
            "断开应恢复用户原 modelPicker"
        );

        // 无快照路径（文件为本代理生成）：modelPicker 一并移除
        integrate_claude(&home, "http://127.0.0.1:8787", "tok", &["glm-5.2".to_string()], "", 0)
            .unwrap();
        std::fs::remove_file(backup_path(&path)).unwrap();
        disconnect_claude(&home).unwrap();
        let v3: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v3.get("modelPicker").is_none(), "无快照时移除本代理的 modelPicker");
    }

    #[test]
    fn status_detects_foreign_config() {
        let home = temp_home("st-foreign");
        let path = claude_settings_path(&home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"env": {"ANTHROPIC_BASE_URL": "https://api.anthropic.com"}}"#,
        )
        .unwrap();
        let st = claude_status(&home, "http://127.0.0.1:8787");
        assert!(!st.integrated);
        assert_eq!(st.base_url.as_deref(), Some("https://api.anthropic.com"));
    }
}
