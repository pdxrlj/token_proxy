//! 上游可用模型列表拉取（GUI「获取模型列表」用）。
//!
//! 调上游 `GET /models` 拉取模型清单。OpenAI 标准格式只返回 `id`；
//! 部分上游（SenseNova / OpenRouter 等）会附带模态信息，可据此给模型
//! 打上类别标签：
//! - 输出含 `image` → 图片生成模型
//! - 输入含 `image` / `audio` / `video`（输出为文本）→ 多模态模型
//! - 输入输出均为 `text` → 文本模型
//! - 无模态字段 → 无法判断（标准 OpenAI 响应即如此），类别留空

use crate::proxy::config::ProviderConfig;
use crate::proxy::headers::build_upstream_headers;
use axum::http::HeaderMap;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// 拉取模型列表的超时（秒）。与探活同级：列表请求很轻，15s 足够覆盖冷启动。
const FETCH_TIMEOUT_SECS: u64 = 15;

/// 上游返回的单个模型（GUI 展示用）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UpstreamModel {
    pub id: String,
    /// 模型类别：`text` 文本 / `multimodal` 多模态 / `image` 图片生成；
    /// 上游未提供模态信息时为空字符串（无法判断）。
    pub category: String,
    /// 上下文窗口（tokens）；上游未提供时为 None。
    pub context_length: Option<u64>,
}

/// 拉取上游可用模型列表。
///
/// `headers` 为 GUI 里填写的自定义请求头（与 ProviderConfig.headers 同一语义）。
/// 返回按上游原始顺序去重后的模型列表。
pub async fn fetch_upstream_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    user_agent: &str,
    headers: &BTreeMap<String, String>,
) -> Result<Vec<UpstreamModel>, String> {
    // 借用临时 ProviderConfig 复用既有的鉴权 / UA / 自定义头拼装逻辑
    let provider = ProviderConfig {
        id: String::new(),
        name: String::new(),
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        timeout_secs: FETCH_TIMEOUT_SECS,
        enabled: true,
        models: vec![],
        model_categories: BTreeMap::new(),
        user_agent: user_agent.to_string(),
        headers: headers.clone(),
    };
    let url = provider.endpoint("models");

    let mut req = http
        .get(&url)
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS));
    for (name, value) in build_upstream_headers(&HeaderMap::new(), &provider) {
        req = req.header(name, value);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求失败：{e}（请检查 Base URL 是否可达）"))?;

    let status = resp.status();
    if !status.is_success() {
        // 401/403 给出更直接的提示；其余带上一段响应体便于定位
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        return Err(match status.as_u16() {
            401 | 403 => format!("上游返回 HTTP {}：API Key 无效或未授权", status.as_u16()),
            _ => format!("上游返回 HTTP {}：{}", status.as_u16(), snippet),
        });
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("响应不是合法 JSON：{e}"))?;

    parse_models_response(&json)
}

/// 解析 `/models` 响应为模型列表。
///
/// 兼容三种形态：
/// - 标准 OpenAI：`{"data": [{"id": "gpt-4o"}]}`（无模态信息，类别留空）
/// - SenseNova：`{"data": [{"id", "input_modalities", "output_modalities", ...}]}`
/// - OpenRouter：`{"data": [{"id", "architecture": {"input_modalities", ...}}]}`
pub(crate) fn parse_models_response(json: &serde_json::Value) -> Result<Vec<UpstreamModel>, String> {
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "响应缺少 data 数组（不是 OpenAI 兼容的 /models 响应）".to_string())?;

    let mut out: Vec<UpstreamModel> = Vec::with_capacity(data.len());
    for item in data {
        // 个别上游直接返回字符串数组，容错为纯 id
        let (id, input, output, ctx) = if let Some(s) = item.as_str() {
            (s.to_string(), None, None, None)
        } else {
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            (
                id.to_string(),
                modalities(item, "input_modalities"),
                modalities(item, "output_modalities"),
                item.get("context_length").and_then(|v| v.as_u64()),
            )
        };
        if out.iter().any(|m| m.id == id) {
            continue;
        }
        out.push(UpstreamModel {
            id,
            category: classify_modalities(input, output).to_string(),
            context_length: ctx,
        });
    }
    Ok(out)
}

/// 从模型条目取模态列表：顶层字段优先，其次 OpenRouter 的 `architecture` 嵌套。
fn modalities<'a>(item: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    item.get(key)
        .filter(|v| v.is_array())
        .or_else(|| item.pointer(&format!("/architecture/{key}")).filter(|v| v.is_array()))
}

/// 按输入 / 输出模态判断模型类别。
///
/// 输入 / 输出为 `None` 表示上游未提供该字段。
fn classify_modalities(
    input: Option<&serde_json::Value>,
    output: Option<&serde_json::Value>,
) -> &'static str {
    fn has(v: Option<&serde_json::Value>, m: &str) -> bool {
        v.and_then(|v| v.as_array())
            .map(|a| a.iter().any(|x| x.as_str() == Some(m)))
            .unwrap_or(false)
    }
    // 输出图片 → 图片生成模型
    if has(output, "image") {
        return "image";
    }
    // 输入含图片 / 音频 / 视频（输出为文本）→ 多模态理解模型
    if has(input, "image") || has(input, "audio") || has(input, "video") {
        return "multimodal";
    }
    // 输入输出都是纯文本 → 文本模型
    if has(input, "text") && has(output, "text") {
        return "text";
    }
    // 无模态信息（标准 OpenAI 响应）→ 无法判断
    ""
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_sensenova_like_response() {
        // 模拟 SenseNova 风格：文本 / 图片生成 / 多模态理解三种
        let json = json!({
            "data": [
                {
                    "id": "glm-5.2",
                    "input_modalities": ["text"],
                    "output_modalities": ["text"],
                    "context_length": 1048576
                },
                {
                    "id": "sensenova-u1-fast",
                    "input_modalities": ["text"],
                    "output_modalities": ["image"],
                    "context_length": 262144
                },
                {
                    "id": "sensenova-6.8-flash-lite",
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"]
                },
                {
                    // 重复 id：去重保留首条
                    "id": "glm-5.2"
                }
            ]
        });
        let models = parse_models_response(&json).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].category, "text");
        assert_eq!(models[0].context_length, Some(1048576));
        assert_eq!(models[1].category, "image");
        assert_eq!(models[2].category, "multimodal");
    }

    #[test]
    fn classify_plain_openai_response() {
        // 标准 OpenAI 响应没有模态信息：类别留空但 id 正常返回
        let json = json!({ "data": [{ "id": "gpt-4o" }, { "id": "gpt-4o-mini" }] });
        let models = parse_models_response(&json).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-4o");
        assert_eq!(models[0].category, "");
        assert_eq!(models[0].context_length, None);
    }

    #[test]
    fn classify_openrouter_nested_architecture() {
        let json = json!({
            "data": [
                {
                    "id": "qwen/qwen3-vl",
                    "architecture": {
                        "input_modalities": ["text", "image"],
                        "output_modalities": ["text"]
                    }
                }
            ]
        });
        let models = parse_models_response(&json).unwrap();
        assert_eq!(models[0].category, "multimodal");
    }

    #[test]
    fn classify_audio_and_missing_fields() {
        // 音频输入也算多模态；仅输出 text 缺输入时视为无法判断
        assert_eq!(classify_modalities(Some(&json!(["audio"])), Some(&json!(["text"]))), "multimodal");
        assert_eq!(classify_modalities(None, Some(&json!(["text"]))), "");
        assert_eq!(classify_modalities(Some(&json!([])), Some(&json!([]))), "");
    }

    #[test]
    fn string_array_items_tolerated() {
        let json = json!({ "data": ["m-a", "m-b"] });
        let models = parse_models_response(&json).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "m-a");
        assert_eq!(models[0].category, "");
    }

    #[test]
    fn missing_data_array_is_error() {
        assert!(parse_models_response(&json!({ "models": [] })).is_err());
    }
}
