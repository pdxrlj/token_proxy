// 浏览器预览模式：非 Tauri 环境（vite preview / 浏览器打开）时提供假数据，
// 便于纯前端开发与界面预览。真实桌面运行时走 Tauri IPC。
import type {
  AppConfig,
  BreakerMap,
  MetricsSnapshot,
  RequestLog,
  StatusDto,
} from "./types";

export const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

let seq = 0;
export function mockLog(): RequestLog {
  const models = ["gpt-4o", "deepseek-chat", "claude-sonnet-4", "qwen-max"];
  const statuses = [200, 200, 200, 429, 500];
  const s = statuses[Math.floor(Math.random() * statuses.length)];
  const stream = Math.random() > 0.4;
  const respBytes = stream
    ? 2000 + Math.floor(Math.random() * 18000)
    : 800 + Math.floor(Math.random() * 4000);
  return {
    id: `mock-${++seq}`,
    ts: Date.now() - Math.floor(Math.random() * 120000),
    method: "POST",
    path: "/v1/chat/completions",
    model: models[Math.floor(Math.random() * models.length)],
    provider: ["OpenAI", "DeepSeek", "Anthropic", "Qwen"][
      Math.floor(Math.random() * 4)
    ],
    status: s,
    retries: s >= 500 ? Math.floor(Math.random() * 3) : 0,
    latency_ms: 300 + Math.floor(Math.random() * 4000),
    stream,
    error: s >= 400 ? "上游返回错误（示例数据）" : null,
    tokens_in: 100 + Math.floor(Math.random() * 900),
    tokens_out: 50 + Math.floor(Math.random() * 800),
    response_bytes: respBytes,
    ttfb_ms: stream
      ? 80 + Math.floor(Math.random() * 600)
      : 300 + Math.floor(Math.random() * 3500),
    cache_hit: Math.random() > 0.75,
  };
}

export const MOCK_CONFIG: AppConfig = {
  server: { host: "127.0.0.1", port: 8787, local_api_key: "" },
  retry: {
    max_retries: 3,
    base_delay_ms: 500,
    max_delay_ms: 8000,
    retry_statuses: [408, 429, 500, 502, 503, 529],
    retry_on_network_error: true,
    circuit_breaker: { enabled: true, failure_threshold: 5, cooldown_secs: 30 },
  },
  providers: [
    {
      id: "openai",
      name: "OpenAI",
      base_url: "https://api.openai.com/v1",
      api_key: "sk-demo-••••••••",
      timeout_secs: 300,
      enabled: true,
      models: ["gpt-4o", "gpt-4o-mini", "gpt-4.1"],
    },
    {
      id: "deepseek",
      name: "DeepSeek",
      base_url: "https://api.deepseek.com/v1",
      api_key: "sk-demo-••••••••",
      timeout_secs: 300,
      enabled: true,
      models: ["deepseek-chat", "deepseek-reasoner"],
    },
    {
      id: "anthropic",
      name: "Anthropic",
      base_url: "https://api.anthropic.com/v1",
      api_key: "sk-demo-••••••••",
      timeout_secs: 300,
      enabled: false,
      models: ["claude-sonnet-4", "claude-opus-4"],
    },
    {
      id: "ollama",
      name: "Ollama 本地",
      base_url: "http://127.0.0.1:11434/v1",
      api_key: "",
      timeout_secs: 300,
      enabled: true,
      models: ["qwen2.5:7b", "llama3.1:8b"],
    },
  ],
  routes: [
    {
      model: "gpt-4o",
      provider: "openai",
      overrides: { temperature: 0.7 },
    },
    {
      model: "deepseek-chat",
      provider: "deepseek",
      overrides: {},
    },
    {
      model: "claude-sonnet-4",
      provider: "anthropic",
      overrides: {},
    },
  ],
};

export function mockStatus(): StatusDto {
  return {
    running: true,
    host: "127.0.0.1",
    port: 8787,
    config_path: "C:\\Users\\demo\\AppData\\Roaming\\dev.tokenproxy.app\\config.json",
    uptime_secs: 86400 + seq * 3,
    providers: 4,
    routes: 3,
    total_requests: 1248 + seq,
  };
}

export function mockMetrics(): MetricsSnapshot {
  const recent = Array.from({ length: 8 }, () => mockLog());
  return {
    total: 1248 + seq,
    success: 1190 + seq,
    failed: 58,
    retries: 23,
    tokens_in: 483_200,
    tokens_out: 196_700,
    avg_latency_ms: 1240,
    cache_hits: 312,
    bytes_in: 1_920_000,
    bytes_out: 8_640_000,
    recent,
  };
}

export function mockBreakers(): BreakerMap {
  return {
    openai: { state: "closed", consecutive_failures: 0 },
    deepseek: { state: "closed", consecutive_failures: 1 },
    anthropic: { state: "closed", consecutive_failures: 0 },
    ollama: { state: "half_open", consecutive_failures: 3 },
  };
}
