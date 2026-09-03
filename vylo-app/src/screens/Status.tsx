import { useState } from "react";
import {
  ArrowDownLeft,
  ArrowUpRight,
  ClipboardCopy,
  FilePlus2,
  Folder,
  FolderOpen,
  FolderPlus,
  Monitor,
  MousePointer2,
  RefreshCw,
  ShieldAlert,
} from "lucide-react";
import { Card, CardBody, CardHeader } from "../components/ui/card";
import { Dot } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Switch } from "../components/ui/switch";
import { Progress } from "../components/ui/progress";
import { backend, requests, type FileTransfer } from "../lib/ipc";
import { useDaemon, usePeerClient, usePlatform } from "../lib/store";
import { cn, formatBytes } from "../lib/utils";
import type { TabId } from "../App";

function StatePill({
  label,
  tone,
  text,
}: {
  label: string;
  tone: "ok" | "warn" | "neutral" | "err";
  text: string;
}) {
  return (
    <div className="flex items-center gap-2 rounded-lg border border-line bg-surface2/60 px-3 py-2">
      <Dot tone={tone} />
      <div className="min-w-0">
        <div className="text-[11px] font-medium uppercase tracking-wide text-faint">{label}</div>
        <div className="truncate text-[13px] font-medium text-ink">{text}</div>
      </div>
    </div>
  );
}

function TransferRow({ t }: { t: FileTransfer }) {
  const received = t.direction === "received";
  const folder = t.kind === "folder";
  const Icon = folder ? Folder : received ? ArrowDownLeft : ArrowUpRight;
  const fileCount = folder ? `${t.files} ${t.files === 1 ? "file" : "files"}` : null;
  return (
    <div className="flex items-center gap-3 py-2.5">
      <div
        className={cn(
          "flex h-7 w-7 shrink-0 items-center justify-center rounded-lg",
          received ? "bg-accent-soft text-accent" : "bg-surface2 text-muted",
        )}
      >
        <Icon size={14} />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline justify-between gap-2">
          <span className="truncate text-[13px] font-medium text-ink">
            {t.name}
            {fileCount && <span className="ml-1.5 font-normal text-faint">{fileCount}</span>}
          </span>
          <span className="shrink-0 text-xs text-faint">
            {t.state === "active"
              ? `${formatBytes(t.transferred)} / ${formatBytes(t.total)}`
              : t.state === "done"
                ? formatBytes(t.total)
                : "Failed"}
          </span>
        </div>
        {t.state === "active" ? (
          <Progress value={t.transferred} max={t.total} className="mt-1.5" />
        ) : t.state === "failed" ? (
          <div className="mt-0.5 truncate text-xs text-err">{t.detail || "Transfer failed"}</div>
        ) : (
          <div className="mt-0.5 text-xs text-ok">
            {received ? "Received" : "Sent"}
          </div>
        )}
      </div>
      {t.state === "done" && received && t.detail && (
        <Button
          size="sm"
          variant="ghost"
          aria-label="Show in folder"
          title="Show in folder"
          onClick={() => backend.openFileDir(t.detail!).catch(() => {})}
        >
          <FolderOpen size={14} />
        </Button>
      )}
    </div>
  );
}

/** Amber "input sharing disabled" card with a Retry that re-enables both paths. */
function InputPermissionsCard() {
  const platform = usePlatform();
  const [retrying, setRetrying] = useState(false);

  const retry = async () => {
    if (retrying) return;
    setRetrying(true);
    try {
      await requests.retryInput();
    } catch {
      /* bridge offline; statuses will update via events when it returns */
    } finally {
      window.setTimeout(() => setRetrying(false), 800);
    }
  };

  const isMac = platform === "macos";
  return (
    <div className="flex items-start gap-3 rounded-xl border border-warn/30 bg-warn-soft px-4 py-3.5">
      <ShieldAlert size={18} className="mt-0.5 shrink-0 text-warn" />
      <div className="min-w-0 flex-1">
        <div className="text-sm font-semibold text-ink">
          {isMac ? "Input sharing needs macOS permissions" : "Input sharing is disabled"}
        </div>
        <p className="mt-0.5 text-[13px] leading-5 text-muted">
          {isMac ? (
            <>
              Grant <span className="font-medium text-ink">Accessibility</span> and{" "}
              <span className="font-medium text-ink">Input Monitoring</span> in System
              Settings → Privacy &amp; Security, then click Retry.
            </>
          ) : (
            "Mouse and keyboard capture/emulation is currently off. Click Retry to re-enable it."
          )}
        </p>
      </div>
      <Button size="sm" variant="secondary" onClick={retry} disabled={retrying} className="shrink-0">
        {retrying ? "Retrying…" : "Retry"}
      </Button>
    </div>
  );
}

export function StatusScreen({ onNavigate }: { onNavigate: (tab: TabId) => void }) {
  const s = useDaemon();
  const peer = usePeerClient();
  const [picking, setPicking] = useState(false);

  const peerName =
    s.sync.peer_name || peer?.config.hostname || (peer ? "Peer device" : null);

  const inputLinked = (peer?.state.alive ?? false) || s.incoming.length > 0;
  const inputReady = s.captureEnabled || s.emulationEnabled;
  const clipboardOn = s.vylo?.clipboard_sync ?? false;

  const sendFiles = async () => {
    if (picking) return;
    setPicking(true);
    try {
      const files = await backend.pickFiles();
      if (files && files.length > 0) await requests.sendFiles(files);
    } catch {
      /* dialog cancelled or bridge offline */
    } finally {
      setPicking(false);
    }
  };

  const sendFolder = async () => {
    if (picking) return;
    setPicking(true);
    try {
      const dir = await backend.pickDir("Send folder");
      if (dir) await requests.sendFiles([dir]);
    } catch {
      /* dialog cancelled or bridge offline */
    } finally {
      setPicking(false);
    }
  };

  const inputDisabled = s.ipcConnected && (!s.captureEnabled || !s.emulationEnabled);

  return (
    <div className="flex flex-col gap-4">
      {/* Input permissions / disabled warning */}
      {inputDisabled && <InputPermissionsCard />}

      {/* Hero */}
      <Card>
        <CardBody className="pb-4">
          <div className="flex items-center gap-4">
            <div
              className={cn(
                "flex h-12 w-12 shrink-0 items-center justify-center rounded-xl",
                peerName && s.sync.connected
                  ? "bg-accent text-on-accent"
                  : "bg-surface2 text-faint",
              )}
            >
              <Monitor size={22} />
            </div>
            <div className="min-w-0 flex-1">
              <div className="truncate text-lg font-semibold text-ink">
                {peerName ?? "No device paired"}
              </div>
              <div className="text-[13px] text-muted">
                {peerName
                  ? s.sync.connected
                    ? "Connected on your network"
                    : "Paired — waiting for connection"
                  : "Pair another machine to share your mouse, keyboard and clipboard."}
              </div>
            </div>
            {!peerName && (
              <Button variant="primary" onClick={() => onNavigate("pairing")}>
                Pair a device
              </Button>
            )}
          </div>

          {peerName && (
            <div className="mt-4 grid grid-cols-3 gap-2.5">
              <StatePill
                label="Input link"
                tone={inputLinked ? "ok" : inputReady ? "warn" : "neutral"}
                text={inputLinked ? "Linked" : inputReady ? "Ready" : "Off"}
              />
              <StatePill
                label="Sync channel"
                tone={s.sync.connected ? "ok" : "neutral"}
                text={s.sync.connected ? "Connected" : "Offline"}
              />
              <StatePill
                label="Clipboard"
                tone={clipboardOn ? (s.sync.connected ? "ok" : "warn") : "neutral"}
                text={clipboardOn ? "Syncing" : "Off"}
              />
            </div>
          )}
        </CardBody>
      </Card>

      {/* Clipboard */}
      <Card>
        <CardBody className="flex items-center justify-between gap-4">
          <div className="flex items-center gap-3">
            <div className="flex h-8 w-8 items-center justify-center rounded-lg bg-surface2 text-muted">
              <ClipboardCopy size={15} />
            </div>
            <div>
              <div className="text-sm font-medium text-ink">Clipboard sync</div>
              <div className="text-[13px] text-muted">
                {s.lastClipboard
                  ? `Last ${s.lastClipboard.kind} ${s.lastClipboard.direction} ${new Date(
                      s.lastClipboard.at,
                    ).toLocaleTimeString()}`
                  : "Copy on one machine, paste on the other"}
              </div>
            </div>
          </div>
          <Switch
            checked={clipboardOn}
            disabled={s.vylo === null}
            aria-label="Clipboard sync"
            onCheckedChange={(v) => requests.setClipboardSync(v).catch(() => {})}
          />
        </CardBody>
      </Card>

      {/* Drop zone */}
      <div
        className={cn(
          "flex flex-col items-center justify-center gap-2 rounded-xl border-2 border-dashed border-line",
          "bg-surface px-6 py-6 text-center transition-colors",
          s.dragHover && "border-accent bg-accent-soft/40",
        )}
      >
        <FilePlus2 size={20} className={cn("text-faint", s.dragHover && "text-accent")} />
        <div className="text-sm font-medium text-ink">Drop files or folders here</div>
        <div className="text-xs text-muted">
          {s.sync.connected
            ? `Sent straight to ${peerName ?? "your peer"} — or just drag them off the edge of the screen onto it`
            : "Peer is offline — transfers start when it reconnects"}
        </div>
        <div className="mt-1.5 flex items-center gap-2">
          <Button size="sm" variant="secondary" onClick={sendFiles} disabled={picking}>
            <FilePlus2 size={14} />
            Choose files…
          </Button>
          <Button size="sm" variant="secondary" onClick={sendFolder} disabled={picking}>
            <FolderPlus size={14} />
            Choose folder…
          </Button>
        </div>
      </div>

      {/* Cross-machine drag-and-drop landed here */}
      {s.lastDrop && s.lastDrop.paths.length > 0 && (
        <Card>
          <CardBody className="flex items-center justify-between gap-3 py-3">
            <div className="min-w-0">
              <div className="text-sm font-medium text-ink">
                {s.lastDrop.paths.length === 1
                  ? "1 item dropped here"
                  : `${s.lastDrop.paths.length} items dropped here`}
              </div>
              <div className="truncate text-xs text-muted">
                {s.lastDrop.paths.map((p) => p.split(/[\\/]/).pop()).join(", ")}
              </div>
            </div>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => backend.openFileDir(s.lastDrop!.paths[0]).catch(() => {})}
            >
              Show
            </Button>
          </CardBody>
        </Card>
      )}

      {/* Transfers */}
      <Card>
        <CardHeader
          title="Recent transfers"
          action={
            s.vylo?.file_dir ? (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => backend.openFileDir(s.vylo!.file_dir).catch(() => {})}
              >
                <FolderOpen size={14} />
                Open folder
              </Button>
            ) : undefined
          }
        />
        <CardBody className="pt-1">
          {s.transfers.length === 0 ? (
            <div className="flex items-center gap-2.5 py-3 text-[13px] text-muted">
              <RefreshCw size={14} className="text-faint" />
              Nothing transferred yet — drop a file or folder above to get started.
            </div>
          ) : (
            <div className="divide-y divide-line">
              {s.transfers.map((t) => (
                <TransferRow key={`${t.direction}-${t.id}`} t={t} />
              ))}
            </div>
          )}
        </CardBody>
      </Card>

      {/* Input hint when nothing is linked yet but a peer exists */}
      {peerName && !inputLinked && (
        <div className="flex items-center gap-2 px-1 text-[13px] text-muted">
          <MousePointer2 size={14} className="text-faint" />
          Move your cursor against the shared screen edge to switch machines.
        </div>
      )}
    </div>
  );
}
