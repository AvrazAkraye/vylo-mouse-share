import { useEffect, useRef, useState } from "react";
import { CheckCircle2, KeyRound, Laptop, RadioTower, XCircle } from "lucide-react";
import { Card, CardBody, CardHeader } from "../components/ui/card";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { Dialog } from "../components/ui/dialog";
import { Tabs } from "../components/ui/tabs";
import { Spinner } from "../components/ui/spinner";
import { requests, type DiscoveredPeer } from "../lib/ipc";
import { useDaemon, useDaemonDispatch } from "../lib/store";
import { shortFingerprint } from "../lib/utils";
import type { TabId } from "../App";

function PinDisplay({ pin }: { pin: string }) {
  const digits = pin.split("");
  return (
    <div className="flex items-center justify-center gap-1.5" aria-label={`PIN ${pin}`}>
      {digits.map((d, i) => (
        <span key={i} className="contents">
          {i === 3 && <span className="mx-1 text-2xl font-light text-faint">–</span>}
          <span className="flex h-12 w-9 items-center justify-center rounded-lg border border-line bg-surface2 font-mono text-2xl font-semibold text-ink">
            {d}
          </span>
        </span>
      ))}
    </div>
  );
}

const PIN_RE = /^\d{6}$/;

export function PairingScreen({ onNavigate }: { onNavigate: (tab: TabId) => void }) {
  const s = useDaemon();
  const dispatch = useDaemonDispatch();
  const { pairing } = s;

  const [mode, setMode] = useState<"nearby" | "manual">("nearby");
  const [dialogPeer, setDialogPeer] = useState<DiscoveredPeer | null>(null);
  const [pinInput, setPinInput] = useState("");
  const [manualAddr, setManualAddr] = useState("");
  const [manualPin, setManualPin] = useState("");
  const [manualError, setManualError] = useState<string | null>(null);

  // Auto-navigate to Layout shortly after a successful pairing.
  const navTimer = useRef<number | null>(null);
  useEffect(() => {
    if (pairing.phase === "complete") {
      navTimer.current = window.setTimeout(() => {
        dispatch({ type: "pairing-dismiss" });
        onNavigate("layout");
      }, 1400);
    }
    return () => {
      if (navTimer.current !== null) window.clearTimeout(navTimer.current);
    };
  }, [pairing.phase, dispatch, onNavigate]);

  const deviceName = s.vylo?.device_name ?? "This device";
  const fp = s.fingerprint;

  const startShowPin = () => requests.startPairing().catch(() => {});
  const cancelPairing = () => {
    requests.cancelPairing().catch(() => {});
    dispatch({ type: "pairing-dismiss" });
  };

  const pairWith = (addr: string, pin: string) => {
    dispatch({ type: "pairing-waiting" });
    requests.pairWithPeer(addr, pin).catch(() => {});
  };

  const submitDialogPin = () => {
    if (!dialogPeer || !PIN_RE.test(pinInput)) return;
    const addr = `${dialogPeer.addrs[0]}:${dialogPeer.port}`;
    setDialogPeer(null);
    setPinInput("");
    pairWith(addr, pinInput);
  };

  const submitManual = () => {
    const addr = manualAddr.trim();
    if (!/^\S+:\d{1,5}$/.test(addr)) {
      setManualError("Enter the peer as host:port, e.g. 192.168.1.7:4243");
      return;
    }
    if (!PIN_RE.test(manualPin)) {
      setManualError("The PIN is the 6 digits shown on the other machine");
      return;
    }
    setManualError(null);
    pairWith(addr, manualPin);
  };

  /* ---------------- This device card body, per pairing phase ------- */

  let thisDeviceBody;
  if (pairing.phase === "showing-pin" && pairing.pin) {
    thisDeviceBody = (
      <div className="flex flex-col items-center gap-4 py-2">
        <PinDisplay pin={pairing.pin} />
        <div className="flex items-center gap-2 text-[13px] text-muted">
          <Spinner size={14} />
          Waiting for peer… enter this PIN on the other machine
          {pairing.port ? ` (port ${pairing.port})` : ""}
        </div>
        <Button variant="secondary" onClick={cancelPairing}>
          Cancel
        </Button>
      </div>
    );
  } else if (pairing.phase === "waiting-peer") {
    thisDeviceBody = (
      <div className="flex flex-col items-center gap-4 py-4">
        <Spinner size={22} />
        <div className="text-[13px] text-muted">Pairing with peer…</div>
        <Button variant="secondary" onClick={cancelPairing}>
          Cancel
        </Button>
      </div>
    );
  } else if (pairing.phase === "complete") {
    thisDeviceBody = (
      <div className="flex flex-col items-center gap-2.5 py-4">
        <CheckCircle2 size={28} className="text-ok" />
        <div className="text-sm font-semibold text-ink">
          Paired with {pairing.peerName ?? "peer"}
        </div>
        <div className="text-[13px] text-muted">Taking you to screen layout…</div>
      </div>
    );
  } else {
    thisDeviceBody = (
      <div className="flex items-center justify-between gap-4">
        <div className="flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-surface2 text-muted">
            <Laptop size={18} />
          </div>
          <div>
            <div className="text-sm font-medium text-ink">{deviceName}</div>
            <div className="font-mono text-xs text-muted">
              {fp ? shortFingerprint(fp) : "identity pending…"}
            </div>
          </div>
        </div>
        <Button variant="primary" size="lg" onClick={startShowPin} disabled={!s.ipcConnected}>
          <KeyRound size={15} />
          Show PIN
        </Button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <Card>
        <CardHeader
          title="This device"
          subtitle="Show a PIN here and enter it on the other machine, or the other way round."
        />
        <CardBody>
          {thisDeviceBody}
          {pairing.phase === "failed" && (
            <div className="mt-3 flex items-center justify-between gap-3 rounded-lg border border-err/25 bg-err-soft px-3 py-2.5">
              <div className="flex items-center gap-2 text-[13px] text-err">
                <XCircle size={15} />
                {pairing.error ?? "Pairing failed"}
              </div>
              <Button size="sm" variant="secondary" onClick={() => dispatch({ type: "pairing-dismiss" })}>
                Try again
              </Button>
            </div>
          )}
        </CardBody>
      </Card>

      <Card>
        <CardHeader
          title="Pair with a device"
          action={
            <Tabs
              items={[
                { value: "nearby", label: "Nearby" },
                { value: "manual", label: "By address" },
              ]}
              value={mode}
              onChange={setMode}
            />
          }
        />
        <CardBody className="pt-2">
          {mode === "nearby" ? (
            s.peers.length === 0 ? (
              <div className="flex flex-col items-center gap-2 py-6 text-center">
                <RadioTower size={20} className="text-faint" />
                <div className="text-[13px] text-muted">
                  Looking for Vylo devices on your network…
                </div>
                <div className="text-xs text-faint">
                  Open Vylo on the other machine and keep both on the same Wi-Fi or LAN.
                </div>
              </div>
            ) : (
              <div className="divide-y divide-line">
                {s.peers.map((peer) => (
                  <div
                    key={`${peer.name}-${peer.addrs[0] ?? ""}`}
                    className="flex items-center justify-between gap-3 py-2.5"
                  >
                    <div className="flex min-w-0 items-center gap-3">
                      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-accent-soft text-accent">
                        <Laptop size={15} />
                      </div>
                      <div className="min-w-0">
                        <div className="truncate text-sm font-medium text-ink">{peer.name}</div>
                        <div className="truncate text-xs text-muted">
                          {peer.addrs.join(", ")} · port {peer.port}
                        </div>
                      </div>
                    </div>
                    <Button
                      size="sm"
                      variant="primary"
                      disabled={peer.addrs.length === 0 || pairing.phase === "waiting-peer"}
                      onClick={() => {
                        setPinInput("");
                        setDialogPeer(peer);
                      }}
                    >
                      Pair
                    </Button>
                  </div>
                ))}
              </div>
            )
          ) : (
            <div className="flex flex-col gap-3 py-1">
              <div className="grid grid-cols-[1fr_130px] gap-2.5">
                <div>
                  <label className="mb-1 block text-xs font-medium text-muted">
                    Peer address
                  </label>
                  <Input
                    placeholder="192.168.1.7:4243"
                    value={manualAddr}
                    onChange={(e) => setManualAddr(e.target.value)}
                    spellCheck={false}
                  />
                </div>
                <div>
                  <label className="mb-1 block text-xs font-medium text-muted">PIN</label>
                  <Input
                    placeholder="123456"
                    inputMode="numeric"
                    maxLength={6}
                    className="font-mono tracking-widest"
                    value={manualPin}
                    onChange={(e) => setManualPin(e.target.value.replace(/\D/g, ""))}
                    onKeyDown={(e) => e.key === "Enter" && submitManual()}
                  />
                </div>
              </div>
              {manualError && <div className="text-xs text-err">{manualError}</div>}
              <div>
                <Button
                  variant="primary"
                  onClick={submitManual}
                  disabled={pairing.phase === "waiting-peer" || !s.ipcConnected}
                >
                  Pair by address
                </Button>
              </div>
              <p className="text-xs text-faint">
                Use this if discovery is blocked on your network. The PIN is shown by
                “Show PIN” on the other machine.
              </p>
            </div>
          )}
        </CardBody>
      </Card>

      {/* PIN entry dialog for a discovered peer */}
      <Dialog
        open={dialogPeer !== null}
        onClose={() => setDialogPeer(null)}
        title={`Pair with ${dialogPeer?.name ?? ""}`}
        footer={
          <>
            <Button variant="secondary" onClick={() => setDialogPeer(null)}>
              Cancel
            </Button>
            <Button variant="primary" disabled={!PIN_RE.test(pinInput)} onClick={submitDialogPin}>
              Pair
            </Button>
          </>
        }
      >
        <p className="mb-3 text-[13px] text-muted">
          On <span className="font-medium text-ink">{dialogPeer?.name}</span>, click
          “Show PIN” and enter the 6 digits it displays.
        </p>
        <Input
          autoFocus
          placeholder="123456"
          inputMode="numeric"
          maxLength={6}
          className="text-center font-mono text-lg tracking-[0.4em]"
          value={pinInput}
          onChange={(e) => setPinInput(e.target.value.replace(/\D/g, ""))}
          onKeyDown={(e) => e.key === "Enter" && submitDialogPin()}
        />
      </Dialog>
    </div>
  );
}
