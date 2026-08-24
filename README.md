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
- 熔断器：上游连续失败达阈值后熔断冷却，半开试探自动恢复（每 Provider 独立）
- 多 Provider + 路由表：模型 → 上游映射，支持参数改写（temperature/max_tokens 等）
- 配置热重载：修改配置文件即时生效，无需重启
- 桌面 GUI：总览 / 上游 Provider / 路由 / 请求日志 / 重试与设置五个页面，
  实时请求流、路由链路图、熔断状态展示
- 本地鉴权：可选的 `local_api_key`，默认仅本机可访问

## 开发

```bash
# 前置：Rust 1.77+、Node 18+、Windows 需 WebView2（Win10/11 自带）

npm install
npm run tauri dev      # 开发模式（vite + cargo 热重载）
npm run tauri build    # 打包安装程序
```

**纯前端预览**（不需要 Tauri 环境，mock 数据驱动）：

```bash
npm run build && npm run preview
# 打开 http://localhost:4173 —— 非 Tauri 环境自动注入示例数据
```

**测试**：

```bash
# 代理核心：单元测试 + 端到端冒烟（转发/流式/重试/熔断，内置 mock 上游）
cd src-tauri/proxy-core && cargo test
```

## 架构

```
src-tauri/
├── proxy-core/            # 独立 crate：代理内核（不依赖 Tauri，可单独测试）
│   └── src/proxy/
│       ├── api.rs         # axum 路由 + OpenAI 兼容端点（含 SSE 流式）
│       ├── upstream.rs    # 上游转发：非流式 JSON / SSE 逐行透传 / token 统计
│       ├── retry.rs       # 重试语义判断 + 指数退避抖动
│       ├── circuit.rs     # 熔断器（每 Provider 独立，Arc 共享实例）
│       ├── config.rs      # Provider / 路由 / 重试策略配置模型
│       ├── store.rs       # 配置读写（原子写）与校验
│       └── state.rs       # 共享状态 + 指标 + 事件推送抽象(EventSink)
└── src/                   # Tauri 壳：IPC 命令 + 事件桥 + 配置热重载
    └── lib.rs

Tauri 2 桌面壳 (React)
   │  IPC (invoke / event)
   ▼
proxy-core (Rust, 127.0.0.1:8787)
   ├─ /v1/* OpenAI 兼容端点（SSE 流式透传）
   ├─ 路由表 → Provider 适配（base_url + api_key + 参数改写）
   ├─ 重试循环：指数退避 + 抖动（429/5xx/网络错误）
   ├─ 熔断器（连续失败阈值 → 冷却 → 半开试探）
   └─ 指标 / 请求日志（EventSink 推送到 GUI）
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
    "circuit_breaker": {
      "enabled": true,
      "failure_threshold": 5,
      "cooldown_secs": 30,
    },
  },
  "providers": [
    {
      "id": "openai",
      "name": "OpenAI",
      "base_url": "https://api.openai.com/v1",
      "api_key": "sk-...",
      "timeout_secs": 300,
      "enabled": true,
      "models": ["gpt-4o", "gpt-4o-mini"],
    },
  ],
  "routes": [
    {
      "model": "gpt-4o",
      "provider": "openai",
      "overrides": { "temperature": 0.2 },
    },
  ],
}
```

## 快速试用

```bash
# 1. 应用内「上游 Provider」添加任意 OpenAI 兼容服务并启用
# 2. 用任意客户端调用：
curl http://127.0.0.1:8787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'
```

## 常见问题

- **Node spawn EPERM（沙箱/受限环境）**：vite/esbuild 需要 spawn 子进程；
  若被环境拦截，前端构建可用 `node --import ./scripts/esbuild-wasm-loader.mjs
node_modules/vite/bin/vite.js build` 降级为 WASM 模式（无需 spawn）。
