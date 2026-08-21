import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AppConfig,
  BreakerMap,
  MetricsSnapshot,
  RequestLog,
  StatusDto,
} from "./types";

/** Rust 命令封装（Tauri v2：JS camelCase 自动映射到 snake_case 参数） */
export const api = {
  getConfig: () => invoke<AppConfig>("get_config"),
  saveConfig: (cfg: AppConfig) => invoke<void>("save_config", { cfg }),
  reloadConfig: () => invoke<AppConfig>("reload_config"),
  getStatus: () => invoke<StatusDto>("get_status"),
  getMetrics: () => invoke<MetricsSnapshot>("get_metrics"),
  resetBreaker: (providerId: string) =>
    invoke<void>("reset_breaker", { providerId }),
  getBreakerStatus: () => invoke<BreakerMap>("get_breaker_status"),
};

/** 订阅代理请求日志推送 */
export function onProxyLog(cb: (log: RequestLog) => void) {
  return listen<RequestLog>("proxy-log", (e) => cb(e.payload));
}

/** 订阅配置热重载 */
export function onConfigChanged(cb: () => void) {
  return listen("config-changed", cb);
}
