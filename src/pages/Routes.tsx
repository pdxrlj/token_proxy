import { useMemo, useState } from "react";
import type { AppData } from "../hooks/useAppData";
import type { LbStrategy, RouteConfig } from "../lib/types";
import { CATEGORY_META } from "../lib/types";
import { Button, Field, Modal, Switch, useToast } from "../components/ui";
import { Plus, Check, SlidersHorizontal, Trash2, Waypoints, Zap } from "lucide-react";

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

/** 路由去重键：与后端 route_key（model|provider|upstream_model）同口径 */
const routeKey = (r: { model: string; provider: string; upstream_model?: string }) =>
  `${r.model}|${r.provider}|${r.upstream_model ?? ""}`;

/** 快速添加弹窗状态：选中渠道 + 勾选的模型集合 */
interface QuickAdd {
  provider: string;
  selected: Set<string>;
}

export function Routes({ data }: { data: AppData }) {
  const { config, save } = data;
  const toast = useToast();
  const [newRoute, setNewRoute] = useState<RouteConfig>(() =>
    emptyRoute(config.providers[0]?.id ?? ""),
  );
  const [adding, setAdding] = useState(false);
  /** 添加路由弹窗里选中的 Provider（可多选，多选即建多条同名路由） */
  const [selectedProviders, setSelectedProviders] = useState<string[]>([]);
  const [overridesJson, setOverridesJson] = useState("{}");
  const [editingOverrides, setEditingOverrides] = useState<RouteConfig | null>(null);
  const [editingOverridesIdx, setEditingOverridesIdx] = useState<number>(-1);
  // 行内编辑上游模型名的本地草稿（key=原始索引），失焦时提交，避免每次按键写文件
  const [draftUpstream, setDraftUpstream] = useState<Record<string, string>>({});
  /** 快速添加弹窗（把某渠道的可用模型批量建为路由） */
  const [quick, setQuick] = useState<QuickAdd | null>(null);

  const enabledProviders = config.providers.filter((p) => p.enabled);
  const lbEnabled = config.load_balance?.enabled ?? false;
  const lbStrategy = config.load_balance?.strategy ?? "round_robin";

  /** 已选中的 Provider 列表 */
  const selectedProviderObjs = enabledProviders.filter((p) =>
    selectedProviders.includes(p.id),
  );

  /**
   * 上游模型名候选：合并所有已选 Provider 的模型，按名称去重
   * （同名只展示一个），count = 提供该模型的已选渠道数（>1 可做多渠道负载均衡）。
   */
  const upstreamCandidates = (() => {
    const counts = new Map<string, number>();
    for (const p of selectedProviderObjs) {
      for (const m of p.models) {
        counts.set(m, (counts.get(m) ?? 0) + 1);
      }
    }
    return [...counts.entries()].map(([id, count]) => ({ id, count }));
  })();

  /** 点选上游模型名；模型名为空时顺带补上（通常客户端请求名与转发名一致） */
  const pickUpstreamModel = (id: string) => {
    setNewRoute((nr) => ({
      ...nr,
      upstream_model: nr.upstream_model === id ? "" : id,
      model: nr.model.trim() ? nr.model : id,
    }));
  };

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

  /** 打开添加弹窗（可预填模型名） */
  const openAdd = (model = "") => {
    setNewRoute({ ...emptyRoute(config.providers[0]?.id ?? ""), model });
    setSelectedProviders(
      config.providers[0] ? [config.providers[0].id] : [],
    );
    setAdding(true);
  };

  const toggleNewProvider = (id: string) => {
    setSelectedProviders((cur) =>
      cur.includes(id) ? cur.filter((x) => x !== id) : [...cur, id],
    );
  };

  const addRoute = async () => {
    const model = newRoute.model.trim();
    if (!model) {
      toast("请填写模型名", "err");
      return;
    }
    if (selectedProviders.length === 0) {
      toast("请至少选择一个上游 Provider", "err");
      return;
    }
    const upstream = (newRoute.upstream_model ?? "").trim();
    const existing = new Set(config.routes.map(routeKey));
    const fresh = selectedProviders.filter(
      (pid) => !existing.has(`${model}|${pid}|${upstream}`),
    );
    if (fresh.length === 0) {
      toast("所选 Provider 的同名路由均已存在", "err");
      return;
    }
    const routes = fresh.map((pid) => ({
      ...emptyRoute(pid),
      model,
      upstream_model: upstream,
    }));
    await save({ ...config, routes: [...config.routes, ...routes] });
    const skipped = selectedProviders.length - fresh.length;
    setAdding(false);
    setNewRoute(emptyRoute(config.providers[0]?.id ?? ""));
    toast(
      `已添加 ${fresh.length} 条路由${skipped > 0 ? `，跳过 ${skipped} 条已存在` : ""}${
        fresh.length > 1 ? "（负载均衡）" : ""
      }`,
    );
  };

  /** 快速添加：把渠道可用模型批量建为路由（模型名不改写，已存在的跳过） */
  const quickProvider = config.providers.find((p) => p.id === quick?.provider);

  const openQuick = () => {
    const p = enabledProviders[0];
    setQuick(p ? { provider: p.id, selected: new Set(p.models) } : null);
  };

  const toggleQuickModel = (m: string) => {
    setQuick((q) => {
      if (!q) return q;
      const next = new Set(q.selected);
      if (next.has(m)) {
        next.delete(m);
      } else {
        next.add(m);
      }
      return { ...q, selected: next };
    });
  };

  const addQuickRoutes = async () => {
    if (!quick || !quickProvider) return;
    const existing = new Set(config.routes.map(routeKey));
    const fresh = quickProvider.models.filter(
      (m) => quick.selected.has(m) && !existing.has(`${m}|${quickProvider.id}|`),
    );
    if (fresh.length === 0) {
      toast("所选模型均已存在路由", "err");
      return;
    }
    const routes = fresh.map((m) => ({
      ...emptyRoute(quickProvider.id),
      model: m,
    }));
    await save({ ...config, routes: [...config.routes, ...routes] });
    setQuick(null);
    toast(`已为「${quickProvider.name}」添加 ${fresh.length} 条路由`);
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
        <div style={{ display: "flex", gap: 8 }}>
          <Button onClick={openQuick} disabled={enabledProviders.length === 0} title="把某个渠道的可用模型一次性批量添加为路由">
            <Zap size={14} /> 快速添加
          </Button>
          <Button variant="primary" onClick={() => openAdd()} disabled={enabledProviders.length === 0}>
            <Plus size={15} /> 添加路由
          </Button>
        </div>
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
                    onClick={() => openAdd(group.model)}
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
          <Field
            label="上游模型名"
            help="转发到上游时使用的模型名；留空则沿用客户端请求的 model。可从已选 Provider 的模型列表中点选，也可手动输入"
          >
            <input
              className="input mono"
              value={newRoute.upstream_model ?? ""}
              placeholder="z-ai/glm-5.2:free"
              onChange={(e) => setNewRoute({ ...newRoute, upstream_model: e.target.value })}
            />
            {selectedProviders.length > 0 ? (
              <div className="model-chip-cloud upstream-chips">
                <button
                  type="button"
                  className={`model-chip${!newRoute.upstream_model ? " on" : ""}`}
                  onClick={() => setNewRoute({ ...newRoute, upstream_model: "" })}
                  title="不改写：沿用客户端请求的 model"
                >
                  跟随请求模型
                </button>
                {upstreamCandidates.map(({ id, count }) => (
                  <button
                    key={id}
                    type="button"
                    className={`model-chip${newRoute.upstream_model === id ? " on" : ""}`}
                    onClick={() => pickUpstreamModel(id)}
                  >
                    {id}
                    {count > 1 && (
                      <span className="model-chip-ctx" title={`${count} 个已选渠道都提供该模型`}>
                        ×{count} 渠道
                      </span>
                    )}
                  </button>
                ))}
              </div>
            ) : (
              <span className="help">先在下方选择 Provider，即可从其模型列表中点选</span>
            )}
          </Field>
          <Field
            label="转发到 Provider"
            help="可多选：选中多个时，为每个 Provider 各创建一条同名路由，组成负载均衡候选；已存在的相同路由会自动跳过"
          >
            <div className="model-fetch-bar">
              <span className="model-fetch-meta">
                已选 {selectedProviders.length} / {enabledProviders.length}
              </span>
              <span className="spacer" />
              <button
                className="link-btn"
                onClick={() => setSelectedProviders(enabledProviders.map((p) => p.id))}
              >
                全选
              </button>
              <button className="link-btn" onClick={() => setSelectedProviders([])}>
                清空
              </button>
            </div>
            {enabledProviders.length === 0 ? (
              <span className="help">（无可用 Provider）</span>
            ) : (
              <div className="model-chip-cloud provider-chips">
                {enabledProviders.map((p) => {
                  const on = selectedProviders.includes(p.id);
                  return (
                    <button
                      key={p.id}
                      type="button"
                      className={`model-chip${on ? " on" : ""}`}
                      onClick={() => toggleNewProvider(p.id)}
                    >
                      {on && <Check size={11} className="chip-check" />}
                      {p.name} ({p.id})
                    </button>
                  );
                })}
              </div>
            )}
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

      {quick && quickProvider && (
        <Modal title="快速添加路由" onClose={() => setQuick(null)}>
          <Field
            label="选择 Provider"
            help="把该渠道「可用模型」批量添加为路由：路由模型名 = 渠道模型名，不改写转发名；已存在的相同路由自动跳过"
          >
            <select
              className="select"
              value={quick.provider}
              onChange={(e) => {
                const p = config.providers.find((x) => x.id === e.target.value);
                setQuick(p ? { provider: p.id, selected: new Set(p.models) } : null);
              }}
            >
              {enabledProviders.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name} ({p.id}) · {p.models.length} 个模型
                </option>
              ))}
            </select>
          </Field>

          {quickProvider.models.length === 0 ? (
            <div className="help" style={{ padding: "4px 0" }}>
              该渠道还没有模型。先到「上游 Provider」页编辑该渠道，点击「获取模型列表」后再来快速添加。
            </div>
          ) : (
            <>
              <div className="model-fetch-bar">
                <span className="model-fetch-meta">
                  共 {quickProvider.models.length} 个模型 · 已勾选 {quick.selected.size} 个
                </span>
                <span className="spacer" />
                <button
                  className="link-btn"
                  onClick={() =>
                    setQuick({ ...quick, selected: new Set(quickProvider.models) })
                  }
                >
                  全选
                </button>
                <button
                  className="link-btn"
                  onClick={() => setQuick({ ...quick, selected: new Set() })}
                >
                  清空
                </button>
              </div>
              <div className="model-chip-cloud quick-chips">
                {quickProvider.models.map((m) => {
                  const cat = quickProvider.model_categories?.[m];
                  const meta = cat ? CATEGORY_META[cat] : undefined;
                  return (
                    <button
                      key={m}
                      type="button"
                      className={`model-chip${quick.selected.has(m) ? " on" : ""}`}
                      onClick={() => toggleQuickModel(m)}
                    >
                      {m}
                      {meta && (
                        <span className={`modality-chip ${meta.cls}`}>{meta.label}</span>
                      )}
                    </button>
                  );
                })}
              </div>
            </>
          )}

          <div className="form-actions">
            <Button variant="ghost" onClick={() => setQuick(null)}>取消</Button>
            <Button variant="primary" onClick={addQuickRoutes}>添加</Button>
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
