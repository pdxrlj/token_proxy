import { useState } from "react";
import type { AppData } from "../hooks/useAppData";
import type { ProviderConfig } from "../lib/types";
import { Button, Field, Modal, StatusDot, Switch, useToast } from "../components/ui";
import { KeyRound, Pencil, Plus, RefreshCw, Server, Trash2 } from "lucide-react";

function emptyProvider(): ProviderConfig {
  return {
    id: "",
    name: "",
    base_url: "https://api.openai.com/v1",
    api_key: "",
    timeout_secs: 300,
    enabled: true,
    models: [],
  };
}

export function Providers({ data }: { data: AppData }) {
  const { config, save, breakers, resetBreaker } = data;
  const toast = useToast();
  const [editing, setEditing] = useState<ProviderConfig | null>(null);
  const [isNew, setIsNew] = useState(false);
  const [modelsText, setModelsText] = useState("");
  const [showKey, setShowKey] = useState<Record<string, boolean>>({});

  const openNew = () => {
    setEditing(emptyProvider());
    setModelsText("");
    setIsNew(true);
  };

  const openEdit = (p: ProviderConfig) => {
    setEditing({ ...p });
    setModelsText(p.models.join("\n"));
    setIsNew(false);
  };

  const saveProvider = async () => {
    if (!editing) return;
    const models = modelsText
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    const provider: ProviderConfig = { ...editing, models };

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
                      {p.models.map((m) => (
                        <span className="chip" key={m}>{m}</span>
                      ))}
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
          <Field label="Base URL" help="OpenAI 兼容地址，含 /v1">
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
          <Field label="可用模型" help="每行一个模型名，回车分隔">
            <textarea
              className="textarea"
              rows={4}
              value={modelsText}
              placeholder={"gpt-4o\ngpt-4o-mini"}
              onChange={(e) => setModelsText(e.target.value)}
            />
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
