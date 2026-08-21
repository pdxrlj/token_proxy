import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AppConfig,
  BreakerMap,
  MetricsSnapshot,
  RequestLog,
  StatusDto,
} from "./types";
import {
  MOCK_CONFIG,
  isTauri,
  mockBreakers,
  mockLog,
  mockMetrics,
  mockStatus,
} from "./mock";

/** Rust 命令封装（Tauri v2：JS camelCase 自动映射到 snake_case 参数）。
 *  非 Tauri 环境（浏览器预览）返回 mock 数据。 */
export const api = {
  getConfig: () =>
    isTauri
      ? invoke<AppConfig>("get_config")
      : Promise.resolve(MOCK_CONFIG),
  saveConfig: (cfg: AppConfig) =>
    isTauri ? invoke<void>("save_config", { cfg }) : Promise.resolve(),
  reloadConfig: () =>
    isTauri ? invoke<AppConfig>("reload_config") : Promise.resolve(MOCK_CONFIG),
  getStatus: () =>
    isTauri ? invoke<StatusDto>("get_status") : Promise.resolve(mockStatus()),
  getMetrics: () =>
    isTauri
      ? invoke<MetricsSnapshot>("get_metrics")
      : Promise.resolve(mockMetrics()),
  resetBreaker: (providerId: string) =>
    isTauri ? invoke<void>("reset_breaker", { providerId }) : Promise.resolve(),
  getBreakerStatus: () =>
    isTauri
      ? invoke<BreakerMap>("get_breaker_status")
      : Promise.resolve(mockBreakers()),
};

/** 订阅代理请求日志推送 */
export function onProxyLog(cb: (log: RequestLog) => void) {
  if (!isTauri) {
    const t = setInterval(() => cb(mockLog()), 2500);
    return Promise.resolve(() => clearInterval(t));
  }
  return listen<RequestLog>("proxy-log", (e) => cb(e.payload));
}

/** 订阅配置热重载 */
export function onConfigChanged(cb: () => void) {
  if (!isTauri) {
    return Promise.resolve(() => {});
  }
  return listen("config-changed", cb);
}
