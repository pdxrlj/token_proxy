// 与 Rust 侧 serde 结构对应的类型定义

export interface ServerConfig {
  host: string;
  port: number;
  local_api_key: string;
}

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

export interface ProviderConfig {
  id: string;
  name: string;
  base_url: string;
  api_key: string;
  timeout_secs: number;
  enabled: boolean;
  models: string[];
}

export interface RouteConfig {
  model: string;
  provider: string;
  overrides: Record<string, unknown>;
}

export interface AppConfig {
  server: ServerConfig;
  retry: RetryConfig;
  providers: ProviderConfig[];
  routes: RouteConfig[];
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
}

export interface MetricsSnapshot {
  total: number;
  success: number;
  failed: number;
  retries: number;
  tokens_in: number;
  tokens_out: number;
  avg_latency_ms: number;
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
};
