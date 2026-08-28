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
  /** 各用户的已用请求数：{ id: 次数 } */
  userUsage: Record<string, number>;
  /** 最近一次刷新错误 */
  error: string | null;
  /** 有流式请求在途（驱动顶部脉冲条） */
  streaming: boolean;
  refresh: () => Promise<void>;
  save: (cfg: AppConfig) => Promise<void>;
  resetBreaker: (providerId: string) => Promise<void>;
  resetUserUsage: (id?: string | null) => Promise<void>;
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
  const [userUsage, setUserUsage] = useState<Record<string, number>>({});
  const [error, setError] = useState<string | null>(null);
  const [streaming, setStreaming] = useState(false);
  const streamCount = useRef(0);

  const refresh = useCallback(async () => {
    try {
      const [cfg, st, ms, bs, usage] = await Promise.all([
        api.getConfig(),
        api.getStatus(),
        api.getMetrics(),
        api.getBreakerStatus(),
        api.getUserUsage(),
      ]);
      setConfig(cfg);
      setStatus(st);
      setMetrics(ms);
      setBreakers(bs);
      setUserUsage(usage);
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
  // 使用 ref 标记是否已取消，防止 StrictMode 双挂载下 listen Promise 未 resolve
  // 时 cleanup 执行导致短暂双订阅
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;

    onProxyLog((log: RequestLog) => {
      if (cancelled) return;
      setMetrics((m) => {
        // 按 ID 去重：如果已存在则更新，否则插入到最前
        const existingIdx = m.recent.findIndex((r) => r.id === log.id);
        let recent: RequestLog[];
        let total: number;
        let success: number;
        let failed: number;
        let cacheHits: number;

        if (existingIdx >= 0) {
          // 更新已有条目（重试/流式结束后的局部刷新）
          const prev = m.recent[existingIdx];
          recent = m.recent.slice();
          recent[existingIdx] = log;
          // 只有从"进行中"(status=0) 变为完成时才更新计数
          const wasPending = prev.status === 0;
          const isDone = log.status !== 0;
          if (wasPending && isDone) {
            total = m.total + 1;
            success = m.success + (log.status >= 200 && log.status < 400 ? 1 : 0);
            failed = m.failed + (log.status >= 400 || log.error ? 1 : 0);
          } else {
            total = m.total;
            success = m.success;
            failed = m.failed;
          }
          // 缓存命中：只在首次变为 true 时累加
          cacheHits = (m.cache_hits ?? 0) + (!prev.cache_hit && log.cache_hit ? 1 : 0);
        } else {
          // 新日志条目
          recent = [log, ...m.recent].slice(0, 100);
          const isPending = log.status === 0;
          total = isPending ? m.total : m.total + 1;
          success = isPending
            ? m.success
            : m.success + (log.status >= 200 && log.status < 400 ? 1 : 0);
          failed = isPending
            ? m.failed
            : m.failed + (log.status >= 400 || log.error ? 1 : 0);
          cacheHits = (m.cache_hits ?? 0) + (log.cache_hit ? 1 : 0);
        }

        return {
          ...m,
          total,
          success,
          failed,
          cache_hits: cacheHits,
          avg_latency_ms:
            total > 0 ? (m.avg_latency_ms * (total - 1) + log.latency_ms) / total : 0,
          recent,
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
    }).then((f) => {
      if (cancelled) {
        // cleanup 已执行，立即取消订阅
        f();
      } else {
        unlisten = f;
      }
    });

    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

  // 配置热重载
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;

    onConfigChanged(() => {
      if (!cancelled) refresh();
    }).then((f) => {
      if (cancelled) {
        f();
      } else {
        unlisten = f;
      }
    });

    return () => {
      cancelled = true;
      if (unlisten) unlisten();
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

  const resetUserUsage = useCallback(async (id?: string | null) => {
    await api.resetUserUsage(id ?? null);
    setUserUsage(await api.getUserUsage());
  }, []);

  return {
    config,
    status,
    metrics,
    breakers,
    userUsage,
    error,
    streaming,
    refresh,
    save,
    resetBreaker,
    resetUserUsage,
  };
}
