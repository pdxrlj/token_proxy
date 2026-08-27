import { useState } from "react";
import type { DatasetRecord } from "../lib/types";
import { Modal, fmtBytes } from "./ui";
import { Check, Copy, FileText, MessageSquare } from "lucide-react";

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

function JsonBlock({ label, json }: { label: string; json: string }) {
  const [copied, setCopied] = useState(false);
  const [expanded, setExpanded] = useState(false);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(json);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* ignore */
    }
  };

  let pretty = json;
  try {
    pretty = JSON.stringify(JSON.parse(json), null, 2);
  } catch {
    /* not JSON, show raw */
  }

  return (
    <div style={{ marginTop: 8 }}>
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          marginBottom: 6,
        }}
      >
        <span
          style={{
            fontSize: 12,
            fontWeight: 600,
            color: "var(--text-2)",
            display: "flex",
            alignItems: "center",
            gap: 6,
          }}
        >
          <FileText size={13} /> {label}
        </span>
        <div style={{ display: "flex", gap: 6 }}>
          <button
            className="btn ghost sm"
            onClick={() => setExpanded((e) => !e)}
            style={{ fontSize: 11 }}
          >
            {expanded ? "折叠" : "展开"}
          </button>
          <button className="btn ghost sm" onClick={copy} style={{ fontSize: 11 }}>
            {copied ? <Check size={12} /> : <Copy size={12} />}
            {copied ? "已复制" : "复制"}
          </button>
        </div>
      </div>
      <pre
        className="mono"
        style={{
          background: "var(--bg-2)",
          border: "1px solid var(--border-soft)",
          borderRadius: 8,
          padding: 12,
          fontSize: 11.5,
          lineHeight: 1.5,
          maxHeight: expanded ? "none" : 160,
          overflow: expanded ? "visible" : "auto",
          whiteSpace: "pre-wrap",
          wordBreak: "break-all",
          color: "var(--text-2)",
          margin: 0,
        }}
      >
        {pretty}
      </pre>
    </div>
  );
}

export function DatasetDetailModal({
  record,
  onClose,
}: {
  record: DatasetRecord;
  onClose: () => void;
}) {
  return (
    <Modal title={`数据记录 #${record.id}`} onClose={onClose}>
      {/* 元信息 */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 10,
          flexWrap: "wrap",
          padding: "12px 14px",
          background: "var(--bg-2)",
          borderRadius: 10,
          border: "1px solid var(--border-soft)",
        }}
      >
        <span className="mono" style={{ fontWeight: 650, fontSize: 14 }}>
          {record.model}
        </span>
        {record.stream && <span className="chip on">流式</span>}
        {record.cache_hit && (
          <span className="chip" style={{ color: "var(--green)" }}>
            缓存命中
          </span>
        )}
        <span className="chip">{record.provider ?? "—"}</span>
      </div>

      {/* 字段明细 */}
      <div style={{ marginTop: 6 }}>
        <Row k="记录 ID" v={record.id} mono />
        <Row k="时间" v={fmtDateTime(record.ts)} mono />
        <Row k="模型" v={record.model} mono />
        <Row k="上游模型" v={record.upstream_model ?? "—"} mono />
        <Row k="Provider" v={record.provider ?? "—"} />
        <Row k="流式" v={record.stream ? "是" : "否"} />
        <Row
          k="Tokens（入/出）"
          v={
            record.tokens_in != null || record.tokens_out != null
              ? `${record.tokens_in ?? "—"} / ${record.tokens_out ?? "—"}`
              : "—"
          }
          mono
        />
        <Row k="缓存命中" v={record.cache_hit ? "是" : "否"} />
        <Row k="请求体大小" v={fmtBytes(record.request_body.length)} mono />
        <Row k="响应体大小" v={fmtBytes(record.response_body.length)} mono />
      </div>

      {/* JSON 块 */}
      <div style={{ display: "flex", alignItems: "center", gap: 6, marginTop: 14, marginBottom: 4 }}>
        <MessageSquare size={14} style={{ color: "var(--accent)" }} />
        <span style={{ fontSize: 13, fontWeight: 600, color: "var(--text-2)" }}>
          请求与响应
        </span>
      </div>
      <JsonBlock label="请求体（OpenAI 格式）" json={record.request_body} />
      <JsonBlock label="响应体" json={record.response_body} />

      {/* JSONL 格式 */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 6,
          marginTop: 14,
          marginBottom: 4,
        }}
      >
        <FileText size={14} style={{ color: "var(--green)" }} />
        <span style={{ fontSize: 13, fontWeight: 600, color: "var(--text-2)" }}>
          JSONL（可用于 OpenAI fine-tuning）
        </span>
      </div>
      <JsonBlock label="finetune_jsonl" json={record.finetune_jsonl} />
    </Modal>
  );
}
