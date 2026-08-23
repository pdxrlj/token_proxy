// 浏览器预览模式：非 Tauri 环境（vite preview / 浏览器打开）时提供假数据，
// 便于纯前端开发与界面预览。真实桌面运行时走 Tauri IPC。
import type {
  AppConfig,
  BreakerMap,
  DatasetRecord,
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
  const cacheHit = Math.random() > 0.75;
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
    cache_hit: cacheHit,
    cached_tokens: cacheHit
      ? 100 + Math.floor(Math.random() * 2000)
      : null,
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
      upstream_model: "",
      overrides: { temperature: 0.7 },
    },
    {
      model: "deepseek-chat",
      provider: "deepseek",
      upstream_model: "",
      overrides: {},
    },
    {
      model: "claude-sonnet-4",
      provider: "anthropic",
      upstream_model: "",
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

const MOCK_PROMPTS = [
  "请解释 Rust 中所有权机制的核心概念",
  "用 Python 实现一个快速排序算法",
  "比较 React 和 Vue 的响应式系统差异",
  "写一段 SQL 查询：找出订单金额最高的前 10 个用户",
  "解释什么是 CAP 定理及其在分布式系统中的意义",
];

const MOCK_RESPONSES = [
  "Rust 的所有权机制基于三条核心规则：每个值有且仅有一个所有者，当所有者离开作用域时值被自动释放，赋值或传参时所有权转移而非复制……",
  "快速排序的核心思想是分治：选择一个基准元素，将数组分为小于和大于基准的两部分，递归排序后合并……",
  "React 使用不可变状态 + 虚拟 DOM diff 触发重渲染，Vue 基于 Proxy 实现响应式依赖追踪，数据变更自动触发更新……",
  "SELECT u.user_id, u.name, SUM(o.amount) AS total FROM orders o JOIN users u ON o.user_id = u.user_id GROUP BY u.user_id ORDER BY total DESC LIMIT 10;",
  "CAP 定理指出分布式系统最多同时满足一致性（C）、可用性（A）、分区容错（P）中的两个。在网络分区不可避免时，需在 CP 和 AP 间取舍……",
];

export function mockDataset(limit: number, _offset: number): DatasetRecord[] {
  const models = ["gpt-4o", "deepseek-chat", "claude-sonnet-4", "qwen-max"];
  const providers = ["OpenAI", "DeepSeek", "Anthropic", "Qwen"];
  return Array.from({ length: Math.min(limit, 5) }, (_, i) => {
    const idx = i % MOCK_PROMPTS.length;
    const mi = i % models.length;
    const stream = i % 2 === 0;
    const reqBody = JSON.stringify({
      model: models[mi],
      messages: [
        { role: "system", content: "你是一个专业的 AI 助手。" },
        { role: "user", content: MOCK_PROMPTS[idx] },
      ],
    });
    const respBody = JSON.stringify({
      choices: [{ message: { role: "assistant", content: MOCK_RESPONSES[idx] } }],
      usage: { prompt_tokens: 120 + i * 10, completion_tokens: 80 + i * 5 },
    });
    const finetune = JSON.stringify({
      messages: [
        { role: "system", content: "你是一个专业的 AI 助手。" },
        { role: "user", content: MOCK_PROMPTS[idx] },
        { role: "assistant", content: MOCK_RESPONSES[idx] },
      ],
    });
    return {
      id: 1000 - i,
      ts: Date.now() - i * 3600000,
      model: models[mi],
      upstream_model: i % 3 === 0 ? models[mi] + "-upstream" : null,
      provider: providers[mi],
      stream,
      tokens_in: 120 + i * 10,
      tokens_out: 80 + i * 5,
      cache_hit: i % 4 === 0,
      request_body: reqBody,
      response_body: respBody,
      finetune_jsonl: finetune,
    };
  });
}

export function mockDatasetCount(): number {
  return 42;
}
