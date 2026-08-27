import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/tauri";
import type { DatasetRecord } from "../lib/types";
import { Button, fmtCount, fmtTime, useToast } from "../components/ui";
import {
  Database,
  Download,
  Inbox,
  Search,
  Trash2,
  Eye,
} from "lucide-react";
import { DatasetDetailModal } from "../components/DatasetDetail";

export function Dataset() {
  const toast = useToast();
  const [records, setRecords] = useState<DatasetRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(true);
  const [filter, setFilter] = useState("");
  const [page, setPage] = useState(0);
  const [selected, setSelected] = useState<DatasetRecord | null>(null);
  const pageSize = 50;

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [recs, cnt] = await Promise.all([
        api.getDataset(pageSize, page * pageSize, filter || undefined),
        api.getDatasetCount(filter || undefined),
      ]);
      setRecords(recs);
      setTotal(cnt);
    } catch (e) {
      toast(`加载数据集失败：${e}`, "err");
    } finally {
      setLoading(false);
    }
  }, [page, filter, toast]);

  useEffect(() => {
    load();
  }, [load]);

  const handleExport = async () => {
    try {
      const jsonl = await api.exportDatasetJsonl(filter || undefined);
      if (!jsonl) {
        toast("数据集为空，无可导出内容", "err");
        return;
      }
      const blob = new Blob([jsonl], { type: "application/jsonl" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `dataset-${Date.now()}.jsonl`;
      a.click();
      URL.revokeObjectURL(url);
      toast(`已导出 ${total} 条数据`);
    } catch (e) {
      toast(`导出失败：${e}`, "err");
    }
  };

  const handleClear = async () => {
    if (!confirm(`确认清空全部 ${total} 条数据集记录？此操作不可撤销。`)) return;
    try {
      await api.clearDataset();
      toast("已清空数据集");
      setPage(0);
      load();
    } catch (e) {
      toast(`清空失败：${e}`, "err");
    }
  };

  const handleDelete = async (id: number) => {
    try {
      await api.deleteDatasetRecord(id);
      toast("已删除");
      load();
    } catch (e) {
      toast(`删除失败：${e}`, "err");
    }
  };

  const totalPages = Math.max(1, Math.ceil(total / pageSize));

  return (
    <>
      {/* 工具栏 */}
      <div
        className="card"
        style={{
          padding: "14px 20px",
          display: "flex",
          gap: 12,
          alignItems: "center",
          flexWrap: "wrap",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 8, flex: 1, minWidth: 200 }}>
          <Database size={16} style={{ color: "var(--accent)" }} />
          <span style={{ color: "var(--text-2)", fontSize: 13 }}>
            共 <span className="mono" style={{ color: "var(--text-1)" }}>{fmtCount(total)}</span> 条数据
          </span>
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <Search size={14} style={{ color: "var(--text-3)" }} />
          <input
            className="input mono"
            style={{ width: 180 }}
            placeholder="按模型名筛选…"
            value={filter}
            onChange={(e) => {
              setFilter(e.target.value);
              setPage(0);
            }}
          />
        </div>
        <Button size="sm" onClick={handleExport} disabled={total === 0}>
          <Download size={13} /> 导出 JSONL
        </Button>
        <Button size="sm" variant="danger" onClick={handleClear} disabled={total === 0}>
          <Trash2 size={13} /> 清空
        </Button>
      </div>

      {/* 数据表 */}
      <div className="card" style={{ padding: 0, overflow: "hidden" }}>
        {loading ? (
          <div className="empty">
            <div style={{ color: "var(--text-3)" }}>加载中…</div>
          </div>
        ) : records.length === 0 ? (
          <div className="empty">
            <Inbox size={28} />
            <div>
              数据集为空。成功的请求会自动存入此处。
              <br />
              <span style={{ fontSize: 12, color: "var(--text-3)" }}>
                发起一次 chat/completions 请求后，数据会出现在这里。
              </span>
            </div>
          </div>
        ) : (
          <div style={{ overflowX: "auto" }}>
            <table className="table" style={{ minWidth: 920 }}>
              <thead>
                <tr>
                  <th>时间</th>
                  <th>模型</th>
                  <th>上游模型</th>
                  <th>Provider</th>
                  <th>流式</th>
                  <th style={{ textAlign: "right" }}>Tokens</th>
                  <th>缓存</th>
                  <th style={{ width: 80 }}></th>
                </tr>
              </thead>
              <tbody>
                {records.map((r) => (
                  <tr key={r.id}>
                    <td className="mono" style={{ color: "var(--text-3)" }}>
                      {fmtTime(r.ts)}
                    </td>
                    <td>
                      <span className="mono" style={{ fontWeight: 600 }}>
                        {r.model}
                      </span>
                    </td>
                    <td className="mono" style={{ color: "var(--text-3)" }}>
                      {r.upstream_model ?? "—"}
                    </td>
                    <td className="mono" style={{ color: "var(--text-2)" }}>
                      {r.provider ?? "—"}
                    </td>
                    <td>
                      {r.stream ? (
                        <span className="chip on">流</span>
                      ) : (
                        <span className="mono" style={{ color: "var(--text-3)" }}>
                          —
                        </span>
                      )}
                    </td>
                    <td className="mono" style={{ textAlign: "right", color: "var(--text-3)" }}>
                      {r.tokens_in ?? "—"} / {r.tokens_out ?? "—"}
                    </td>
                    <td>
                      {r.cache_hit ? (
                        <span className="chip" style={{ color: "var(--green)" }}>
                          命中
                        </span>
                      ) : (
                        <span className="mono" style={{ color: "var(--text-3)" }}>
                          —
                        </span>
                      )}
                    </td>
                    <td style={{ textAlign: "right" }}>
                      <div style={{ display: "flex", gap: 4, justifyContent: "flex-end" }}>
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => setSelected(r)}
                          title="查看详情"
                        >
                          <Eye size={13} />
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          onClick={() => handleDelete(r.id)}
                          title="删除"
                        >
                          <Trash2 size={13} />
                        </Button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* 分页 */}
      {totalPages > 1 && (
        <div style={{ display: "flex", justifyContent: "center", gap: 8, alignItems: "center" }}>
          <Button size="sm" variant="ghost" onClick={() => setPage((p) => Math.max(0, p - 1))} disabled={page === 0}>
            上一页
          </Button>
          <span className="mono" style={{ color: "var(--text-3)", fontSize: 12 }}>
            {page + 1} / {totalPages}
          </span>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
            disabled={page >= totalPages - 1}
          >
            下一页
          </Button>
        </div>
      )}

      {selected && (
        <DatasetDetailModal record={selected} onClose={() => setSelected(null)} />
      )}
    </>
  );
}
