# Token Proxy

本地 OpenAI 兼容模型代理 + 桌面控制台（Tauri 2 + Rust/Axum + React）。

把任意 OpenAI 兼容客户端（Cherry Studio、Cline、自研代码等）的 `base_url`
指向 `http://127.0.0.1:8787/v1`，由本应用负责：**路由到多上游、失败自动重试
（指数退避 + 熔断）、SSE 流式透传、参数改写、请求统计**。

## 功能

- OpenAI 兼容 API：`/v1/models`、`/v1/chat/completions`、`/v1/completions`、
  `/v1/embeddings`、`/v1/responses`，其余 `/v1/*` 透传
- SSE 流式（`stream: true`）逐行转发，断流返回错误事件，自动统计 token 用量
- 失败重试：可配置重试次数/退避/可重试状态码；网络错误、429、5xx 自动重试，
  4xx 业务错误不重试；指数退避 + 抖动
- 熔断器：上游连续失败达阈值后熔断冷却，半开试探自动恢复
- 多 Provider + 路由表：模型 → 上游映射，支持参数改写（temperature/max_tokens 等）
- 配置热重载：修改配置文件即时生效，无需重启
- 桌面 GUI：总览/Provider/路由/日志/设置五个页面，实时请求流、熔断状态展示

## 开发

```bash
# 前置：Rust nightly 不需要；Node 18+；Windows 需 WebView2（Win10/11 自带）

npm install
npm run tauri dev      # 开发模式（vite + cargo）
npm run tauri build    # 打包安装程序
```

## 配置

配置文件位于 `%APPDATA%\dev.tokenproxy.app\config.json`（Windows）。
也可以在应用内「重试与设置」页编辑并保存。

```jsonc
{
  "server": { "host": "127.0.0.1", "port": 8787, "local_api_key": "" },
  "retry": {
    "max_retries": 3,
    "base_delay_ms": 500,
    "max_delay_ms": 8000,
    "retry_statuses": [408, 429, 500, 502, 503, 529],
    "retry_on_network_error": true,
    "circuit_breaker": { "enabled": true, "failure_threshold": 5, "cooldown_secs": 30 }
  },
  "providers": [
    {
      "id": "openai",
      "name": "OpenAI",
      "base_url": "https://api.openai.com/v1",
      "api_key": "sk-...",
      "timeout_secs": 300,
      "enabled": true,
      "models": ["gpt-4o", "gpt-4o-mini"]
    }
  ],
  "routes": [
    { "model": "gpt-4o", "provider": "openai", "overrides": { "temperature": 0.2 } }
  ]
}
```

## 快速试用

```bash
# 添加一个本地 mock 上游验证代理：
# 1. 在 GUI「上游 Provider」添加 base_url 指向任意 OpenAI 兼容服务
# 2. curl http://127.0.0.1:8787/v1/chat/completions \
#      -H 'Content-Type: application/json' \
#      -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'
```

## 架构

```
Tauri 2 桌面壳 (React)
   │  IPC (invoke / event)
   ▼
Axum 代理内核 (Rust, 127.0.0.1:8787)
   ├─ /v1/* OpenAI 兼容端点（含 SSE 流式）
   ├─ 路由表 → Provider 适配（base_url + api_key）
   ├─ 重试循环：指数退避 + 抖动
   ├─ 熔断器（每 Provider 独立）
   └─ 指标/请求日志（推送到 GUI）
```
