import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import type { AppData } from "../hooks/useAppData";
import { StatusDot, fmtCount } from "./ui";
import {
  Activity,
  Cable,
  Database,
  LayoutDashboard,
  Settings,
  ScrollText,
  ServerCog,
  TriangleAlert,
  Users,
  Waypoints,
} from "lucide-react";

export type PageKey =
  | "overview"
  | "providers"
  | "routes"
  | "users"
  | "logs"
  | "dataset"
  | "settings";

const NAV: { key: PageKey; label: string; icon: ReactNode }[] = [
  { key: "overview", label: "总览", icon: <LayoutDashboard size={16} /> },
  { key: "providers", label: "模型", icon: <ServerCog size={16} /> },
  { key: "routes", label: "路由策略", icon: <Waypoints size={16} /> },
  { key: "users", label: "访问控制", icon: <Users size={16} /> },
  { key: "logs", label: "请求日志", icon: <ScrollText size={16} /> },
  { key: "dataset", label: "数据归档", icon: <Database size={16} /> },
  { key: "settings", label: "运行设置", icon: <Settings size={16} /> },
];

const PAGE_TITLES: Record<PageKey, { title: string; sub: string }> = {
  overview: { title: "总览", sub: "代理运行状态与实时流量" },
  providers: { title: "模型", sub: "接入与管理各家大模型服务商" },
  routes: { title: "路由策略", sub: "配置模型转发与优先级规则" },
  users: { title: "访问控制", sub: "管理调用者、Key 与请求限额" },
  logs: { title: "请求日志", sub: "追踪每次转发的上下文与结果" },
  dataset: { title: "数据归档", sub: "存储成功请求的 prompt 与 completion" },
  settings: { title: "运行设置", sub: "重试、熔断与服务参数配置" },
};

export function AppShell({
  page,
  onNavigate,
  data,
  children,
}: {
  page: PageKey;
  onNavigate: (p: PageKey) => void;
  data: AppData;
  children: ReactNode;
}) {
  const st = data.status;
  const openBreakers = Object.entries(data.breakers).filter(([, b]) => b.state === "open");
  const halfOpen = Object.entries(data.breakers).some(([, b]) => b.state === "half_open");
  const broken = openBreakers.length > 0;
  const down = !!st && !st.running;

  // 每秒刷新倒计时显示
  const [, tick] = useState(0);
  useEffect(() => {
    if (!broken) return;
    const id = setInterval(() => tick((n) => n + 1), 1000);
    return () => clearInterval(id);
  }, [broken]);

  const fmtCooldown = (secs: number | null | undefined): string => {
    if (!secs || secs <= 0) return "即将恢复";
    if (secs >= 3600) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
    if (secs >= 60) return `${Math.floor(secs / 60)}m ${secs % 60}s`;
    return `${secs}s`;
  };

  return (
    <div className="app-shell">
      {data.streaming && <div className="stream-pulse" />}
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">
            <Cable size={18} strokeWidth={2.2} />
          </div>
          <div>
            <div className="brand-name">Token Proxy</div>
            <div className="brand-sub">AI Gateway</div>
          </div>
        </div>

        <nav className="nav">
          {NAV.map((n) => (
            <button
              key={n.key}
              className={`nav-item ${page === n.key ? "active" : ""}`}
              onClick={() => onNavigate(n.key)}
            >
              {n.icon}
              {n.label}
            </button>
          ))}
        </nav>

        <div className="sidebar-footer">
          <div className="server-mini" title={st?.config_path}>
            {st?.running ? (
              <StatusDot color={broken ? "amber" : "cyan"} />
            ) : (
              <StatusDot color="gray" />
            )}
            <span className="mono">
              {st ? `${st.host}:${st.port}` : "…"}
            </span>
            <span style={{ marginLeft: "auto", color: "var(--text-3)", fontSize: 11 }}>
              {st ? `↑${fmtCount(st.total_requests)}` : ""}
            </span>
          </div>
        </div>
      </aside>

      <main className="main">
        <div className="topbar">
          <div>
            <h1>{PAGE_TITLES[page].title}</h1>
            <div className="sub">{PAGE_TITLES[page].sub}</div>
          </div>
          <div className="topbar-right">
            {down ? (
              <span className="pill warn" title={st.server_error ?? undefined}>
                <Activity size={13} />
                代理未监听 · 检查端口
              </span>
            ) : broken ? (
              <span className="pill warn">
                <Activity size={13} />
                熔断中 · {openBreakers.length} 个上游故障
              </span>
            ) : halfOpen ? (
              <span className="pill warn">
                <Activity size={13} />
                探活中 · 等待上游恢复
              </span>
            ) : st?.running ? (
              <span className="pill ok">
                <StatusDot color="cyan" />
                代理运行中
              </span>
            ) : (
              <span className="pill">
                <StatusDot color="gray" />
                未运行
              </span>
            )}
          </div>
        </div>

        {/* 全局熔断告警栏 */}
        {broken && (
          <div className="breaker-alert-bar">
            <TriangleAlert size={15} className="breaker-alert-icon" />
            <div className="breaker-alert-body">
              <span className="breaker-alert-title">
                {openBreakers.length === 1
                  ? `上游 ${openBreakers[0][0]} 已熔断`
                  : `${openBreakers.length} 个上游已熔断：${openBreakers.map(([k]) => k).join("、")}`}
              </span>
              <div className="breaker-alert-chips">
                {openBreakers.map(([pid, b]) => (
                  <span key={pid} className="breaker-alert-chip">
                    {pid}
                    {b.cooldown_remaining_secs != null && b.cooldown_remaining_secs > 0
                      ? ` · ${fmtCooldown(b.cooldown_remaining_secs)}`
                      : " · 即将探活"}
                  </span>
                ))}
              </div>
            </div>
          </div>
        )}

        <div className="content">{children}</div>
      </main>
    </div>
  );
}
