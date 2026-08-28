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
  { key: "providers", label: "上游 Provider", icon: <ServerCog size={16} /> },
  { key: "routes", label: "路由", icon: <Waypoints size={16} /> },
  { key: "users", label: "用户管理", icon: <Users size={16} /> },
  { key: "logs", label: "请求日志", icon: <ScrollText size={16} /> },
  { key: "dataset", label: "数据存储", icon: <Database size={16} /> },
  { key: "settings", label: "重试与设置", icon: <Settings size={16} /> },
];

const PAGE_TITLES: Record<PageKey, { title: string; sub: string }> = {
  overview: { title: "总览", sub: "代理运行状态与实时流量" },
  providers: { title: "上游 Provider", sub: "管理模型服务商与密钥" },
  routes: { title: "路由", sub: "模型到上游的转发规则" },
  users: { title: "用户管理", sub: "管理使用者及其请求配额" },
  logs: { title: "请求日志", sub: "每次转发的完整记录" },
  dataset: { title: "数据存储", sub: "成功请求的 prompt + completion 存档" },
  settings: { title: "重试与设置", sub: "失败重试策略、服务配置与导入导出" },
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
  const broken = Object.values(data.breakers).some((b) => b.state === "open");
  const down = !!st && !st.running;

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
            <div className="brand-sub">Dispatch Console</div>
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
                熔断中 · 上游可能故障
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
        <div className="content">{children}</div>
      </main>
    </div>
  );
}
