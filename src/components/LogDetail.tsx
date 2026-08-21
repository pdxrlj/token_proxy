import type { RequestLog } from "../lib/types";
import { Modal, fmtDuration } from "./ui";
import { Check, Copy, RefreshCw, X } from "lucide-react";
import { useState } from "react";

function fmtDateTime(ts: number): string {
  const d = new Date(ts);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(
    d.getHours(),
  )}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
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
      </div>

      {/* 字段明细 */}
      <div style={{ marginTop: 6 }}>
        <Row k="请求 ID" v={log.id} mono />
        <Row k="时间" v={fmtDateTime(log.ts)} mono />
        <Row k="方法" v={log.method} mono />
        <Row k="端点" v={log.path} mono />
        <Row k="Provider" v={log.provider ?? "—"} />
        <Row k="延迟" v={fmtDuration(log.latency_ms)} mono />
        <Row
          k="Tokens（入/出）"
          v={
            log.tokens_in != null || log.tokens_out != null
              ? `${log.tokens_in ?? "—"} / ${log.tokens_out ?? "—"}`
              : "—"
          }
          mono
        />
      </div>

      {/* 错误详情（完整信息 + 复制） */}
      {log.error && (
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
