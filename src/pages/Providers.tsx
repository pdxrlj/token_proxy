import { useState } from "react";
import type { AppData } from "../hooks/useAppData";
import type { ProviderConfig, UpstreamModel } from "../lib/types";
import { CATEGORY_META } from "../lib/types";
import { api } from "../lib/tauri";
import { Button, Field, Modal, StatusDot, Switch, useToast } from "../components/ui";
import {
  CloudDownload,
  KeyRound,
  Pencil,
  Plus,
  RefreshCw,
  Server,
  Trash2,
  X,
} from "lucide-react";

function emptyProvider(): ProviderConfig {
  return {
    id: "",
    name: "",
    base_url: "https://api.openai.com/v1",
    api_key: "",
    timeout_secs: 300,
    enabled: true,
    models: [],
    model_categories: {},
    user_agent: "",
    headers: {},
  };
}

/** 候选区的类别筛选 tab */
const CATEGORY_TABS = ["all", "text", "multimodal", "image"] as const;
type CategoryFilter = (typeof CATEGORY_TABS)[number];

/** 上下文窗口缩写：1048576 → 1M，262144 → 256k */
function fmtContext(n?: number | null): string | null {
  if (!n || n <= 0) return null;
  if (n >= 1024 * 1024) {
    const m = n / (1024 * 1024);
    return `${Number.isInteger(m) ? m : m.toFixed(1)}M`;
  }
  if (n >= 1024) return `${Math.round(n / 1024)}k`;
  return String(n);
}

export function Providers({ data }: { data: AppData }) {
  const { config, save, breakers, resetBreaker } = data;
  const toast = useToast();
  const [editing, setEditing] = useState<ProviderConfig | null>(null);
  const [isNew, setIsNew] = useState(false);
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
  const [headersText, setHeadersText] = useState("");
  const [showKey, setShowKey] = useState<Record<string, boolean>>({});
  const [fetchedModels, setFetchedModels] = useState<UpstreamModel[] | null>(null);
  const [fetching, setFetching] = useState(false);
  const [catFilter, setCatFilter] = useState<CategoryFilter>("all");
  const [searchText, setSearchText] = useState("");
  const [manualInput, setManualInput] = useState("");

  const openNew = () => {
    setEditing(emptyProvider());
    setSelectedModels([]);
    setHeadersText("");
    resetFetchState();
    setIsNew(true);
  };

  const openEdit = (p: ProviderConfig) => {
    setEditing({ ...p });
    setSelectedModels([...p.models]);
    setHeadersText(
      p.headers && Object.keys(p.headers).length > 0
        ? JSON.stringify(p.headers, null, 2)
        : "",
    );
    resetFetchState();
    setIsNew(false);
  };

  const resetFetchState = () => {
    setFetchedModels(null);
    setCatFilter("all");
    setSearchText("");
    setManualInput("");
  };

  /** 某模型已知的类别：本次拉取结果优先，其次配置里缓存的类别（手动添加的模型两者皆无） */
  const categoryOf = (id: string): string =>
    fetchedModels?.find((m) => m.id === id)?.category ??
    editing?.model_categories?.[id] ??
    "";

  /** 已选 / 取消一个模型：selectedModels 是唯一事实来源 */
  const toggleModel = (id: string) => {
    setSelectedModels((cur) =>
      cur.includes(id) ? cur.filter((x) => x !== id) : [...cur, id],
    );
  };

  /** 手动输入回车：添加为标签（无类别信息） */
  const addManualModel = () => {
    const id = manualInput.trim();
    if (!id) return;
    if (!selectedModels.includes(id)) {
      toggleModel(id);
    }
    setManualInput("");
  };

  /** 候选区当前可见（未选中且通过类别/搜索筛选）的模型 */
  const visibleModels = (fetchedModels ?? []).filter(
    (m) =>
      !selectedModels.includes(m.id) &&
      (catFilter === "all" || m.category === catFilter) &&
      (!searchText.trim() ||
        m.id.toLowerCase().includes(searchText.trim().toLowerCase())),
  );

  /** 全选当前可见的候选模型 */
  const selectVisible = () => {
    setSelectedModels((cur) => [
      ...cur,
      ...visibleModels.map((m) => m.id).filter((id) => !cur.includes(id)),
    ]);
  };

  /** 解析自定义请求头输入框；不合法时 toast 并返回 null */
  const parseHeaders = (): Record<string, string> | null => {
    const raw = headersText.trim();
    if (!raw) return {};
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      toast("自定义请求头不是合法的 JSON", "err");
      return null;
    }
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      toast("自定义请求头需写成 JSON 对象", "err");
      return null;
    }
    return Object.fromEntries(
      Object.entries(parsed as Record<string, unknown>).map(([k, v]) => [k, String(v)]),
    );
  };

  /** 从上游 GET /models 拉取可用模型列表；已选模型保留，候选区出现新模型供点选 */
  const fetchModels = async () => {
    if (!editing || fetching) return;
    if (!editing.base_url.trim()) {
      toast("请先填写 Base URL", "err");
      return;
    }
    const headers = parseHeaders();
    if (headers === null) return;

    setFetching(true);
    try {
      const models = await api.fetchProviderModels({
        base_url: editing.base_url,
        api_key: editing.api_key,
        user_agent: editing.user_agent || undefined,
        headers,
      });
      setFetchedModels(models);
      if (models.length === 0) {
        toast("上游返回了空的模型列表", "err");
      } else {
        toast(`已获取 ${models.length} 个模型，点击标签按需添加`);
      }
    } catch (e) {
      toast(String(e), "err");
    } finally {
      setFetching(false);
    }
  };

  const saveProvider = async () => {
    if (!editing) return;
    const models = selectedModels.map((s) => s.trim()).filter(Boolean);

    // 类别缓存：本次拉取到的优先，沿用配置里已有的；只保留仍选中的模型
    const model_categories: Record<string, string> = {};
    for (const id of models) {
      const c = categoryOf(id);
      if (c) model_categories[id] = c;
    }

    const headers = parseHeaders();
    if (headers === null) return;

    const provider: ProviderConfig = { ...editing, models, headers, model_categories };

    if (isNew) {
      if (!provider.id) {
        toast("请填写 Provider id", "err");
        return;
      }
      if (config.providers.some((p) => p.id === provider.id)) {
        toast(`Provider id「${provider.id}」已存在`, "err");
        return;
      }
      await save({ ...config, providers: [...config.providers, provider] });
      toast(`已添加 Provider「${provider.name}」`);
    } else {
      await save({
        ...config,
        providers: config.providers.map((p) => (p.id === provider.id ? provider : p)),
      });
      toast("已保存");
    }
    setEditing(null);
  };

  const removeProvider = async (id: string) => {
    if (!confirm(`删除 Provider「${id}」？关联的路由会失效。`)) return;
    await save({
      ...config,
      providers: config.providers.filter((p) => p.id !== id),
      routes: config.routes.filter((r) => r.provider !== id),
    });
    toast("已删除");
  };

  const toggleEnabled = async (p: ProviderConfig) => {
    await save({
      ...config,
      providers: config.providers.map((x) =>
        x.id === p.id ? { ...x, enabled: !x.enabled } : x,
      ),
    });
  };

  return (
    <>
      <div className="card" style={{ padding: "14px 20px", display: "flex", justifyContent: "space-between", alignItems: "center" }}>
        <span style={{ color: "var(--text-2)", fontSize: 13 }}>
          已配置 {config.providers.length} 个上游服务商 ·
          <span style={{ color: "var(--text-3)" }}>
            {" "}API Key 保存在本地配置文件，不会上传
          </span>
        </span>
        <Button variant="primary" onClick={openNew}>
          <Plus size={15} /> 添加 Provider
        </Button>
      </div>

      {config.providers.length === 0 ? (
        <div className="card">
          <div className="empty">
            <Server size={30} />
            <div>还没有上游 Provider。点击「添加 Provider」配置你的第一个模型服务商。</div>
          </div>
        </div>
      ) : (
        <div className="provider-list">
          {config.providers.map((p) => {
            const b = breakers[p.id];
            return (
              <div className="provider-card" key={p.id}>
                <div className="provider-avatar">{p.name.slice(0, 1).toUpperCase()}</div>
                <div className="provider-info">
                  <div className="name">
                    {p.name}
                    {!p.enabled && <span className="chip">已停用</span>}
                    {b?.state === "open" && (
                      <span className="chip" style={{ color: "var(--rose)", borderColor: "rgba(251,113,133,.4)" }}>
                        熔断中
                      </span>
                    )}
                    {b?.state === "half_open" && (
                      <span className="chip" style={{ color: "var(--amber)", borderColor: "rgba(245,158,11,.4)" }}>
                        试探中
                      </span>
                    )}
                  </div>
                  <div className="url">
                    {p.base_url}
                    {p.api_key ? (
                      <span style={{ marginLeft: 10, display: "inline-flex", alignItems: "center", gap: 4 }}>
                        <KeyRound size={11} />
                        <button
                          className="btn ghost sm"
                          style={{ padding: "0 4px", fontSize: 11 }}
                          onClick={() => setShowKey((m) => ({ ...m, [p.id]: !m[p.id] }))}
                        >
                          {showKey[p.id] ? p.api_key : "••••••••"}
                        </button>
                      </span>
                    ) : null}
                  </div>
                  {p.models.length > 0 && (
                    <div className="provider-models">
                      {p.models.map((m) => {
                        const cat = p.model_categories?.[m];
                        const meta = cat ? CATEGORY_META[cat] : undefined;
                        return (
                          <span className="chip" key={m}>
                            {meta && (
                              <span className={`cat-dot ${cat}`} title={meta.label} />
                            )}
                            {m}
                          </span>
                        );
                      })}
                    </div>
                  )}
                </div>
                <div className="provider-actions">
                  <div style={{ display: "flex", gap: 6, alignItems: "center" }}>
                    <span style={{ fontSize: 11.5, color: "var(--text-3)" }}>启用</span>
                    <Switch checked={p.enabled} onChange={() => toggleEnabled(p)} />
                  </div>
                  <div style={{ display: "flex", gap: 6 }}>
                    <Button size="sm" onClick={() => openEdit(p)}>
                      <Pencil size={13} /> 编辑
                    </Button>
                    {b?.state !== "closed" && (
                      <Button size="sm" onClick={() => resetBreaker(p.id)} title="重置熔断状态">
                        <RefreshCw size={13} />
                      </Button>
                    )}
                    <Button size="sm" variant="danger" onClick={() => removeProvider(p.id)}>
                      <Trash2 size={13} />
                    </Button>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      )}

      {editing && (
        <Modal title={isNew ? "添加 Provider" : `编辑 Provider · ${editing.id}`} onClose={() => setEditing(null)}>
          <div className="form-row">
            <Field label="Provider ID" help="唯一标识，路由引用用">
              <input
                className="input mono"
                value={editing.id}
                disabled={!isNew}
                placeholder="openai"
                onChange={(e) => setEditing({ ...editing, id: e.target.value.trim() })}
              />
            </Field>
            <Field label="显示名称">
              <input
                className="input"
                value={editing.name}
                placeholder="OpenAI"
                onChange={(e) => setEditing({ ...editing, name: e.target.value })}
              />
            </Field>
          </div>
          <Field label="Base URL" help="服务商根地址（如 https://api.openai.com/v1）。不要带 /chat/completions 等端点路径">
            <input
              className="input mono"
              value={editing.base_url}
              placeholder="https://api.openai.com/v1"
              onChange={(e) => setEditing({ ...editing, base_url: e.target.value.trim() })}
            />
          </Field>
          <Field label="API Key" help="留空表示该上游无需鉴权（本地模型）">
            <input
              className="input mono"
              type="password"
              value={editing.api_key}
              placeholder="sk-..."
              onChange={(e) => setEditing({ ...editing, api_key: e.target.value.trim() })}
            />
          </Field>
          <div className="form-row">
            <Field label="超时（秒）">
              <input
                className="input mono"
                type="number"
                min={30}
                value={editing.timeout_secs}
                onChange={(e) =>
                  setEditing({ ...editing, timeout_secs: Number(e.target.value) || 300 })
                }
              />
            </Field>
            <Field label="启用">
              <div style={{ display: "flex", alignItems: "center", height: 36 }}>
                <Switch checked={editing.enabled} onChange={(v) => setEditing({ ...editing, enabled: v })} />
              </div>
            </Field>
          </div>
          <Field
            label="User-Agent"
            help="发往该上游的 UA。留空表示跟随客户端；客户端也没带时，使用官方 SDK 形态的默认值"
          >
            <input
              className="input mono"
              value={editing.user_agent ?? ""}
              placeholder="跟随客户端"
              onChange={(e) => setEditing({ ...editing, user_agent: e.target.value })}
            />
          </Field>
          <Field
            label="自定义请求头"
            help='JSON 对象，如 {"HTTP-Referer": "https://my.app"}。优先级最高，可覆盖客户端透传过来的同名头'
          >
            <textarea
              className="textarea"
              rows={3}
              value={headersText}
              placeholder={'{\n  "HTTP-Referer": "https://my.app"\n}'}
              onChange={(e) => setHeadersText(e.target.value)}
            />
          </Field>
          <Field
            label="可用模型"
            help="点击「获取模型列表」从上游拉取，再点击下方候选标签按需添加；也可手动输入模型名后回车"
          >
            <div className="model-fetch-bar">
              <Button size="sm" onClick={fetchModels} disabled={fetching}>
                <CloudDownload size={13} className={fetching ? "spin" : undefined} />
                {fetching ? "获取中…" : "获取模型列表"}
              </Button>
              {selectedModels.length > 0 && (
                <span className="model-fetch-meta">
                  已选 {selectedModels.length} 个模型
                </span>
              )}
            </div>
            {/* 已选区：要保存的模型标签 */}
            <div className="model-tags">
              {selectedModels.map((id) => {
                const cat = CATEGORY_META[categoryOf(id)];
                return (
                  <span key={id} className={`model-tag${cat ? "" : " no-cat"}`}>
                    <span className="model-tag-id">{id}</span>
                    {cat && (
                      <span className={`modality-chip ${cat.cls}`}>{cat.label}</span>
                    )}
                    <button
                      type="button"
                      className="model-tag-x"
                      aria-label={`移除 ${id}`}
                      onClick={() => toggleModel(id)}
                    >
                      <X size={11} />
                    </button>
                  </span>
                );
              })}
              <input
                className="model-add-input"
                value={manualInput}
                placeholder={selectedModels.length === 0 ? "手动添加模型名，回车确认" : "+ 添加"}
                onChange={(e) => setManualInput(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    addManualModel();
                  }
                }}
              />
            </div>
            {/* 候选区：上游拉取到、尚未选用的模型 */}
            {fetchedModels && fetchedModels.length > 0 && (
              <div className="model-palette">
                <div className="model-palette-bar">
                  <div className="cat-tabs">
                    {CATEGORY_TABS.map((c) => (
                      <button
                        key={c}
                        type="button"
                        className={`cat-tab${catFilter === c ? " on" : ""}`}
                        onClick={() => setCatFilter(c)}
                      >
                        {c === "all" ? "全部" : CATEGORY_META[c].label}
                      </button>
                    ))}
                  </div>
                  <input
                    className="input model-search"
                    value={searchText}
                    placeholder="搜索模型…"
                    onChange={(e) => setSearchText(e.target.value)}
                  />
                  <button className="link-btn" onClick={selectVisible}>
                    全选
                  </button>
                  <button className="link-btn" onClick={() => setSelectedModels([])}>
                    清空
                  </button>
                </div>
                <div className="model-chip-cloud">
                  {visibleModels.map((m) => {
                    const cat = CATEGORY_META[m.category];
                    const ctx = fmtContext(m.context_length);
                    return (
                      <button
                        key={m.id}
                        type="button"
                        className="model-chip"
                        onClick={() => toggleModel(m.id)}
                      >
                        <span className="model-chip-id">{m.id}</span>
                        {cat && (
                          <span className={`modality-chip ${cat.cls}`}>{cat.label}</span>
                        )}
                        {ctx && <span className="model-chip-ctx">{ctx}</span>}
                        <Plus size={10} className="model-chip-add" />
                      </button>
                    );
                  })}
                  {visibleModels.length === 0 && (
                    <span className="model-palette-empty">没有匹配的模型</span>
                  )}
                </div>
              </div>
            )}
          </Field>
          <div className="form-actions">
            <Button variant="ghost" onClick={() => setEditing(null)}>取消</Button>
            <Button variant="primary" onClick={saveProvider}>
              <StatusDot color="cyan" /> {isNew ? "添加" : "保存"}
            </Button>
          </div>
        </Modal>
      )}
    </>
  );
}
