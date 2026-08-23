import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AppConfig,
  BreakerMap,
  DatasetRecord,
  MetricsSnapshot,
  RequestLog,
  StatusDto,
} from "./types";
import {
  MOCK_CONFIG,
  isTauri,
  mockBreakers,
  mockDataset,
  mockDatasetCount,
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
  getDataset: (limit = 50, offset = 0, modelFilter?: string) =>
    isTauri
      ? invoke<DatasetRecord[]>("get_dataset", { limit, offset, modelFilter })
      : Promise.resolve(mockDataset(limit, offset)),
  getDatasetCount: (modelFilter?: string) =>
    isTauri
      ? invoke<number>("get_dataset_count", { modelFilter })
      : Promise.resolve(mockDatasetCount()),
  exportDatasetJsonl: (modelFilter?: string) =>
    isTauri
      ? invoke<string>("export_dataset_jsonl", { modelFilter })
      : Promise.resolve(""),
  clearDataset: () =>
    isTauri ? invoke<void>("clear_dataset") : Promise.resolve(),
  deleteDatasetRecord: (id: number) =>
    isTauri
      ? invoke<void>("delete_dataset_record", { id })
      : Promise.resolve(),
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
