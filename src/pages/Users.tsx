import { useMemo, useState } from "react";
import type { AppData } from "../hooks/useAppData";
import type { UserConfig } from "../lib/types";
import {
  Button,
  Field,
  Modal,
  Switch,
  fmtCount,
  useToast,
} from "../components/ui";
import {
  Copy,
  Eye,
  EyeOff,
  Plus,
  RefreshCw,
  ShieldAlert,
  Trash2,
  Users as UsersIcon,
} from "lucide-react";

/** 生成一个易读且不易撞车的 Key */
function generateKey(): string {
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  const body = Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  return `sk-tp-${body}`;
}

function emptyUser(): UserConfig {
  return {
    id: "",
    name: "",
    key: generateKey(),
    enabled: true,
    max_requests: null,
  };
}

export function Users({ data }: { data: AppData }) {
  const { config, userUsage, save, resetUserUsage } = data;
  const toast = useToast();
  const [adding, setAdding] = useState(false);
  const [draft, setDraft] = useState<UserConfig>(emptyUser());
  const [revealed, setRevealed] = useState<Record<string, boolean>>({});
  const [copiedId, setCopiedId] = useState<string | null>(null);

  const users = useMemo(() => config.users ?? [], [config.users]);

  const toggleReveal = (id: string) =>
    setRevealed((r) => ({ ...r, [id]: !r[id] }));

  const copyKey = async (u: UserConfig) => {
    try {
      await navigator.clipboard.writeText(u.key);
      setCopiedId(u.id);
      setTimeout(() => setCopiedId(null), 1600);
    } catch {
      toast("复制失败，请手动复制", "err");
    }
  };

  const addUser = async () => {
    const id = draft.id.trim();
    if (!id) {
      toast("请填写用户标识", "err");
      return;
    }
    if (users.some((u) => u.id === id)) {
      toast(`用户标识「${id}」已存在`, "err");
      return;
    }
    if (!draft.key.trim()) {
      toast("API Key 不能为空", "err");
      return;
    }
    if (users.some((u) => u.key === draft.key.trim())) {
      toast("该 API Key 与其它用户重复", "err");
      return;
    }
    await save({
      ...config,
      users: [...users, { ...draft, id, key: draft.key.trim() }],
    });
    setAdding(false);
    setDraft(emptyUser());
    toast(`已添加用户「${id}」`);
  };

  const updateUser = async (id: string, patch: Partial<UserConfig>) => {
    await save({
      ...config,
      users: users.map((u) => (u.id === id ? { ...u, ...patch } : u)),
    });
  };

  const removeUser = async (id: string) => {
    await save({ ...config, users: users.filter((u) => u.id !== id) });
    // 连带清掉计数，避免残留占位
    await resetUserUsage(id);
    toast("已删除用户");
  };

  const toggleStrict = async (v: boolean) => {
    await save({ ...config, server: { ...config.server, require_api_key: v } });
    toast(
      v
        ? "已开启：请求必须携带已登记用户的 API Key"
        : "已关闭：未携带或未登记的 Key 将按匿名请求放行",
    );
  };

  const strict = config.server.require_api_key ?? false;

  return (
    <>
      {/* 严格模式开关 */}
      <div className="card">
        <div className="card-header">
          <span className="card-title" style={{ display: "flex", alignItems: "center", gap: 7 }}>
            <ShieldAlert size={14} /> 强制校验
          </span>
          <Switch checked={strict} onChange={toggleStrict} />
        </div>
        <p style={{ color: "var(--text-3)", fontSize: 12.5, marginBottom: 10, lineHeight: 1.7 }}>
          默认关闭时，未携带 Key 或携带了未登记 Key 的请求会当作匿名请求放行——这是为了兼容
          <b> 客户端总是自带 Authorization 头 </b>
          的现实情况（多数 OpenAI SDK 即使连本地代理也会带上自己的 key）。
        </p>
        {!strict && users.some((u) => u.max_requests != null) ? (
          <div
            style={{
              padding: "10px 12px",
              background: "var(--bg-2)",
              borderRadius: 8,
              fontSize: 12,
              color: "var(--amber)",
              border: "1px solid var(--border)",
              lineHeight: 1.7,
            }}
          >
            <b>注意：当前有用户设置了配额上限，但强制校验是关闭的。</b>
            <br />
            此时客户端只要改发一个随机 Key 就会被当作匿名请求放行，配额可被绕过。
            若需要配额真正生效，请开启上方的「强制校验」。
          </div>
        ) : null}
      </div>

      {/* 用户列表 */}
      <div
        className="card"
        style={{ padding: "14px 20px", display: "flex", justifyContent: "space-between", alignItems: "center" }}
      >
        <span style={{ color: "var(--text-2)", fontSize: 13 }}>
          共 {users.length} 个用户 ·{" "}
          <span style={{ color: "var(--text-3)" }}>
            客户端在 Authorization: Bearer 中携带该用户的 Key 即可（可选）
          </span>
        </span>
        <div style={{ display: "flex", gap: 8 }}>
          <Button
            variant="ghost"
            onClick={async () => {
              await resetUserUsage(null);
              toast("已重置全部用户的已用次数");
            }}
            disabled={users.length === 0}
          >
            <RefreshCw size={14} /> 重置全部计数
          </Button>
          <Button variant="primary" onClick={() => { setDraft(emptyUser()); setAdding(true); }}>
            <Plus size={15} /> 添加用户
          </Button>
        </div>
      </div>

      {users.length === 0 ? (
        <div className="card">
          <div className="empty">
            <UsersIcon size={30} />
            <div>
              还没有用户。添加后可分发给不同使用者，并单独限制各自的请求次数。
              不添加也不影响使用——未携带 Key 的请求默认不限次数。
            </div>
          </div>
        </div>
      ) : (
        <div className="card">
          <table className="table">
            <thead>
              <tr>
                <th style={{ width: "15%" }}>用户标识</th>
                <th style={{ width: "15%" }}>备注名</th>
                <th>API Key</th>
                <th style={{ width: 150 }}>已用 / 上限</th>
                <th style={{ width: 60 }}>启用</th>
                <th style={{ width: 110 }}></th>
              </tr>
            </thead>
            <tbody>
              {users.map((u) => {
                const used = userUsage[u.id] ?? 0;
                const limit = u.max_requests ?? null;
                const exhausted = limit != null && used >= limit;
                const pct =
                  limit != null && limit > 0 ? Math.min(100, (used / limit) * 100) : 0;
                return (
                  <tr key={u.id} style={u.enabled ? undefined : { opacity: 0.5 }}>
                    <td>
                      <span className="mono" style={{ fontWeight: 600, color: "var(--text-1)" }}>
                        {u.id}
                      </span>
                    </td>
                    <td>
                      <input
                        className="input"
                        style={{ width: "100%" }}
                        value={u.name}
                        placeholder="（无）"
                        onChange={(e) => updateUser(u.id, { name: e.target.value })}
                      />
                    </td>
                    <td>
                      <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                        <span className="mono" style={{ flex: 1, fontSize: 12 }}>
                          {revealed[u.id] ? u.key : "•".repeat(Math.min(u.key.length, 32))}
                        </span>
                        <button
                          className="btn ghost sm"
                          onClick={() => toggleReveal(u.id)}
                          title={revealed[u.id] ? "隐藏" : "显示"}
                        >
                          {revealed[u.id] ? <EyeOff size={13} /> : <Eye size={13} />}
                        </button>
                        <button
                          className="btn ghost sm"
                          onClick={() => copyKey(u)}
                          title="复制 API Key"
                        >
                          <Copy size={13} />
                          {copiedId === u.id ? "已复制" : ""}
                        </button>
                      </div>
                    </td>
                    <td>
                      {limit == null ? (
                        <span style={{ color: "var(--text-3)" }}>
                          {fmtCount(used)} / 不限
                        </span>
                      ) : (
                        <div title={`${used} / ${limit}`}>
                          <span
                            style={{
                              color: exhausted
                                ? "var(--rose)"
                                : pct > 80
                                  ? "var(--amber)"
                                  : "var(--text-2)",
                              fontWeight: 600,
                            }}
                          >
                            {fmtCount(used)} / {fmtCount(limit)}
                          </span>
                          <div
                            style={{
                              marginTop: 4,
                              height: 4,
                              borderRadius: 2,
                              background: "var(--bg-3)",
                              overflow: "hidden",
                            }}
                          >
                            <div
                              style={{
                                width: `${pct}%`,
                                height: "100%",
                                background: exhausted ? "var(--rose)" : "var(--accent)",
                              }}
                            />
                          </div>
                        </div>
                      )}
                    </td>
                    <td style={{ textAlign: "center" }}>
                      <input
                        type="checkbox"
                        checked={u.enabled}
                        onChange={() => updateUser(u.id, { enabled: !u.enabled })}
                        title={u.enabled ? "已启用" : "已停用"}
                      />
                    </td>
                    <td style={{ textAlign: "right" }}>
                      <div style={{ display: "flex", gap: 4, justifyContent: "flex-end" }}>
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={async () => {
                            await resetUserUsage(u.id);
                            toast(`已重置用户「${u.id}」的已用次数`);
                          }}
                          title="重置已用次数"
                        >
                          <RefreshCw size={13} />
                        </Button>
                        <Button size="sm" variant="danger" onClick={() => removeUser(u.id)}>
                          <Trash2 size={13} />
                        </Button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
          <div style={{ marginTop: 10, fontSize: 11.5, color: "var(--text-3)" }}>
            已用次数在应用重启后清零；如需重新计数请手动重置。
          </div>
        </div>
      )}

      {adding && (
        <Modal title="添加用户" onClose={() => setAdding(false)}>
          <Field label="用户标识" help="唯一 ID，用于识别与配额计数，如 alice / team-a">
            <input
              className="input mono"
              value={draft.id}
              placeholder="alice"
              onChange={(e) => setDraft({ ...draft, id: e.target.value })}
              autoFocus
            />
          </Field>
          <Field label="备注名" help="便于识别归属，可留空">
            <input
              className="input"
              value={draft.name}
              placeholder="张三 / Claude Code"
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
          </Field>
          <Field
            label="API Key"
            help="已自动生成随机 Key，可自行修改；客户端以 Bearer 方式携带"
          >
            <div style={{ display: "flex", gap: 6 }}>
              <input
                className="input mono"
                style={{ flex: 1 }}
                value={draft.key}
                onChange={(e) => setDraft({ ...draft, key: e.target.value })}
              />
              <Button onClick={() => setDraft({ ...draft, key: generateKey() })} title="重新生成">
                <RefreshCw size={14} />
              </Button>
            </div>
          </Field>
          <Field
            label="最大请求数"
            help="留空表示不限制；填写后该用户达到上限将返回 429"
          >
            <input
              className="input mono"
              type="number"
              min={1}
              placeholder="不限"
              value={draft.max_requests ?? ""}
              onChange={(e) => {
                const raw = e.target.value.trim();
                const n = Number(raw);
                setDraft({
                  ...draft,
                  max_requests: raw === "" || !Number.isFinite(n) || n <= 0 ? null : Math.floor(n),
                });
              }}
            />
          </Field>
          <div className="form-actions">
            <Button variant="ghost" onClick={() => setAdding(false)}>
              取消
            </Button>
            <Button variant="primary" onClick={addUser}>
              添加
            </Button>
          </div>
        </Modal>
      )}
    </>
  );
}
