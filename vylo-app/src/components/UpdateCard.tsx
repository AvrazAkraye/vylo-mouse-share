import { useCallback, useEffect, useState } from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { Download, RefreshCw, CheckCircle2, AlertTriangle } from "lucide-react";
import { Card, CardBody, CardHeader } from "./ui/card";
import { Button } from "./ui/button";
import { Badge } from "./ui/badge";
import { Progress } from "./ui/progress";
import { useAppVersion } from "../lib/store";

type Phase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "up-to-date"; at: number }
  | { kind: "available"; update: Update }
  | { kind: "downloading"; update: Update; progress: number | null }
  | { kind: "installing" }
  | { kind: "error"; message: string };

/**
 * "About" card: shows the installed version and drives the in-app update
 * flow (check → download+install → relaunch) against the signed manifest
 * published on GitHub Releases by CI.
 */
export function UpdateCard() {
  const version = useAppVersion();
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });

  const checkNow = useCallback(async () => {
    setPhase({ kind: "checking" });
    try {
      const update = await check();
      setPhase(update ? { kind: "available", update } : { kind: "up-to-date", at: Date.now() });
    } catch (e) {
      setPhase({ kind: "error", message: humanize(e) });
    }
  }, []);

  const install = useCallback(async () => {
    if (phase.kind !== "available") return;
    const { update } = phase;
    setPhase({ kind: "downloading", update, progress: 0 });
    let total = 0;
    let received = 0;
    try {
      await update.downloadAndInstall((ev) => {
        if (ev.event === "Started") {
          total = ev.data.contentLength ?? 0;
        } else if (ev.event === "Progress") {
          received += ev.data.chunkLength;
          setPhase({
            kind: "downloading",
            update,
            progress: total > 0 ? Math.min(1, received / total) : null,
          });
        } else if (ev.event === "Finished") {
          setPhase({ kind: "installing" });
        }
      });
      await relaunch();
    } catch (e) {
      setPhase({ kind: "error", message: humanize(e) });
    }
  }, [phase]);

  // a quiet check on first open so a newer release is surfaced without
  // the user having to ask
  useEffect(() => {
    checkNow();
  }, [checkNow]);

  return (
    <Card>
      <CardHeader
        title="About"
        action={
          <Badge tone="neutral" className="font-mono">
            v{version}
          </Badge>
        }
      />
      <CardBody className="space-y-3">
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0 text-[13px] leading-5 text-muted">
            {phase.kind === "idle" && "Vylo Mouse Share"}
            {phase.kind === "checking" && "Checking for updates…"}
            {phase.kind === "up-to-date" && (
              <span className="inline-flex items-center gap-1.5">
                <CheckCircle2 size={14} className="text-emerald-500" />
                You're on the latest version
              </span>
            )}
            {phase.kind === "available" && (
              <span>
                <span className="font-medium text-ink">
                  Version {phase.update.version} is available
                </span>
                {phase.update.body ? (
                  <span className="block truncate text-xs">{phase.update.body}</span>
                ) : null}
              </span>
            )}
            {phase.kind === "downloading" &&
              (phase.progress === null
                ? "Downloading update…"
                : `Downloading update… ${Math.round(phase.progress * 100)}%`)}
            {phase.kind === "installing" && "Installing — Vylo will restart in a moment"}
            {phase.kind === "error" && (
              <span className="inline-flex items-center gap-1.5 text-amber-600">
                <AlertTriangle size={14} />
                {phase.message}
              </span>
            )}
          </div>

          <div className="shrink-0">
            {phase.kind === "available" ? (
              <Button size="sm" onClick={install}>
                <Download size={14} />
                Update to {phase.update.version}
              </Button>
            ) : phase.kind === "downloading" || phase.kind === "installing" ? (
              <Button size="sm" variant="secondary" disabled>
                <RefreshCw size={14} className="animate-spin" />
                Updating…
              </Button>
            ) : (
              <Button
                size="sm"
                variant="secondary"
                onClick={checkNow}
                disabled={phase.kind === "checking"}
              >
                <RefreshCw size={14} className={phase.kind === "checking" ? "animate-spin" : ""} />
                Check for updates
              </Button>
            )}
          </div>
        </div>

        {phase.kind === "downloading" && phase.progress !== null && (
          <Progress value={phase.progress * 100} />
        )}
      </CardBody>
    </Card>
  );
}

function humanize(e: unknown): string {
  const raw = e instanceof Error ? e.message : String(e);
  // the updater surfaces raw HTTP/network errors; keep them short
  if (/404|Not Found/i.test(raw)) return "No update manifest published yet";
  if (/network|fetch|dns|connect/i.test(raw)) return "Couldn't reach the update server";
  return raw.length > 120 ? raw.slice(0, 117) + "…" : raw;
}
