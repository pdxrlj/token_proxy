import { useState } from "react";
import { AppShell, type PageKey } from "./components/AppShell";
import { ToastProvider } from "./components/ui";
import { useAppData } from "./hooks/useAppData";
import { Overview } from "./pages/Overview";
import { Providers } from "./pages/Providers";
import { Routes } from "./pages/Routes";
import { Users } from "./pages/Users";
import { Logs } from "./pages/Logs";
import { Dataset } from "./pages/Dataset";
import { Settings } from "./pages/Settings";

function AppInner() {
  const data = useAppData();
  const [page, setPage] = useState<PageKey>("overview");

  return (
    <AppShell page={page} onNavigate={setPage} data={data}>
      {page === "overview" && <Overview data={data} />}
      {page === "providers" && <Providers data={data} />}
      {page === "routes" && <Routes data={data} />}
      {page === "users" && <Users data={data} />}
      {page === "logs" && <Logs data={data} />}
      {page === "dataset" && <Dataset />}
      {page === "settings" && <Settings data={data} />}
    </AppShell>
  );
}

export default function App() {
  return (
    <ToastProvider>
      <AppInner />
    </ToastProvider>
  );
}
