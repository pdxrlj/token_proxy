import { useState } from "react";
import type { AppData } from "../hooks/useAppData";
import { Button, Field, Switch, useToast } from "../components/ui";
import { FileCog, Flame, RotateCcw, Server } from "lucide-react";

const DEFAULT_STATUSES = [408, 429, 500, 502, 503, 529];
const PRESETS = {
  "常见(429/5xx)": [408, 429, 500, 502, 503, 529],
  保守: [429, 503],
  激进: [408, 429, 500, 502, 503, 504, 529, 520, 524],
};

export function Settings({ data }: { data: AppData }) {
  const { config, status, save } = data;
  const toast = useToast();
  const [draft, setDraft] = useState(() => JSON.parse(JSON.stringify(config)));
  const [dirty, setDirty] = useState(false);

  const set = (patch: unknown) => {
    setDraft((d: typeof config) => merge(d, patch));
    setDirty(true);
  };

  const toggleStatus = (code: number) => {
    const list = draft.retry.retry_statuses as number[];
    const next = list.includes(code) ? list.filter((c) => c !== code) : [...list, code].sort();
    set({ retry: { ...draft.retry, retry_statuses: next } });
  };

  const persist = async () => {
    try {
      await save(draft);
      setDirty(false);
      toast("设置已保存，代理立即生效");
    } catch (e) {
      toast(`保存失败：${e}`, "err");
    }
  };

  return (
    <>
      <div className="grid-2">
        {/* 服务配置 */}
        <div className="card">
          <div className="card-header">
            <span className="card-title" style={{ display: "flex", alignItems: "center", gap: 7 }}>
              <Server size={13} /> 服务配置
            </span>
          </div>
          <div className="section">
            <div className="form-row">
              <Field label="监听地址">
                <input
                  className="input mono"
                  value={draft.server.host}
                  onChange={(e) => set({ server: { ...draft.server, host: e.target.value.trim() } })}
                />
              </Field>
              <Field label="端口">
                <input
                  className="input mono"
                  type="number"
                  min={1}
                  max={65535}
                  value={draft.server.port}
                  onChange={(e) => set({ server: { ...draft.server, port: Number(e.target.value) || 8787 } })}
                />
              </Field>
            </div>
            <div style={{ height: 12 }} />
            <Field label="本地访问令牌" help="留空则本机免鉴权；设置后客户端需带 Authorization: Bearer <令牌>">
              <input
                className="input mono"
                type="password"
                value={draft.server.local_api_key}
                placeholder="留空"
                onChange={(e) => set({ server: { ...draft.server, local_api_key: e.target.value } })}
              />
            </Field>
            {status && (
              <div style={{ marginTop: 14, padding: "10px 12px", background: "var(--bg-2)", borderRadius: 10, fontSize: 12, color: "var(--text-3)", fontFamily: "var(--font-mono)" }}>
                配置文件：{status.config_path}
                <br />
                修改配置会热重载，无需重启应用
              </div>
            )}
          </div>
        </div>

        {/* 熔断 */}
        <div className="card">
          <div className="card-header">
            <span className="card-title" style={{ display: "flex", alignItems: "center", gap: 7 }}>
              <Flame size={13} /> 熔断器
            </span>
            <Switch
              checked={draft.retry.circuit_breaker.enabled}
              onChange={(v) => set({ retry: { ...draft.retry, circuit_breaker: { ...draft.retry.circuit_breaker, enabled: v } } })}
            />
          </div>
          <p style={{ color: "var(--text-3)", fontSize: 12.5, marginBottom: 10 }}>
            上游连续失败达到阈值后暂停请求一段时间，避免对故障上游反复轰炸；冷却后自动试探恢复。
          </p>
          <div className="form-row">
            <Field label="失败阈值（次）">
              <input
                className="input mono"
                type="number"
                min={1}
                value={draft.retry.circuit_breaker.failure_threshold}
                onChange={(e) =>
                  set({
                    retry: {
                      ...draft.retry,
                      circuit_breaker: { ...draft.retry.circuit_breaker, failure_threshold: Number(e.target.value) || 1 },
                    },
                  })
                }
              />
            </Field>
            <Field label="冷却时间（秒）">
              <input
                className="input mono"
                type="number"
                min={1}
                value={draft.retry.circuit_breaker.cooldown_secs}
                onChange={(e) =>
                  set({
                    retry: {
                      ...draft.retry,
                      circuit_breaker: { ...draft.retry.circuit_breaker, cooldown_secs: Number(e.target.value) || 1 },
                    },
                  })
                }
              />
            </Field>
          </div>
        </div>
      </div>

      {/* 重试策略 */}
      <div className="card">
        <div className="card-header">
          <span className="card-title" style={{ display: "flex", alignItems: "center", gap: 7 }}>
            <RotateCcw size={13} /> 重试策略
          </span>
          <div style={{ display: "flex", gap: 6 }}>
            {Object.entries(PRESETS).map(([label, codes]) => (
              <button
                key={label}
                className="chip toggle"
                onClick={() => set({ retry: { ...draft.retry, retry_statuses: codes } })}
              >
                {label}
              </button>
            ))}
          </div>
        </div>
        <div className="form-row">
          <Field label="最大重试次数">
            <input
              className="input mono"
              type="number"
              min={0}
              max={10}
              value={draft.retry.max_retries}
              onChange={(e) => set({ retry: { ...draft.retry, max_retries: Number(e.target.value) || 0 } })}
            />
          </Field>
          <Field label="退避基数（毫秒）" help="指数退避：基数 × 2ⁿ + 抖动">
            <input
              className="input mono"
              type="number"
              min={50}
              value={draft.retry.base_delay_ms}
              onChange={(e) => set({ retry: { ...draft.retry, base_delay_ms: Number(e.target.value) || 500 } })}
            />
          </Field>
          <Field label="最大退避（毫秒）">
            <input
              className="input mono"
              type="number"
              min={100}
              value={draft.retry.max_delay_ms}
              onChange={(e) => set({ retry: { ...draft.retry, max_delay_ms: Number(e.target.value) || 8000 } })}
            />
          </Field>
        </div>
        <div style={{ height: 14 }} />
        <div className="switch-row">
          <div className="text">
            <b>网络错误重试</b>
            <span>连接失败、超时、断流等网络层错误也参与重试</span>
          </div>
          <Switch
            checked={draft.retry.retry_on_network_error}
            onChange={(v) => set({ retry: { ...draft.retry, retry_on_network_error: v } })}
          />
        </div>
        <div style={{ height: 6 }} />
        <Field label="可重试的状态码" help="点击切换；4xx 业务错误（400/401/403/404）永不重试">
          <div style={{ display: "flex", flexWrap: "wrap", gap: 6 }}>
            {DEFAULT_STATUSES.map((c) => (
              <button
                key={c}
                className={`chip toggle ${draft.retry.retry_statuses.includes(c) ? "on" : ""}`}
                onClick={() => toggleStatus(c)}
              >
                {c}
              </button>
            ))}
          </div>
        </Field>
      </div>

      {/* 操作栏 */}
      <div className="card" style={{ display: "flex", alignItems: "center", gap: 12, padding: "14px 20px" }}>
        <FileCog size={16} style={{ color: "var(--text-3)" }} />
        <span style={{ color: "var(--text-3)", fontSize: 12.5, flex: 1 }}>
          所有修改即时写入配置文件，代理与界面无需重启。
        </span>
        <Button variant="primary" disabled={!dirty} onClick={persist}>
          保存设置
        </Button>
      </div>
    </>
  );
}

/** 浅合并（draft 结构固定，直接递归覆盖对象） */
function merge<T extends object>(base: T, patch: unknown): T {
  if (typeof patch !== "object" || patch === null || Array.isArray(patch)) {
    return patch as T;
  }
  const out: Record<string, unknown> = {
    ...(base as unknown as Record<string, unknown>),
  };
  for (const [k, v] of Object.entries(patch as Record<string, unknown>)) {
    const cur = (out[k] as Record<string, unknown> | undefined) ?? {};
    out[k] = merge(cur, v);
  }
  return out as T;
}
