import { useMemo, useState } from "react";
import type { AppData } from "../hooks/useAppData";
import type { LbStrategy, RouteConfig } from "../lib/types";
import { Button, Field, Modal, Switch, useToast } from "../components/ui";
import { Plus, SlidersHorizontal, Trash2, Waypoints } from "lucide-react";

/** 负载均衡策略的中文名（仅在该模型有多条启用路由时展示） */
const STRATEGY_LABEL: Record<LbStrategy, string> = {
  round_robin: "轮询",
  fastest_response: "最快响应",
  weighted: "加权",
  random: "随机",
  least_connections: "最少连接",
};

/** 一个模型分组：携带原始索引，便于回写到 config.routes 的正确位置 */
interface RouteGroup {
  model: string;
  routes: { route: RouteConfig; idx: number }[];
}

function emptyRoute(defaultProvider: string): RouteConfig {
  return {
    model: "",
    provider: defaultProvider,
    upstream_model: "",
    overrides: {},
    weight: 1,
    disabled: false,
  };
}

export function Routes({ data }: { data: AppData }) {
  const { config, save } = data;
  const toast = useToast();
  const [newRoute, setNewRoute] = useState<RouteConfig>(() =>
    emptyRoute(config.providers[0]?.id ?? ""),
  );
  const [adding, setAdding] = useState(false);
  const [overridesJson, setOverridesJson] = useState("{}");
  const [editingOverrides, setEditingOverrides] = useState<RouteConfig | null>(null);
  const [editingOverridesIdx, setEditingOverridesIdx] = useState<number>(-1);
  // 行内编辑上游模型名的本地草稿（key=原始索引），失焦时提交，避免每次按键写文件
  const [draftUpstream, setDraftUpstream] = useState<Record<string, string>>({});

  const enabledProviders = config.providers.filter((p) => p.enabled);
  const lbEnabled = config.load_balance?.enabled ?? false;
  const lbStrategy = config.load_balance?.strategy ?? "round_robin";

  // 按模型名分组：保持首次出现顺序，组内保持配置中的原始顺序
  const groups = useMemo<RouteGroup[]>(() => {
    const map = new Map<string, { route: RouteConfig; idx: number }[]>();
    config.routes.forEach((route, idx) => {
      const list = map.get(route.model) ?? [];
      list.push({ route, idx });
      map.set(route.model, list);
    });
    return [...map.entries()].map(([model, routes]) => ({ model, routes }));
  }, [config.routes]);

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
    await save({ ...config, routes: [...config.routes, { ...newRoute, model }] });
    setAdding(false);
    setNewRoute(emptyRoute(config.providers[0]?.id ?? ""));
    toast(`已添加路由 ${model} → ${newRoute.provider}`);
  };

  /** 从某个模型分组的头部快速新增：预填模型名 */
  const addForModel = (model: string) => {
    setNewRoute({ ...emptyRoute(config.providers[0]?.id ?? ""), model });
    setAdding(true);
  };

  const changeProvider = async (idx: number, provider: string) => {
    await save({
      ...config,
      routes: config.routes.map((r, i) => (i === idx ? { ...r, provider } : r)),
    });
  };

  const changeUpstreamModel = async (idx: number, upstreamModel: string) => {
    const trimmed = upstreamModel.trim();
    const current = config.routes[idx]?.upstream_model ?? "";
    if (trimmed === current) return;
    await save({
      ...config,
      routes: config.routes.map((r, i) =>
        i === idx ? { ...r, upstream_model: trimmed } : r,
      ),
    });
  };

  const changeWeight = async (idx: number, weight: number) => {
    await save({
      ...config,
      routes: config.routes.map((r, i) =>
        i === idx ? { ...r, weight: Math.max(1, weight) } : r,
      ),
    });
  };

  const toggleDisabled = async (idx: number) => {
    await save({
      ...config,
      routes: config.routes.map((r, i) =>
        i === idx ? { ...r, disabled: !r.disabled } : r,
      ),
    });
  };

  const removeRoute = async (idx: number) => {
    await save({ ...config, routes: config.routes.filter((_, i) => i !== idx) });
    // 删除后后续路由的索引会整体前移，行内草稿若按索引缓存会错位写到下一行，
    // 这里直接清空草稿（删除是低频操作，不影响输入体验）
    setDraftUpstream({});
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
      routes: config.routes.map((r, i) =>
        i === editingOverridesIdx ? { ...r, overrides: parsed } : r,
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
          共 {config.routes.length} 条路由 · {groups.length} 个模型 ·{" "}
          <span style={{ color: "var(--text-3)" }}>
            同一模型多条路由可做负载均衡；未配置路由的模型按 Provider 的模型列表自动匹配
          </span>
        </span>
        <Button variant="primary" onClick={() => { setNewRoute(emptyRoute(config.providers[0]?.id ?? "")); setAdding(true); }} disabled={enabledProviders.length === 0}>
          <Plus size={15} /> 添加路由
        </Button>
      </div>

      {config.routes.length === 0 ? (
        <div className="card">
          <div className="empty">
            <Waypoints size={30} />
            <div>还没有路由规则。模型请求会按 Provider 列表自动匹配；添加路由可精确控制转发与参数改写。同名模型多条路由可启用负载均衡。</div>
          </div>
        </div>
      ) : (
        <div className="route-list">
          {groups.map((group) => {
            // 只有启用中的路由参与分发，权重占比也只在它们之间计算
            const active = group.routes.filter((x) => !x.route.disabled);
            const totalWeight = active.reduce((s, x) => s + (x.route.weight ?? 1), 0);
            const showPct = active.length > 1 && totalWeight > 0;

            return (
              <div className="route-group" key={group.model}>
                <div className="route-group-head">
                  <span className="model" title={group.model}>
                    {group.model}
                  </span>
                  <span className="chip">{group.routes.length} 条</span>
                  {active.length > 1 && lbEnabled && (
                    <span
                      className="chip"
                      style={{
                        color: "var(--accent-strong)",
                        borderColor: "var(--accent-line)",
                        background: "var(--accent-soft)",
                      }}
                      title="该模型有多条启用路由，请求会按此策略在它们之间分配"
                    >
                      {STRATEGY_LABEL[lbStrategy] ?? lbStrategy}
                    </span>
                  )}
                  {active.length === 0 && (
                    <span
                      className="chip"
                      style={{ color: "var(--rose)", borderColor: "var(--rose-line)" }}
                    >
                      全部停用
                    </span>
                  )}
                  <span className="spacer" />
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={() => addForModel(group.model)}
                    title={`为 ${group.model} 再添加一条路由`}
                  >
                    <Plus size={13} /> 添加
                  </Button>
                </div>

                <div className="route-row head">
                  <span>上游模型名</span>
                  <span>上游 Provider</span>
                  <span>权重</span>
                  <span>参数改写</span>
                  <span>启用</span>
                  <span />
                </div>

                {group.routes.map(({ route: r, idx }) => {
                  const pct =
                    showPct && !r.disabled && totalWeight > 0
                      ? Math.round(((r.weight ?? 1) / totalWeight) * 100)
                      : null;
                  return (
                    <div className={`route-row ${r.disabled ? "off" : ""}`} key={idx}>
                      <input
                        className="input mono"
                        value={draftUpstream[idx] ?? r.upstream_model ?? ""}
                        placeholder={group.model}
                        onChange={(e) =>
                          setDraftUpstream((d) => ({ ...d, [idx]: e.target.value }))
                        }
                        onBlur={(e) => {
                          changeUpstreamModel(idx, e.target.value);
                          setDraftUpstream((d) => {
                            const { [idx]: _, ...rest } = d;
                            return rest;
                          });
                        }}
                        title="转发到上游时使用的模型名；留空则沿用客户端请求的 model"
                      />

                      <select
                        className="select mono"
                        value={r.provider}
                        onChange={(e) => changeProvider(idx, e.target.value)}
                        title="选择转发到哪个上游"
                      >
                        {config.providers.map((p) => (
                          <option key={p.id} value={p.id} disabled={!p.enabled}>
                            {p.name} ({p.id}){p.enabled ? "" : " · 已停用"}
                          </option>
                        ))}
                      </select>

                      <div className="weight-cell">
                        <input
                          className="input mono"
                          type="number"
                          min={1}
                          value={r.weight ?? 1}
                          onChange={(e) => changeWeight(idx, parseInt(e.target.value) || 1)}
                          title="负载均衡权重（weighted 策略时生效）"
                        />
                        {pct != null && <span className="pct">{pct}%</span>}
                      </div>

                      <button
                        className="btn ghost sm"
                        onClick={() => {
                          setEditingOverrides(r);
                          setEditingOverridesIdx(idx);
                          setOverridesJson(JSON.stringify(r.overrides, null, 2));
                        }}
                        title="编辑参数改写（temperature 等）"
                        style={{ justifyContent: "flex-start", overflow: "hidden" }}
                      >
                        <SlidersHorizontal size={13} />
                        <span
                          style={{
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          {overridesSummary(r.overrides)}
                        </span>
                      </button>

                      <div className="switch-cell">
                        <Switch
                          checked={!r.disabled}
                          onChange={() => toggleDisabled(idx)}
                        />
                        <span>{r.disabled ? "停用" : "启用"}</span>
                      </div>

                      <div style={{ display: "flex", justifyContent: "flex-end" }}>
                        <Button size="sm" variant="danger" onClick={() => removeRoute(idx)} title="删除该路由">
                          <Trash2 size={13} />
                        </Button>
                      </div>
                    </div>
                  );
                })}
              </div>
            );
          })}
        </div>
      )}

      {adding && (
        <Modal title="添加路由" onClose={() => setAdding(false)}>
          <Field label="模型名" help="客户端请求体里的 model 字段；同名模型可添加多条路由实现负载均衡">
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
          <Field label="权重" help="负载均衡加权策略（weighted）时生效，默认 1">
            <input
              className="input mono"
              type="number"
              min={1}
              style={{ width: 100 }}
              value={newRoute.weight ?? 1}
              onChange={(e) => setNewRoute({ ...newRoute, weight: parseInt(e.target.value) || 1 })}
            />
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
