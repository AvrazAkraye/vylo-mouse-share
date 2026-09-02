import { useState } from "react";
import { Activity, Link2, LayoutGrid, Settings as SettingsIcon } from "lucide-react";
import { requests } from "./lib/ipc";
import { useAppVersion, useDaemon } from "./lib/store";
import { cn } from "./lib/utils";
import { DropOverlay } from "./components/DropOverlay";
import { Dot } from "./components/ui/badge";
import { StatusScreen } from "./screens/Status";
import { PairingScreen } from "./screens/Pairing";
import { LayoutScreen } from "./screens/Layout";
import { SettingsScreen } from "./screens/Settings";

export type TabId = "status" | "pairing" | "layout" | "settings";

const NAV: Array<{ id: TabId; label: string; icon: typeof Activity }> = [
  { id: "status", label: "Status", icon: Activity },
  { id: "pairing", label: "Pairing", icon: Link2 },
  { id: "layout", label: "Layout", icon: LayoutGrid },
  { id: "settings", label: "Settings", icon: SettingsIcon },
];

export default function App() {
  const [tab, setTab] = useState<TabId>("status");
  const s = useDaemon();
  const version = useAppVersion();

  return (
    <div className="flex h-full bg-bg text-ink">
      {/* Left nav rail */}
      <aside className="flex w-44 shrink-0 flex-col border-r border-line bg-surface">
        <div className="flex items-center gap-2.5 px-4 pb-4 pt-5">
          <div className="flex h-7 w-7 items-center justify-center rounded-lg bg-accent text-[13px] font-bold text-on-accent">
            V
          </div>
          <span className="text-sm font-semibold tracking-tight text-ink">Vylo</span>
        </div>
        <nav className="flex flex-col gap-0.5 px-2.5">
          {NAV.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              type="button"
              onClick={() => setTab(id)}
              className={cn(
                "flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-[13px] font-medium transition-colors",
                "outline-none focus-visible:ring-2 focus-visible:ring-accent/50",
                tab === id
                  ? "bg-accent-soft text-accent"
                  : "text-muted hover:bg-surface2 hover:text-ink",
              )}
            >
              <Icon size={16} />
              {label}
            </button>
          ))}
        </nav>
        <div className="mt-auto border-t border-line px-4 py-3">
          <div className="flex items-center gap-2 text-xs text-muted">
            <Dot tone={s.ipcConnected ? (s.sync.connected ? "ok" : "neutral") : "warn"} />
            {s.ipcConnected
              ? s.sync.connected
                ? `Connected to ${s.sync.peer_name ?? "peer"}`
                : "Service running"
              : "Service starting…"}
          </div>
          {s.ipcConnected && (!s.captureEnabled || !s.emulationEnabled) && (
            <div className="mt-1.5 flex items-center gap-2 text-xs">
              <Dot tone="warn" />
              <span className="text-warn">Input disabled</span>
              <button
                type="button"
                onClick={() => requests.retryInput().catch(() => {})}
                className="font-medium text-accent underline-offset-2 outline-none hover:underline focus-visible:underline"
              >
                Retry
              </button>
            </div>
          )}
          <button
            type="button"
            onClick={() => setTab("settings")}
            title="About & updates"
            className="mt-1.5 text-[11px] text-faint outline-none hover:text-muted focus-visible:text-muted"
          >
            v{version}
          </button>
        </div>
      </aside>

      {/* Content */}
      <main className="min-w-0 flex-1 overflow-y-auto">
        {!s.ipcConnected && (
          <div className="flex items-center gap-2 border-b border-warn/25 bg-warn-soft px-5 py-2 text-[13px] text-warn">
            <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-warn" />
            Starting service… reconnecting to the Vylo background service.
          </div>
        )}
        <div className="mx-auto max-w-2xl px-5 py-5">
          {tab === "status" && <StatusScreen onNavigate={setTab} />}
          {tab === "pairing" && <PairingScreen onNavigate={setTab} />}
          {tab === "layout" && <LayoutScreen onNavigate={setTab} />}
          {tab === "settings" && <SettingsScreen />}
        </div>
      </main>

      <DropOverlay />
    </div>
  );
}
