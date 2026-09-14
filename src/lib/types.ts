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
  /** 熔断持续时间（秒）；旧字段 cooldown_secs 通过 serde alias 兼容读入 */
  open_duration_secs: number;
  /** 主动探活任务的轮询间隔（秒） */
  probe_interval_secs: number;
  /** 半开状态恢复（转 Closed）所需的连续成功次数 */
  recovery_success_threshold: number;
  /** Panic 试探窗口（秒）：全部候选熔断时按此窗口限速放行试探请求 */
  panic_probe_window_secs: number;
}

export interface RetryConfig {
  max_retries: number;
  retry_statuses: number[];
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
  /** 转发到该上游时使用的 User-Agent；留空表示跟随客户端 */
  user_agent?: string;
  /** 附加到上游请求的自定义头，优先级高于客户端透传头 */
  headers?: Record<string, string>;
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
  /** 本机局域网 IPv4 地址（不含回环），供总览页展示局域网访问入口 */
  lan_ips?: string[];
}

export type BreakerStateName = "closed" | "open" | "half_open";

export interface BreakerStatus {
  state: BreakerStateName;
  consecutive_failures: number;
  /** 熔断冷却剩余秒数（仅 open 状态有值，其余为 null） */
  cooldown_remaining_secs?: number | null;
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
    retry_statuses: [408, 429, 500, 502, 503, 529],
    circuit_breaker: {
      enabled: true,
      failure_threshold: 5,
      open_duration_secs: 3600,
      probe_interval_secs: 30,
      recovery_success_threshold: 2,
      panic_probe_window_secs: 60,
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
