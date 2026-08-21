import { useMemo, useState } from "react";
import type { AppData } from "../hooks/useAppData";
import { fmtDuration, fmtTime } from "../components/ui";
import { Inbox, RefreshCw, ScrollText } from "lucide-react";

type Filter = "all" | "ok" | "err" | "retry";

const FILTERS: { key: Filter; label: string }[] = [
  { key: "all", label: "全部" },
  { key: "ok", label: "成功" },
  { key: "err", label: "失败" },
  { key: "retry", label: "含重试" },
];

export function Logs({ data }: { data: AppData }) {
  const { metrics } = data;
  const [filter, setFilter] = useState<Filter>("all");

  const rows = useMemo(() => {
    const list = metrics.recent;
    switch (filter) {
      case "ok":
        return list.filter((r) => r.status < 400 && !r.error);
      case "err":
        return list.filter((r) => r.status >= 400 || r.error);
      case "retry":
        return list.filter((r) => r.retries > 0);
      default:
        return list;
    }
  }, [metrics.recent, filter]);

  return (
    <>
      <div className="card" style={{ padding: "12px 20px", display: "flex", gap: 8, alignItems: "center" }}>
        {FILTERS.map((f) => (
          <button
            key={f.key}
            className={`chip toggle ${filter === f.key ? "on" : ""}`}
            onClick={() => setFilter(f.key)}
          >
            {f.label}
          </button>
        ))}
        <span style={{ marginLeft: "auto", color: "var(--text-3)", fontSize: 12 }}>
          显示最近 {rows.length} / {metrics.recent.length} 条（服务重启后清空）
        </span>
      </div>

      <div className="card" style={{ padding: 0, overflow: "hidden" }}>
        {rows.length === 0 ? (
          <div className="empty">
            {filter === "all" ? <Inbox size={28} /> : <ScrollText size={28} />}
            <div>
              {filter === "all"
                ? "还没有请求记录。客户端调用一次后这里会实时滚动。"
                : "当前筛选条件下没有记录。"}
            </div>
          </div>
        ) : (
          <div style={{ overflowX: "auto" }}>
            <table className="table" style={{ minWidth: 860 }}>
              <thead>
                <tr>
                  <th>时间</th>
                  <th>状态</th>
                  <th>模型</th>
                  <th>端点</th>
                  <th>Provider</th>
                  <th style={{ textAlign: "right" }}>延迟</th>
                  <th style={{ textAlign: "right" }}>Tokens (入/出)</th>
                  <th>错误</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r) => (
                  <tr key={r.id}>
                    <td className="mono" style={{ color: "var(--text-3)" }}>{fmtTime(r.ts)}</td>
                    <td>
                      <span
                        className={`status-badge ${
                          r.status >= 400 || r.error ? "err" : "ok"
                        } ${r.retries > 0 ? "retry" : ""}`}
                      >
                        {r.status}
                        {r.retries > 0 && (
                          <span style={{ display: "inline-flex", alignItems: "center", gap: 3 }}>
                            <RefreshCw size={10} />
                            {r.retries}
                          </span>
                        )}
                      </span>
                    </td>
                    <td>
                      <span className="mono" style={{ fontWeight: 550 }}>{r.model || "—"}</span>
                    </td>
                    <td className="mono" style={{ color: "var(--text-3)" }}>{r.path}</td>
                    <td className="mono" style={{ color: "var(--text-2)" }}>{r.provider ?? "—"}</td>
                    <td className="mono" style={{ textAlign: "right" }}>
                      {r.stream ? "流 " : ""}
                      {fmtDuration(r.latency_ms)}
                    </td>
                    <td className="mono" style={{ textAlign: "right", color: "var(--text-3)" }}>
                      {r.tokens_in ?? "—"} / {r.tokens_out ?? "—"}
                    </td>
                    <td className="mono" style={{ color: "var(--rose)", fontSize: 11.5, maxWidth: 220 }}>
                      <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", display: "block" }} title={r.error ?? ""}>
                        {r.error ?? ""}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </>
  );
}
