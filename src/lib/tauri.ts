import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AppConfig,
  BreakerMap,
  CompatIntegrationStatus,
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

/** 预览模式下的接入状态（非 Tauri 环境无真实配置文件可读写） */
const MOCK_COMPAT_STATUS: CompatIntegrationStatus = {
  base_url: "http://127.0.0.1:8787",
  claude: {
    path: "~/.claude/settings.json",
    integrated: false,
    base_url: null,
    has_backup: false,
  },
  codex: {
    path: "~/.codex/config.toml",
    integrated: false,
    base_url: null,
    has_backup: false,
  },
};

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
  /** 导出配置为 JSON 字符串（含 API Key 等敏感信息） */
  exportConfig: () =>
    isTauri
      ? invoke<string>("export_config")
      : Promise.resolve(JSON.stringify(MOCK_CONFIG, null, 2)),
  /** 弹出「另存为」对话框，写入用户选择的位置；返回保存路径，用户取消时为 null */
  exportConfigToFile: (defaultName: string): Promise<string | null> =>
    isTauri
      ? invoke<string | null>("export_config_to_file", { defaultName })
      : // 浏览器预览模式没有原生对话框，退回浏览器下载（位置由浏览器决定）
        Promise.resolve(downloadJson(JSON.stringify(MOCK_CONFIG, null, 2), defaultName)),
  /** 导入配置 JSON 字符串；后端会校验格式与内容 */
  importConfig: (json: string) =>
    isTauri
      ? invoke<AppConfig>("import_config", { json })
      : Promise.resolve(MOCK_CONFIG),
  /** 重置用户已用次数（不传 id 表示全部重置） */
  resetUserUsage: (id?: string | null) =>
    isTauri
      ? invoke<void>("reset_user_usage", { id: id ?? null })
      : Promise.resolve(),
  /** 各用户已用次数：{ id: 次数 } */
  getUserUsage: () =>
    isTauri
      ? invoke<Record<string, number>>("get_user_usage")
      : Promise.resolve({}),
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
  /** Claude Code / Codex 一键接入状态 */
  getCompatStatus: (): Promise<CompatIntegrationStatus> =>
    isTauri
      ? invoke<CompatIntegrationStatus>("compat_integration_status")
      : Promise.resolve(MOCK_COMPAT_STATUS),
  /** 一键接入：改写 CLI 本地配置指向本代理（返回更新后的状态） */
  compatIntegrate: (target: "claude" | "codex"): Promise<CompatIntegrationStatus> =>
    isTauri
      ? invoke<CompatIntegrationStatus>("compat_integrate", { target })
      : Promise.resolve({
          ...MOCK_COMPAT_STATUS,
          [target]: { ...MOCK_COMPAT_STATUS[target], integrated: true },
        } as CompatIntegrationStatus),
  /** 断开接入：从备份恢复 CLI 原配置（返回更新后的状态） */
  compatDisconnect: (target: "claude" | "codex"): Promise<CompatIntegrationStatus> =>
    isTauri
      ? invoke<CompatIntegrationStatus>("compat_disconnect", { target })
      : Promise.resolve({
          ...MOCK_COMPAT_STATUS,
          [target]: { ...MOCK_COMPAT_STATUS[target], integrated: false },
        } as CompatIntegrationStatus),
};

/// 触发浏览器下载（仅非 Tauri 预览环境使用）。
///
/// 浏览器不告知保存位置，故返回一个说明性文本，避免界面误判为"用户取消"。
function downloadJson(json: string, fileName: string): string {
  const url = URL.createObjectURL(new Blob([json], { type: "application/json" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = fileName;
  a.click();
  URL.revokeObjectURL(url);
  return "浏览器默认下载目录（预览模式，非桌面端）";
}

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
