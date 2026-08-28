import type { ErrorDetail, ErrorKind, RequestLog } from "../lib/types";
import { Modal, fmtBytes, fmtCount, fmtDuration } from "./ui";
import { AlertTriangle, Check, Copy, Lightbulb, RefreshCw, X } from "lucide-react";
import { useState } from "react";

function fmtDateTime(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(
    d.getHours(),
  )}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** 各错误大类的中文标题 */
const KIND_TITLE: Record<ErrorKind, string> = {
  upstream: "上游返回错误",
  circuit_open: "渠道已熔断",
  network: "网络连接失败",
  timeout: "请求超时",
  route_not_found: "找不到可用路由",
  bad_request: "请求参数有误",
  unauthorized: "鉴权失败",
  internal: "代理内部错误",
};

/** 把秒数格式化为「1 分 30 秒」这类可读文本 */
function fmtCooldown(secs: number): string {
  if (secs <= 0) return "可立即重试";
  if (secs < 60) return `约 ${secs} 秒后自动恢复`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return s === 0 ? `约 ${m} 分钟后自动恢复` : `约 ${m} 分 ${s} 秒后自动恢复`;
}

/** 结构化错误详情面板：摘要 / 原始错误 / 上下文 / 排查建议 */
function ErrorPanel({ detail, raw }: { detail: ErrorDetail; raw: string }) {
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    const text = [
      `【${KIND_TITLE[detail.kind]}】${detail.summary}`,
      detail.detail ? `详情：${detail.detail}` : "",
      detail.hint ? `建议：${detail.hint}` : "",
      detail.provider ? `渠道：${detail.provider}` : "",
      detail.endpoint ? `端点：${detail.endpoint}` : "",
    ]
      .filter(Boolean)
      .join("\n");
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* 忽略剪贴板失败 */
    }
  };

  // 上下文行：只在有值时渲染，避免满屏「—」
  const ctxRows: Array<[string, string]> = [];
  if (detail.provider) ctxRows.push(["失败渠道", detail.provider]);
  if (detail.upstream_model) ctxRows.push(["上游模型", detail.upstream_model]);
  if (detail.upstream_status != null)
    ctxRows.push(["上游状态码", String(detail.upstream_status)]);
  if (detail.endpoint) ctxRows.push(["请求端点", detail.endpoint]);
  if (detail.candidates_tried != null && detail.total_candidates != null)
    ctxRows.push(["已试渠道", `${detail.candidates_tried} / ${detail.total_candidates}`]);
  if (detail.retries) ctxRows.push(["重试次数", `${detail.retries} 次`]);
  if (detail.consecutive_failures)
    ctxRows.push(["连续失败", `${detail.consecutive_failures} 次`]);
  if (detail.cooldown_remaining_secs != null)
    ctxRows.push(["熔断恢复", fmtCooldown(detail.cooldown_remaining_secs)]);

  return (
    <div
      style={{
        marginTop: 10,
        background: "var(--rose-soft)",
        border: "1px solid rgba(251,113,133,.3)",
        borderRadius: 12,
        overflow: "hidden",
      }}
    >
      {/* 头部：标题 + 复制 */}
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          gap: 8,
          padding: "11px 14px",
          borderBottom: "1px solid rgba(251,113,133,.2)",
        }}
      >
        <span
          style={{
            display: "flex",
            alignItems: "center",
            gap: 6,
            color: "var(--rose)",
            fontWeight: 600,
            fontSize: 12.5,
          }}
        >
          <AlertTriangle size={13} />
          {KIND_TITLE[detail.kind]}
        </span>
        <button
          className="btn ghost sm"
          onClick={copy}
          style={{ padding: "3px 8px", fontSize: 11.5 }}
        >
          {copied ? <Check size={12} /> : <Copy size={12} />}
          {copied ? "已复制" : "复制"}
        </button>
      </div>

      <div style={{ padding: "11px 14px", display: "grid", gap: 10 }}>
        {/* 摘要 */}
        <div style={{ fontSize: 13, color: "var(--text-1)", lineHeight: 1.6 }}>
          {detail.summary}
        </div>

        {/* 上游原始错误 */}
        {detail.detail && (
          <div>
            <div
              style={{
                fontSize: 11,
                color: "var(--text-3)",
                marginBottom: 4,
                fontWeight: 600,
              }}
            >
              原始错误
            </div>
            <div
              className="mono"
              style={{
                fontSize: 11.5,
                color: "var(--rose)",
                background: "var(--bg-1)",
                border: "1px solid var(--border-soft)",
                borderRadius: 8,
                padding: "8px 10px",
                whiteSpace: "pre-wrap",
                wordBreak: "break-all",
                lineHeight: 1.6,
                maxHeight: 160,
                overflowY: "auto",
              }}
            >
              {detail.detail}
            </div>
          </div>
        )}

        {/* 上下文 */}
        {ctxRows.length > 0 && (
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "auto 1fr",
              gap: "4px 12px",
              fontSize: 12,
            }}
          >
            {ctxRows.map(([k, v]) => (
              <div key={k} style={{ display: "contents" }}>
                <span style={{ color: "var(--text-3)" }}>{k}</span>
                <span
                  className="mono"
                  style={{ color: "var(--text-1)", wordBreak: "break-all" }}
                >
                  {v}
                </span>
              </div>
            ))}
          </div>
        )}

        {/* 排查建议 */}
        {detail.hint && (
          <div
            style={{
              display: "flex",
              gap: 7,
              padding: "9px 11px",
              background: "var(--bg-1)",
              border: "1px solid var(--border-soft)",
              borderRadius: 8,
            }}
          >
            <Lightbulb
              size={13}
              style={{ color: "var(--amber)", flex: "none", marginTop: 1 }}
            />
            <span style={{ fontSize: 12, color: "var(--text-2)", lineHeight: 1.65 }}>
              {detail.hint}
            </span>
          </div>
        )}

        {/* 兜底：无结构化详情时展示原始错误字符串 */}
        {!detail.detail && raw && (
          <div
            className="mono"
            style={{
              fontSize: 11.5,
              color: "var(--text-3)",
              whiteSpace: "pre-wrap",
              wordBreak: "break-all",
            }}
          >
            {raw}
          </div>
        )}
      </div>
    </div>
  );
}

function Row({ k, v, mono }: { k: string; v: React.ReactNode; mono?: boolean }) {
  return (
    <div className="key-value">
      <span className="k">{k}</span>
      <span className={`v ${mono ? "mono" : ""}`}>{v}</span>
    </div>
  );
}

/** 请求日志详情弹窗：点击日志行后展示完整字段 */
export function LogDetailModal({
  log,
  onClose,
}: {
  log: RequestLog;
  onClose: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const failed = log.status >= 400 || !!log.error;

  const copyError = async () => {
    if (!log.error) return;
    try {
      await navigator.clipboard.writeText(log.error);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* 忽略剪贴板失败 */
    }
  };

  return (
    <Modal title="请求详情" onClose={onClose}>
      {/* 顶部：模型 + 状态 + 重试 */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 10,
          flexWrap: "wrap",
          padding: "12px 14px",
          background: "var(--bg-2)",
          borderRadius: 12,
          border: "1px solid var(--border-soft)",
        }}
      >
        <span className="mono" style={{ fontWeight: 650, fontSize: 14 }}>
          {log.model || "—"}
        </span>
        <span className={`status-badge ${failed ? "err" : "ok"}`}>{log.status}</span>
        <span
          className={`status-badge ${log.retries > 0 ? "retry" : ""}`}
          style={log.retries === 0 ? { color: "var(--text-3)", background: "var(--bg-3)" } : undefined}
          title="该请求在上游失败后自动重发的次数"
        >
          <RefreshCw size={11} />
          重试 {log.retries} 次
        </span>
        {log.stream && <span className="chip">流式</span>}
        {log.cache_hit && (
          <span className="chip" style={{ color: "var(--green)", background: "rgba(16,185,129,.12)" }}>
            缓存命中
            {log.cached_tokens != null && log.cached_tokens > 0
              ? ` · ${fmtCount(log.cached_tokens)} tok`
              : ""}
          </span>
        )}
      </div>

      {/* 字段明细 */}
      <div style={{ marginTop: 6 }}>
        <Row k="请求 ID" v={log.id} mono />
        <Row k="时间" v={fmtDateTime(log.ts)} mono />
        <Row k="方法" v={log.method} mono />
        <Row k="端点" v={log.path} mono />
        <Row k="Provider" v={log.provider ?? "—"} />
        <Row k="上游模型" v={log.upstream_model ?? "—"} mono />
        <Row k="延迟" v={fmtDuration(log.latency_ms)} mono />
        <Row k="首字节时间" v={log.ttfb_ms != null ? fmtDuration(log.ttfb_ms) : "—"} mono />
        <Row
          k="Tokens（入/出）"
          v={
            log.tokens_in != null || log.tokens_out != null
              ? `${log.tokens_in ?? "—"} / ${log.tokens_out ?? "—"}`
              : "—"
          }
          mono
        />
        <Row k="输出大小" v={fmtBytes(log.response_bytes)} mono />
        <Row
          k="缓存命中"
          v={
            !log.cache_hit
              ? "否"
              : log.cached_tokens != null && log.cached_tokens > 0
                ? `是 · ${fmtCount(log.cached_tokens)} tok`
                : "是"
          }
        />
      </div>

      {/* 错误详情（结构化：摘要 / 原始错误 / 上下文 / 建议） */}
      {log.error_detail && <ErrorPanel detail={log.error_detail} raw={log.error ?? ""} />}

      {/* 兼容：旧数据只有扁平错误字符串，没有结构化详情 */}
      {log.error && !log.error_detail && (
        <div
          style={{
            marginTop: 10,
            padding: "12px 14px",
            background: "var(--rose-soft)",
            border: "1px solid rgba(251,113,133,.3)",
            borderRadius: 12,
          }}
        >
          <div
            style={{
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
              marginBottom: 6,
            }}
          >
            <span style={{ color: "var(--rose)", fontWeight: 600, fontSize: 12.5 }}>
              错误信息
            </span>
            <button
              className="btn ghost sm"
              onClick={copyError}
              style={{ padding: "3px 8px", fontSize: 11.5 }}
            >
              {copied ? <Check size={12} /> : <Copy size={12} />}
              {copied ? "已复制" : "复制"}
            </button>
          </div>
          <div
            className="mono"
            style={{
              fontSize: 12,
              color: "var(--rose)",
              whiteSpace: "pre-wrap",
              wordBreak: "break-all",
              lineHeight: 1.6,
              maxHeight: 180,
              overflowY: "auto",
            }}
          >
            {log.error}
          </div>
        </div>
      )}

      {!log.error && (
        <div style={{ marginTop: 10, color: "var(--text-3)", fontSize: 12.5 }}>
          <X size={12} style={{ verticalAlign: -2, marginRight: 4 }} />
          请求成功，未产生错误。
        </div>
      )}
    </Modal>
  );
}
