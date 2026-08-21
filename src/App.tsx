import { useState } from "react";
import { AppShell, type PageKey } from "./components/AppShell";
import { ToastProvider } from "./components/ui";
import { useAppData } from "./hooks/useAppData";
import { Overview } from "./pages/Overview";
import { Providers } from "./pages/Providers";
import { Routes } from "./pages/Routes";
import { Logs } from "./pages/Logs";
import { Settings } from "./pages/Settings";

function AppInner() {
  const data = useAppData();
  const [page, setPage] = useState<PageKey>("overview");

  return (
    <AppShell page={page} onNavigate={setPage} data={data}>
      {page === "overview" && <Overview data={data} />}
      {page === "providers" && <Providers data={data} />}
      {page === "routes" && <Routes data={data} />}
      {page === "logs" && <Logs data={data} />}
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
