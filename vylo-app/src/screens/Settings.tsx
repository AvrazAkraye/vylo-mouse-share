import { useEffect, useState } from "react";
import { Check, Copy, Fingerprint, FolderOpen, ShieldCheck, Trash2 } from "lucide-react";
import { Card, CardBody, CardHeader } from "../components/ui/card";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { Switch } from "../components/ui/switch";
import { Badge } from "../components/ui/badge";
import { Dialog } from "../components/ui/dialog";
import { PeerInputCard } from "../components/PeerInputCard";
import { UpdateCard } from "../components/UpdateCard";
import { backend, requests } from "../lib/ipc";
import { useDaemon } from "../lib/store";
import { copyText, shortFingerprint } from "../lib/utils";

function Row({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-6 py-3">
      <div className="min-w-0">
        <div className="text-sm font-medium text-ink">{label}</div>
        {hint && <div className="mt-0.5 text-[13px] text-muted">{hint}</div>}
      </div>
      <div className="flex shrink-0 items-center gap-2">{children}</div>
    </div>
  );
}

export function SettingsScreen() {
  const s = useDaemon();

  /* Device name (commit on blur) */
  const [name, setName] = useState("");
  const [nameDirty, setNameDirty] = useState(false);
  useEffect(() => {
    if (!nameDirty) setName(s.vylo?.device_name ?? "");
  }, [s.vylo?.device_name, nameDirty]);

  const commitName = () => {
    setNameDirty(false);
    const trimmed = name.trim();
    if (trimmed && trimmed !== (s.vylo?.device_name ?? "")) {
      requests.setDeviceName(trimmed).catch(() => {});
    } else {
      setName(s.vylo?.device_name ?? "");
    }
  };

  /* Autostart */
  const [autostart, setAutostart] = useState<boolean | null>(null);
  useEffect(() => {
    backend
      .getAutostart()
      .then(setAutostart)
      .catch(() => setAutostart(null));
  }, []);
  const toggleAutostart = async (v: boolean) => {
    setAutostart(v); // optimistic
    try {
      await backend.setAutostart(v);
      setAutostart(await backend.getAutostart());
    } catch {
      setAutostart(!v);
    }
  };

  /* File dir */
  const changeDir = async () => {
    try {
      const dir = await backend.pickDir();
      if (dir) await requests.setFileDir(dir);
    } catch {
      /* cancelled */
    }
  };

  /* Fingerprint copy */
  const [copied, setCopied] = useState(false);
  const doCopy = async () => {
    if (!s.fingerprint) return;
    if (await copyText(s.fingerprint)) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    }
  };

  /* Authorized keys */
  const [removeFp, setRemoveFp] = useState<string | null>(null);
  const confirmRemove = () => {
    if (removeFp) requests.removeAuthorizedKey(removeFp).catch(() => {});
    setRemoveFp(null);
  };

  const authorized = Object.entries(s.authorized);

  return (
    <div className="flex flex-col gap-4">
      <Card>
        <CardHeader title="General" />
        <CardBody className="divide-y divide-line pt-0">
          <Row label="Device name" hint="Shown to nearby devices while pairing">
            <Input
              className="w-56"
              value={name}
              placeholder="my-computer"
              onChange={(e) => {
                setNameDirty(true);
                setName(e.target.value);
              }}
              onBlur={commitName}
              onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
              disabled={s.vylo === null}
            />
          </Row>
          <Row label="Received files" hint="Where incoming files are saved">
            <span
              className="max-w-64 truncate font-mono text-xs text-muted selectable"
              title={s.vylo?.file_dir ?? ""}
            >
              {s.vylo?.file_dir ?? "—"}
            </span>
            {s.vylo?.file_dir && (
              <Button
                size="sm"
                variant="ghost"
                aria-label="Open folder"
                title="Open folder"
                onClick={() => backend.openFileDir(s.vylo!.file_dir).catch(() => {})}
              >
                <FolderOpen size={14} />
              </Button>
            )}
            <Button size="sm" onClick={changeDir} disabled={s.vylo === null}>
              Change…
            </Button>
          </Row>
          <Row
            label="Sync port"
            hint="Must be the same on both machines. Set sync_port in config.toml, then restart."
          >
            <Badge tone="neutral" className="font-mono">
              {s.vylo?.sync_port ?? 4243}
            </Badge>
          </Row>
          <Row
            label="Sync keyboard language"
            hint="Switch the paired device's input language when you switch yours"
          >
            <Switch
              checked={s.vylo?.keyboard_layout_sync ?? true}
              aria-label="Sync keyboard language"
              onCheckedChange={(v) => requests.setKeyboardLayoutSync(v)}
            />
          </Row>
          <Row label="Start on login" hint="Launch Vylo when you sign in">
            <Switch
              checked={autostart ?? false}
              disabled={autostart === null}
              aria-label="Start on login"
              onCheckedChange={toggleAutostart}
            />
          </Row>
        </CardBody>
      </Card>

      <PeerInputCard />

      <Card>
        <CardHeader
          title="This device's identity"
          subtitle="Peers verify this fingerprint before any input or data is accepted."
        />
        <CardBody>
          <div className="flex items-center gap-3 rounded-lg border border-line bg-surface2/60 px-3 py-2.5">
            <Fingerprint size={16} className="shrink-0 text-muted" />
            <code className="min-w-0 flex-1 break-all font-mono text-xs leading-4.5 text-ink selectable">
              {s.fingerprint ?? "Waiting for the service to report its identity…"}
            </code>
            <Button
              size="sm"
              variant="ghost"
              onClick={doCopy}
              disabled={!s.fingerprint}
              aria-label="Copy fingerprint"
            >
              {copied ? <Check size={14} className="text-ok" /> : <Copy size={14} />}
            </Button>
          </div>
        </CardBody>
      </Card>

      <Card>
        <CardHeader
          title="Authorized devices"
          subtitle="Machines allowed to connect. Pairing adds devices here automatically."
        />
        <CardBody className="pt-1">
          {authorized.length === 0 ? (
            <div className="flex items-center gap-2.5 py-3 text-[13px] text-muted">
              <ShieldCheck size={15} className="text-faint" />
              No devices authorized yet — pair one from the Pairing tab.
            </div>
          ) : (
            <div className="divide-y divide-line">
              {authorized.map(([fp, desc]) => (
                <div key={fp} className="flex items-center justify-between gap-3 py-2.5">
                  <div className="min-w-0">
                    <div className="truncate text-sm font-medium text-ink">
                      {desc || "Unnamed device"}
                    </div>
                    <div className="truncate font-mono text-xs text-muted" title={fp}>
                      {shortFingerprint(fp)} · {fp.slice(0, 23)}…
                    </div>
                  </div>
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={`Remove ${desc || fp}`}
                    className="text-err hover:bg-err-soft"
                    onClick={() => setRemoveFp(fp)}
                  >
                    <Trash2 size={14} />
                  </Button>
                </div>
              ))}
            </div>
          )}
        </CardBody>
      </Card>

      <UpdateCard />

      <Dialog
        open={removeFp !== null}
        onClose={() => setRemoveFp(null)}
        title="Remove authorized device?"
        footer={
          <>
            <Button variant="secondary" onClick={() => setRemoveFp(null)}>
              Cancel
            </Button>
            <Button variant="danger" onClick={confirmRemove}>
              Remove
            </Button>
          </>
        }
      >
        <p className="text-[13px] leading-5 text-muted">
          <span className="font-medium text-ink">
            {removeFp ? s.authorized[removeFp] || "This device" : ""}
          </span>{" "}
          will no longer be able to connect, share input, or exchange the clipboard and
          files with this machine. You can pair it again at any time.
        </p>
      </Dialog>
    </div>
  );
}
