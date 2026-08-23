import { useState } from "react";
import type { AppData } from "../hooks/useAppData";
import type { RouteConfig } from "../lib/types";
import { Button, Field, Modal, useToast } from "../components/ui";
import { Plus, SlidersHorizontal, Trash2, Waypoints } from "lucide-react";

export function Routes({ data }: { data: AppData }) {
  const { config, save } = data;
  const toast = useToast();
  const [newRoute, setNewRoute] = useState<RouteConfig>({
    model: "",
    provider: config.providers[0]?.id ?? "",
    upstream_model: "",
    overrides: {},
  });
  const [adding, setAdding] = useState(false);
  const [overridesJson, setOverridesJson] = useState("{}");
  const [editingOverrides, setEditingOverrides] = useState<RouteConfig | null>(null);
  // 行内编辑上游模型名的本地草稿（key=route.model），失焦时提交，避免每次按键写文件
  const [draftUpstream, setDraftUpstream] = useState<Record<string, string>>({});

  const enabledProviders = config.providers.filter((p) => p.enabled);

  const addRoute = async () => {
    const model = newRoute.model.trim();
    if (!model) {
      toast("请填写模型名", "err");
      return;
    }
    if (!newRoute.provider) {
      toast("请选择上游 Provider", "err");
      return;
    }
    if (config.routes.some((r) => r.model === model)) {
      toast(`模型「${model}」已有路由`, "err");
      return;
    }
    await save({ ...config, routes: [...config.routes, newRoute] });
    setAdding(false);
    setNewRoute({ model: "", provider: config.providers[0]?.id ?? "", upstream_model: "", overrides: {} });
    toast(`已添加路由 ${model} → ${newRoute.provider}`);
  };

  const changeProvider = async (model: string, provider: string) => {
    await save({
      ...config,
      routes: config.routes.map((r) => (r.model === model ? { ...r, provider } : r)),
    });
  };

  const changeUpstreamModel = async (model: string, upstreamModel: string) => {
    const trimmed = upstreamModel.trim();
    // 值未变则不写
    const current = config.routes.find((r) => r.model === model)?.upstream_model ?? "";
    if (trimmed === current) return;
    await save({
      ...config,
      routes: config.routes.map((r) =>
        r.model === model ? { ...r, upstream_model: trimmed } : r,
      ),
    });
  };

  const removeRoute = async (model: string) => {
    await save({ ...config, routes: config.routes.filter((r) => r.model !== model) });
    toast("已删除路由");
  };

  const saveOverrides = async () => {
    if (!editingOverrides) return;
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(overridesJson || "{}");
    } catch {
      toast("overrides 不是合法 JSON", "err");
      return;
    }
    await save({
      ...config,
      routes: config.routes.map((r) =>
        r.model === editingOverrides.model ? { ...r, overrides: parsed } : r,
      ),
    });
    setEditingOverrides(null);
    toast("已保存参数改写");
  };

  const overridesSummary = (o: Record<string, unknown>) => {
    const keys = Object.keys(o);
    return keys.length === 0 ? "无" : keys.join(", ");
  };

  return (
    <>
      <div className="card" style={{ padding: "14px 20px", display: "flex", justifyContent: "space-between", alignItems: "center" }}>
        <span style={{ color: "var(--text-2)", fontSize: 13 }}>
          共 {config.routes.length} 条路由 ·{" "}
          <span style={{ color: "var(--text-3)" }}>
            未配置路由的模型会按 Provider 的模型列表自动匹配
          </span>
        </span>
        <Button variant="primary" onClick={() => setAdding(true)} disabled={enabledProviders.length === 0}>
          <Plus size={15} /> 添加路由
        </Button>
      </div>

      {config.routes.length === 0 ? (
        <div className="card">
          <div className="empty">
            <Waypoints size={30} />
            <div>还没有路由规则。模型请求会按 Provider 列表自动匹配；添加路由可精确控制转发与参数改写。</div>
          </div>
        </div>
      ) : (
        <div className="card">
          <table className="table">
            <thead>
              <tr>
                <th style={{ width: "24%" }}>模型名</th>
                <th style={{ width: "24%" }}>上游模型名</th>
                <th style={{ width: "22%" }}>上游 Provider</th>
                <th>参数改写</th>
                <th style={{ width: 120 }}></th>
              </tr>
            </thead>
            <tbody>
              {config.routes.map((r) => (
                <tr key={r.model}>
                  <td>
                    <span className="mono" style={{ fontWeight: 600, color: "var(--text-1)" }}>{r.model}</span>
                  </td>
                  <td>
                    <input
                      className="input mono"
                      style={{ width: "100%" }}
                      value={draftUpstream[r.model] ?? r.upstream_model ?? ""}
                      placeholder={r.model}
                      onChange={(e) =>
                        setDraftUpstream((d) => ({ ...d, [r.model]: e.target.value }))
                      }
                      onBlur={(e) => {
                        changeUpstreamModel(r.model, e.target.value);
                        setDraftUpstream((d) => {
                          const { [r.model]: _, ...rest } = d;
                          return rest;
                        });
                      }}
                      title="转发到上游时使用的模型名；留空则沿用客户端请求的 model"
                    />
                  </td>
                  <td>
                    <select
                      className="select mono"
                      style={{ width: "auto", minWidth: 160 }}
                      value={r.provider}
                      onChange={(e) => changeProvider(r.model, e.target.value)}
                    >
                      {config.providers.map((p) => (
                        <option key={p.id} value={p.id} disabled={!p.enabled}>
                          {p.name} ({p.id}){p.enabled ? "" : " · 已停用"}
                        </option>
                      ))}
                    </select>
                  </td>
                  <td>
                    <button
                      className="btn ghost sm"
                      onClick={() => {
                        setEditingOverrides(r);
                        setOverridesJson(JSON.stringify(r.overrides, null, 2));
                      }}
                      title="编辑参数改写（temperature 等）"
                    >
                      <SlidersHorizontal size={13} />
                      {overridesSummary(r.overrides)}
                    </button>
                  </td>
                  <td style={{ textAlign: "right" }}>
                    <Button size="sm" variant="danger" onClick={() => removeRoute(r.model)}>
                      <Trash2 size={13} />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {adding && (
        <Modal title="添加路由" onClose={() => setAdding(false)}>
          <Field label="模型名" help="客户端请求体里的 model 字段">
            <input
              className="input mono"
              value={newRoute.model}
              placeholder="gpt-4o"
              onChange={(e) => setNewRoute({ ...newRoute, model: e.target.value })}
              autoFocus
            />
          </Field>
          <Field label="上游模型名" help="转发到上游时使用的模型名；留空则沿用客户端请求的 model">
            <input
              className="input mono"
              value={newRoute.upstream_model ?? ""}
              placeholder="z-ai/glm-5.2:free"
              onChange={(e) => setNewRoute({ ...newRoute, upstream_model: e.target.value })}
            />
          </Field>
          <Field label="转发到 Provider">
            <select
              className="select"
              value={newRoute.provider}
              onChange={(e) => setNewRoute({ ...newRoute, provider: e.target.value })}
            >
              {enabledProviders.length === 0 && <option value="">（无可用 Provider）</option>}
              {enabledProviders.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name} ({p.id})
                </option>
              ))}
            </select>
          </Field>
          <div className="form-actions">
            <Button variant="ghost" onClick={() => setAdding(false)}>取消</Button>
            <Button variant="primary" onClick={addRoute}>添加</Button>
          </div>
        </Modal>
      )}

      {editingOverrides && (
        <Modal title={`参数改写 · ${editingOverrides.model}`} onClose={() => setEditingOverrides(null)}>
          <Field label="Overrides (JSON)" help={'合并进请求体，覆盖客户端参数，如 {"temperature": 0.2}'}>
            <textarea
              className="textarea"
              rows={8}
              value={overridesJson}
              onChange={(e) => setOverridesJson(e.target.value)}
            />
          </Field>
          <div className="form-actions">
            <Button variant="ghost" onClick={() => setEditingOverrides(null)}>取消</Button>
            <Button variant="primary" onClick={saveOverrides}>保存</Button>
          </div>
        </Modal>
      )}
    </>
  );
}
