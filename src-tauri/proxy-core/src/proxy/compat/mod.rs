//! 客户端协议兼容层：让 Claude Code / Codex 直接把本代理当作其官方 API 使用。
//!
//! 两类客户端的协议都不是 chat completions，无法直接接入主链路：
//! - **Claude Code**（Anthropic Messages API）：`POST /v1/messages`，
//!   `x-api-key` 鉴权、content blocks 消息结构、
//!   `message_start / content_block_* / message_delta / message_stop` SSE 事件流
//! - **Codex**（OpenAI Responses API）：`POST /v1/responses`，
//!   `instructions + input items` 消息结构、
//!   `response.created / response.output_text.delta / response.completed` SSE 事件流
//!
//! 两个端点都把请求**翻译成 chat completions** 后交给
//! [`route_and_call`](crate::proxy::api::route_and_call)——路由、负载均衡、
//! 熔断、重试、failover、在途计数、统计日志全部复用主链路；响应（含流式）
//! 再翻译回各自协议。
//!
//! # 模型映射
//! Claude Code / Codex 请求的是自家模型名（`claude-sonnet-4-5-*` / `gpt-5-*`），
//! 代理上通常没有对应路由。`compat.claude_code_default_model` /
//! `compat.codex_default_model` 配置兜底模型：请求模型未命中任何路由时改写为
//! 兜底模型再走正常路由解析；已命中（用户为这些名字显式配了路由）则不改写。
//!
//! # 上游兼容性
//! 流式翻译时会给上游 chat 请求附加 `stream_options: {"include_usage": true}`
//! 以获取真实 token 用量（Claude Code 界面展示依赖 usage）。主流 OpenAI 兼容
//! 上游均支持该字段；个别严格实现若因此报 400，可将该渠道配置走非流式或升级上游。

pub mod anthropic;
pub mod responses_api;

use crate::proxy::state::AppState;
use std::sync::Arc;

/// 请求模型未命中任何路由时改写为兜底模型；兜底为空或同样未命中则保持原名
/// （由主链路返回带排查指引的 404）。
pub(crate) fn resolve_model_or_fallback(
    state: &Arc<AppState>,
    requested: &str,
    fallback: &str,
) -> String {
    let requested_hit = {
        let cfg = state.config.read();
        !cfg.resolve_candidates(requested).is_empty()
    };
    if requested_hit {
        return requested.to_string();
    }
    let fb = fallback.trim();
    if fb.is_empty() {
        return requested.to_string();
    }
    let fallback_hit = {
        let cfg = state.config.read();
        !cfg.resolve_candidates(fb).is_empty()
    };
    if fallback_hit {
        tracing::info!(
            requested = %requested,
            fallback = %fb,
            "compat: requested model has no route, rewritten to fallback model"
        );
        fb.to_string()
    } else {
        requested.to_string()
    }
}

/// 去掉 Claude Code 的 1M 上下文标记后缀（`模型名[1m]`）。
///
/// Claude Code 用 `[1m]` 后缀声明 1M 上下文窗口（内置模型目录之外模型的
/// 替代方案），后缀可能随模型名一起发到 API。路由解析必须按去掉后缀的
/// 名字进行，否则 `glm-5.2[1m]` 会因未命中路由而被兜底改写到错误的模型。
pub(crate) fn strip_context_suffix(model: &str) -> &str {
    model.strip_suffix("[1m]").unwrap_or(model)
}

/// 粗略 token 估算：JSON 字节数 / 4（与主链路 `estimate_input_tokens` 同口径）。
pub(crate) fn estimate_tokens(v: &serde_json::Value) -> u64 {
    v.to_string().len().checked_div(4).map(|v| v as u64).unwrap_or(0)
}

/// 从 SSE 字节缓冲中取出下一行（不含换行符）；`None` 表示缓冲不足，需继续读取。
pub(crate) fn next_sse_line(buf: &mut Vec<u8>) -> Option<String> {
    let pos = buf.iter().position(|&b| b == b'\n')?;
    let line = String::from_utf8_lossy(&buf[..pos]).trim().to_string();
    buf.drain(..=pos);
    Some(line)
}

/// 提取上游 chat chunk 中的 usage（prompt_tokens, completion_tokens, cached_tokens）。
pub(crate) fn chunk_usage(chunk: &serde_json::Value) -> Option<(u64, u64, u64)> {
    let u = chunk.get("usage")?;
    let prompt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let completion = u
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // 空 usage 对象（部分上游每块都带空 usage）视为无值
    if prompt == 0 && completion == 0 {
        return None;
    }
    Some((prompt, completion, cached))
}
