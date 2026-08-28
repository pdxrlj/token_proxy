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
  /** 强制要求请求携带已配置的 API Key（开启后配额才无法绕过） */
  require_api_key?: boolean;
}

/** 用户（每人持有一个 API Key，可选携带；用于配额限制） */
export interface UserConfig {
  id: string;
  name: string;
  key: string;
  enabled: boolean;
  /** 最大请求数；null / undefined 表示不限制 */
  max_requests?: number | null;
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
  users?: UserConfig[];
}

/** 错误大类：决定详情面板的标题与配色 */
export type ErrorKind =
  | "upstream"
  | "circuit_open"
  | "network"
  | "timeout"
  | "route_not_found"
  | "bad_request"
  | "unauthorized"
  | "internal";

/** 结构化错误详情：后端在出错时收集的诊断信息 */
export interface ErrorDetail {
  kind: ErrorKind;
  /** 一句话摘要：发生了什么 */
  summary: string;
  /** 上游/系统返回的原始错误详情 */
  detail?: string | null;
  /** 面向用户的排查建议 */
  hint?: string | null;
  provider?: string | null;
  provider_id?: string | null;
  upstream_model?: string | null;
  endpoint?: string | null;
  /** 上游返回的 HTTP 状态码 */
  upstream_status?: number | null;
  /** 已尝试的候选数 */
  candidates_tried?: number | null;
  total_candidates?: number | null;
  /** 重试次数 */
  retries?: number | null;
  /** 连续失败次数 */
  consecutive_failures?: number | null;
  /** 熔断冷却剩余秒数；0 表示可立即重试 */
  cooldown_remaining_secs?: number | null;
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
  /** 结构化错误详情（成功请求为 undefined） */
  error_detail?: ErrorDetail | null;
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
  /** 服务启动失败原因（如端口被占用）；正常运行时为 null */
  server_error?: string | null;
}

export type BreakerStateName = "closed" | "open" | "half_open";

export interface BreakerStatus {
  state: BreakerStateName;
  consecutive_failures: number;
}

export type BreakerMap = Record<string, BreakerStatus>;

export const DEFAULT_CONFIG: AppConfig = {
  server: {
    host: "127.0.0.1",
    port: 8787,
    local_api_key: "",
    require_api_key: false,
  },
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
  users: [],
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
