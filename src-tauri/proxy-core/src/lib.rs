//! Token Proxy 核心：OpenAI 兼容代理（路由 / 重试 / 熔断 / SSE 流式）。
//! 不依赖 Tauri，可独立运行与测试。

pub mod proxy;
