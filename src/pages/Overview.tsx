import type { CSSProperties } from "react";
import { useState } from "react";
import type { AppData } from "../hooks/useAppData";
import {
  Button,
  StatusDot,
  fmtBytes,
  fmtCount,
  fmtDuration,
  fmtTime,
  useToast,
} from "../components/ui";
import { LogDetailModal } from "../components/LogDetail";
import {
  ArrowDownToLine,
  ArrowUpFromLine,
  Copy,
  Database,
  Gauge,
  Inbox,
  Radio,
  Waypoints,
  Zap,
} from "lucide-react";
import type { BreakerStateName, RequestLog } from "../lib/types";

function breakerColor(state?: BreakerStateName): "cyan" | "amber" | "rose" | "gray" {
  if (!state) return "cyan";
  if (state === "open") return "rose";
  if (state === "half_open") return "amber";
  return "cyan";
}

function linkClass(state?: BreakerStateName): string {
  if (!state) return "";
  if (state === "open") return "off";
  if (state === "half_open") return "warn";
  return "";
}

export function Overview({ data }: { data: AppData }) {
  const { config, status, metrics, breakers } = data;
  const toast = useToast();
  const [copied, setCopied] = useState(false);
  const [detail, setDetail] = useState<RequestLog | null>(null);

  const endpoint = status ? `${status.host}:${status.port}` : "…";
  const serverDown = !!status && !status.running;
  const successRate =
    metrics.total > 0 ? Math.round((metrics.success / metrics.total) * 100) : 100;
  const cacheHitRate =
    metrics.total > 0 && metrics.cache_hits != null
      ? Math.round((metrics.cache_hits / metrics.total) * 100)
      : 0;

  const copyEndpoint = async () => {
    try {
      await navigator.clipboard.writeText(`http://${endpoint}/v1`);
      setCopied(true);
      toast("已复制 base_url 到剪贴板");
      setTimeout(() => setCopied(false), 1800);
    } catch {
      toast("复制失败，请手动复制", "err");
    }
  };

  // 路由链路：routes 优先，未配置的 provider 模型作为 fallback 展示
  const links: { model: string; provider: string; providerName: string; breaker?: BreakerStateName }[] = [];
  const seen = new Set<string>();
  for (const r of config.routes) {
    const p = config.providers.find((x) => x.id === r.provider);
    if (!p) continue;
    links.push({
      model: r.model,
      provider: p.id,
      providerName: p.name,
      breaker: breakers[p.id]?.state,
    });
    seen.add(r.model);
  }
  for (const p of config.providers) {
    if (!p.enabled) continue;
    for (const m of p.models) {
      if (!seen.has(m)) {
        links.push({ model: m, provider: p.id, providerName: p.name, breaker: breakers[p.id]?.state });
        seen.add(m);
      }
    }
  }

  const recent = metrics.recent.slice(0, 8);

  return (
    <>
      {/* 服务器信息 */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">代理服务</span>
          {status && (
            <span className="pill">
              <Radio size={13} />
              运行 {fmtDuration(status.uptime_secs * 1000)}
            </span>
          )}
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: 12, flexWrap: "wrap" }}>
          <span
            className={`pill ${serverDown ? "warn" : "ok"}`}
            style={{ fontSize: 14, padding: "8px 16px" }}
          >
            <StatusDot color={serverDown ? "rose" : "cyan"} />
            <span className="mono" style={{ fontSize: 13.5 }}>
              http://{endpoint}/v1
            </span>
          </span>
          <Button size="sm" onClick={copyEndpoint}>
            <Copy size={13} />
            {copied ? "已复制" : "复制地址"}
          </Button>
          <span style={{ color: "var(--text-3)", fontSize: 12 }}>
            任何 OpenAI 兼容客户端把 base_url 指向这里即可
          </span>
        </div>
        {status?.server_error && (
          <div style={{ marginTop: 12, fontSize: 12, color: "var(--rose)" }}>
            代理未成功监听：{status.server_error}
          </div>
        )}
        {status && (
          <div style={{ marginTop: 12, fontSize: 11.5, color: "var(--text-3)", fontFamily: "var(--font-mono)" }}>
            配置文件：{status.config_path}
          </div>
        )}
      </div>

      {/* 统计 */}
      <div className="grid-4">
        <div className="stat" style={{ "--stat-accent": "var(--accent)" } as CSSProperties}>
          <div className="label">
            <Inbox size={13} /> 请求总数
          </div>
          <div className="value">{fmtCount(metrics.total)}</div>
          <div className="trend">
            重试 {fmtCount(metrics.retries)} 次 · 失败 {fmtCount(metrics.failed)}
          </div>
        </div>
        <div className="stat" style={{ "--stat-accent": "var(--green)" } as CSSProperties}>
          <div className="label">
            <Gauge size={13} /> 成功率
          </div>
          <div className="value">
            {successRate}
            <small>%</small>
          </div>
          <div className="trend">
            成功 {fmtCount(metrics.success)} 次
          </div>
        </div>
        <div className="stat" style={{ "--stat-accent": "var(--amber)" } as CSSProperties}>
          <div className="label">
            <Zap size={13} /> 平均延迟
          </div>
          <div className="value">{fmtDuration(metrics.avg_latency_ms)}</div>
          <div className="trend">首字节到完成</div>
        </div>
        <div className="stat" style={{ "--stat-accent": "var(--accent-strong)" } as CSSProperties}>
          <div className="label">
            <ArrowUpFromLine size={13} /> Token 用量
          </div>
          <div className="value">
            {fmtCount(metrics.tokens_in + metrics.tokens_out)}
          </div>
          <div className="trend">
            <ArrowDownToLine size={11} style={{ verticalAlign: -1 }} /> 入 {fmtCount(metrics.tokens_in)} · 出{" "}
            {fmtCount(metrics.tokens_out)}
          </div>
        </div>
        <div className="stat" style={{ "--stat-accent": "var(--green)" } as CSSProperties}>
          <div className="label">
            <Database size={13} /> 缓存命中率
          </div>
          <div className="value">
            {cacheHitRate}
            <small>%</small>
          </div>
          <div className="trend">
            命中 {fmtCount(metrics.cache_hits ?? 0)} / {fmtCount(metrics.total)} 次
          </div>
        </div>
        <div className="stat" style={{ "--stat-accent": "var(--amber)" } as CSSProperties}>
          <div className="label">
            <ArrowDownToLine size={13} /> 流量
          </div>
          <div className="value" style={{ fontSize: 22 }}>
            {fmtBytes((metrics.bytes_out ?? 0))}
          </div>
          <div className="trend">
            入 {fmtBytes(metrics.bytes_in ?? 0)} · 出 {fmtBytes(metrics.bytes_out ?? 0)}
          </div>
        </div>
      </div>

      {/* 路由链路图（签名元素） */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">
            路由链路
            <span className="hint">模型 → 上游 Provider · 虚线流动表示流量在途</span>
          </span>
          {links.length > 0 && (
            <span className="pill">
              {config.providers.filter((p) => p.enabled).length} 个启用 Provider ·{" "}
              {links.length} 个模型
            </span>
          )}
        </div>
        {links.length === 0 ? (
          <div className="empty">
            <Waypoints size={28} />
            <div>还没有任何模型路由。去「上游 Provider」添加服务商，或在「路由」配置映射。</div>
          </div>
        ) : (
          <div className="link-list">
            {links.slice(0, 12).map((l) => (
              <div className="link-row" key={`${l.provider}-${l.model}`}>
                <span className="link-model" title={l.model}>
                  {l.model}
                </span>
                <span className={`link-line ${linkClass(l.breaker)}`} />
                <span className="link-provider">
                  <StatusDot color={breakerColor(l.breaker)} />
                  {l.providerName}
                </span>
                <span className="link-meta">
                  {l.breaker === "open"
                    ? "已熔断"
                    : l.breaker === "half_open"
                      ? "试探中"
                      : "正常"}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* 最近请求 */}
      <div className="card">
        <div className="card-header">
          <span className="card-title">最近请求</span>
          <span style={{ color: "var(--text-3)", fontSize: 12 }}>点击行查看详情</span>
        </div>
        {recent.length === 0 ? (
          <div className="empty">
            <Inbox size={28} />
            <div>还没有请求。用任意 OpenAI 客户端调用一次试试。</div>
          </div>
        ) : (
          <div className="log-list">
            {recent.map((r) => (
              <div
                className="log-row clickable"
                key={r.id}
                onClick={() => setDetail(r)}
                title="点击查看详情"
              >
                <span className="time">{fmtTime(r.ts)}</span>
                <span>
                  <span
                    className={`status-badge ${r.status < 400 ? "ok" : "err"} ${r.retries > 0 ? "retry" : ""}`}
                  >
                    {r.status}
                    {r.retries > 0 ? `·重试${r.retries}` : ""}
                  </span>
                </span>
                <span className="model">{r.model || "—"}</span>
                <span className="path">{r.path}</span>
                <span className="lat">
                  {r.stream ? "流式 " : ""}
                  {fmtDuration(r.latency_ms)}
                </span>
                <span className="lat" style={{ color: "var(--text-3)" }}>
                  {r.provider ?? "—"}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      {detail && <LogDetailModal log={detail} onClose={() => setDetail(null)} />}
    </>
  );
}
