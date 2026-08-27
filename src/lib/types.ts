/**
 * 与 Rust 侧 serde 结构对应的类型定义。
 *
 * 注意：所有字段名使用 snake_case，与 Rust 序列化保持一致。
 */

// ─────────────────────────── 服务端配置 ───────────────────────────

export interface ServerConfig {
  host: string;
  port: number;
  local_api_key: string;
}

// ─────────────────────────── 重试与熔断配置 ───────────────────────────

export interface CircuitBreakerConfig {
  enabled: boolean;
  failure_threshold: number;
  cooldown_secs: number;
}

export interface RetryConfig {
  max_retries: number;
  base_delay_ms: number;
  max_delay_ms: number;
  retry_statuses: number[];
  retry_on_network_error: boolean;
  circuit_breaker: CircuitBreakerConfig;
}

// ─────────────────────────── Provider 配置 ───────────────────────────

export interface ProviderConfig {
  id: string;
  name: string;
  base_url: string;
  api_key: string;
  timeout_secs: number;
  enabled: boolean;
  models: string[];
}

// ─────────────────────────── 路由配置 ───────────────────────────

/**
 * 模型 → 上游的路由规则。
 * 同一 model 可配置多条路由，由负载均衡器在它们之间分配请求。
 */
export interface RouteConfig {
  model: string;
  provider: string;
  /** 转发到上游时使用的模型名；为空则不改写，沿用客户端请求的 model */
  upstream_model?: string;
  overrides: Record<string, unknown>;
  /** 负载均衡权重（strategy = weighted 时生效，默认 1） */
  weight?: number;
  /** 是否禁用此路由参与负载均衡（设为 true 则不参与） */
  disabled?: boolean;
}

// ─────────────────────────── 负载均衡配置 ───────────────────────────

/** 负载均衡策略 */
export type LbStrategy =
  | "round_robin"
  | "fastest_response"
  | "weighted"
  | "random"
  | "least_connections";

/** 会话标识来源（用于粘性路由，确保重试命中同一上游） */
export type SessionKeySource =
  | { type: "header"; value: string }
  | { type: "body_field"; value: string }
  | { type: "first_user_message" }
  | { type: "messages_hash" };

/**
 * 会话粘性配置。
 * 启用后，同一会话的请求始终路由到首次选中的上游。
 */
export interface StickyConfig {
  enabled: boolean;
  session_key_source: SessionKeySource;
  /** 粘性 TTL（秒）；超时后重新选择上游 */
  ttl_secs: number;
}

/** 负载均衡配置 */
export interface LoadBalanceConfig {
  enabled: boolean;
  strategy: LbStrategy;
  sticky: StickyConfig;
}

// ─────────────────────────── 顶层配置 ───────────────────────────

/** 顶层应用配置，存为 JSON，GUI 编辑后写回并热重载 */
export interface AppConfig {
  server: ServerConfig;
  retry: RetryConfig;
  providers: ProviderConfig[];
  routes: RouteConfig[];
  load_balance: LoadBalanceConfig;
}

export interface RequestLog {
  id: string;
  ts: number;
  method: string;
  path: string;
  model: string;
  provider: string | null;
  status: number;
  retries: number;
  latency_ms: number;
  stream: boolean;
  error: string | null;
  tokens_in: number | null;
  tokens_out: number | null;
  response_bytes?: number | null;
  ttfb_ms?: number | null;
  cache_hit?: boolean;
  /** 缓存命中的 token 数量（上游返回的 cached_tokens / cache_read_input_tokens 等） */
  cached_tokens?: number | null;
  /** 实际转发给上游的模型名（经路由 upstream_model 改写后） */
  upstream_model?: string | null;
}

export interface MetricsSnapshot {
  total: number;
  success: number;
  failed: number;
  retries: number;
  tokens_in: number;
  tokens_out: number;
  avg_latency_ms: number;
  cache_hits?: number;
  bytes_in?: number;
  bytes_out?: number;
  recent: RequestLog[];
}

export interface StatusDto {
  running: boolean;
  host: string;
  port: number;
  config_path: string;
  uptime_secs: number;
  providers: number;
  routes: number;
  total_requests: number;
}

export type BreakerStateName = "closed" | "open" | "half_open";

export interface BreakerStatus {
  state: BreakerStateName;
  consecutive_failures: number;
}

export type BreakerMap = Record<string, BreakerStatus>;

export const DEFAULT_CONFIG: AppConfig = {
  server: { host: "127.0.0.1", port: 8787, local_api_key: "" },
  retry: {
    max_retries: 3,
    base_delay_ms: 500,
    max_delay_ms: 8000,
    retry_statuses: [408, 429, 500, 502, 503, 529],
    retry_on_network_error: true,
    circuit_breaker: {
      enabled: true,
      failure_threshold: 5,
      cooldown_secs: 30,
    },
  },
  providers: [],
  routes: [],
  load_balance: {
    enabled: false,
    strategy: "round_robin",
    sticky: {
      enabled: true,
      session_key_source: { type: "first_user_message" },
      ttl_secs: 1800,
    },
  },
};

/** 数据存储记录（对应 Rust DatasetRecord） */
export interface DatasetRecord {
  id: number;
  ts: number;
  model: string;
  upstream_model: string | null;
  provider: string | null;
  stream: boolean;
  tokens_in: number | null;
  tokens_out: number | null;
  cache_hit: boolean;
  request_body: string;
  response_body: string;
  finetune_jsonl: string;
}
