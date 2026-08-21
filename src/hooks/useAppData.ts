import { useCallback, useEffect, useRef, useState } from "react";
import { api, onConfigChanged, onProxyLog } from "../lib/tauri";
import type {
  AppConfig,
  BreakerMap,
  MetricsSnapshot,
  RequestLog,
  StatusDto,
} from "../lib/types";
import { DEFAULT_CONFIG } from "../lib/types";

export interface AppData {
  config: AppConfig;
  status: StatusDto | null;
  metrics: MetricsSnapshot;
  breakers: BreakerMap;
  /** 最近一次刷新错误 */
  error: string | null;
  /** 有流式请求在途（驱动顶部脉冲条） */
  streaming: boolean;
  refresh: () => Promise<void>;
  save: (cfg: AppConfig) => Promise<void>;
  resetBreaker: (providerId: string) => Promise<void>;
}

export function useAppData(): AppData {
  const [config, setConfig] = useState<AppConfig>(DEFAULT_CONFIG);
  const [status, setStatus] = useState<StatusDto | null>(null);
  const [metrics, setMetrics] = useState<MetricsSnapshot>({
    total: 0,
    success: 0,
    failed: 0,
    retries: 0,
    tokens_in: 0,
    tokens_out: 0,
    avg_latency_ms: 0,
    recent: [],
  });
  const [breakers, setBreakers] = useState<BreakerMap>({});
  const [error, setError] = useState<string | null>(null);
  const [streaming, setStreaming] = useState(false);
  const streamCount = useRef(0);

  const refresh = useCallback(async () => {
    try {
      const [cfg, st, ms, bs] = await Promise.all([
        api.getConfig(),
        api.getStatus(),
        api.getMetrics(),
        api.getBreakerStatus(),
      ]);
      setConfig(cfg);
      setStatus(st);
      setMetrics(ms);
      setBreakers(bs);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  // 初始加载 + 轮询
  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 3000);
    return () => clearInterval(t);
  }, [refresh]);

  // 事件订阅：请求日志 → 更新统计 + 流式指示
  useEffect(() => {
    const unsub = onProxyLog((log: RequestLog) => {
      setMetrics((m) => {
        const total = m.total + 1;
        const success = m.success + (log.status >= 200 && log.status < 400 ? 1 : 0);
        const failed = m.failed + (log.status >= 400 || log.error ? 1 : 0);
        return {
          ...m,
          total,
          success,
          failed,
          avg_latency_ms:
            (m.avg_latency_ms * m.total + log.latency_ms) / total,
          recent: [log, ...m.recent].slice(0, 100),
        };
      });
      if (log.stream) {
        // 流式请求开始（Rust 侧在响应头返回时推送日志）
        streamCount.current += 1;
        setStreaming(true);
        setTimeout(() => {
          streamCount.current = Math.max(0, streamCount.current - 1);
          if (streamCount.current === 0) setStreaming(false);
        }, 1500);
      }
    });
    return () => {
      unsub.then((f) => f());
    };
  }, []);

  // 配置热重载
  useEffect(() => {
    const unsub = onConfigChanged(() => {
      refresh();
    });
    return () => {
      unsub.then((f) => f());
    };
  }, [refresh]);

  const save = useCallback(async (cfg: AppConfig) => {
    await api.saveConfig(cfg);
    setConfig(cfg);
    setBreakers(await api.getBreakerStatus());
  }, []);

  const resetBreaker = useCallback(async (providerId: string) => {
    await api.resetBreaker(providerId);
    setBreakers(await api.getBreakerStatus());
  }, []);

  return {
    config,
    status,
    metrics,
    breakers,
    error,
    streaming,
    refresh,
    save,
    resetBreaker,
  };
}
