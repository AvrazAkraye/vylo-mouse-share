import { useEffect, useRef, useState } from "react";
import { ArrowLeftRight, Keyboard, MousePointer2, RotateCcw } from "lucide-react";
import { Card, CardBody, CardHeader } from "./ui/card";
import { Button } from "./ui/button";
import { Slider } from "./ui/slider";
import { Select } from "./ui/select";
import {
  DEFAULT_SPEED,
  IDENTITY_MODIFIERS,
  MAX_SPEED,
  MIN_SPEED,
  requests,
  type Modifier,
  type ModifierMap,
} from "../lib/ipc";
import { useDaemon, usePeerClient, usePlatform } from "../lib/store";

const ROLES: Modifier[] = ["ctrl", "alt", "meta"];

const SPEED_STEP = 0.05;

/** snap to the slider's grid (config.toml may hold any in-range value) */
function snapSpeed(v: number): number {
  const snapped = MIN_SPEED + Math.round((v - MIN_SPEED) / SPEED_STEP) * SPEED_STEP;
  return Math.round(Math.min(MAX_SPEED, Math.max(MIN_SPEED, snapped)) * 100) / 100;
}

/** names of the keys on *this* keyboard */
function localKeyName(role: Modifier, platform: string | null): string {
  if (platform === "macos") {
    return { ctrl: "Control  ⌃", alt: "Option  ⌥", meta: "Command  ⌘" }[role];
  }
  if (platform === "windows") {
    return { ctrl: "Ctrl", alt: "Alt", meta: "Windows key" }[role];
  }
  return { ctrl: "Ctrl", alt: "Alt", meta: "Super" }[role];
}

/** what a role means on the other machine — named for both platforms */
function targetName(role: Modifier): string {
  return { ctrl: "Ctrl", alt: "Alt / Option", meta: "Win / Command" }[role];
}

function isIdentity(m: ModifierMap): boolean {
  return m.ctrl === "ctrl" && m.alt === "alt" && m.meta === "meta";
}

/**
 * Per-peer input tuning: how fast the mouse moves over there, and what
 * each of our modifier keys acts as. Applied on this machine before
 * events are sent, so the peer needs no matching setting.
 */
export function PeerInputCard() {
  const s = useDaemon();
  const peer = usePeerClient();
  const platform = usePlatform();

  const configSpeed = snapSpeed(peer?.config.speed ?? DEFAULT_SPEED);
  const modifiers = peer?.config.modifiers ?? IDENTITY_MODIFIERS;
  const peerName = s.sync.peer_name || peer?.config.hostname || "the other machine";

  // The slider owns its value while dragging; daemon state wins otherwise.
  // `lastSent` is the value we asked for that the daemon hasn't echoed yet
  // (requests queue while the bridge reconnects), so a follow-up commit is
  // judged against what we sent, not against stale daemon state.
  const [speed, setSpeed] = useState(configSpeed);
  const dragging = useRef(false);
  const lastSent = useRef<number | null>(null);
  useEffect(() => {
    if (!dragging.current) setSpeed(configSpeed);
    if (lastSent.current !== null && configSpeed === lastSent.current) lastSent.current = null;
  }, [configSpeed]);

  if (!peer) return null;
  const handle = peer.handle;

  const commitSpeed = (v: number) => {
    dragging.current = false;
    const next = snapSpeed(v);
    setSpeed(next);
    const baseline = lastSent.current ?? configSpeed;
    if (next !== baseline) {
      lastSent.current = next;
      requests.updateSpeed(handle, next).catch(() => {});
    }
  };

  const setMap = (next: ModifierMap) => {
    requests.updateModifierMap(handle, next).catch(() => {});
  };

  return (
    <Card>
      <CardHeader
        title={`Input on ${peerName}`}
        action={
          !isIdentity(modifiers) || configSpeed !== DEFAULT_SPEED ? (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                commitSpeed(DEFAULT_SPEED);
                if (!isIdentity(modifiers)) setMap(IDENTITY_MODIFIERS);
              }}
            >
              <RotateCcw size={14} />
              Reset
            </Button>
          ) : undefined
        }
      />
      <CardBody className="space-y-5">
        {/* Mouse speed */}
        <div>
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-2.5">
              <div className="flex h-8 w-8 items-center justify-center rounded-lg bg-surface2 text-muted">
                <MousePointer2 size={15} />
              </div>
              <div>
                <div className="text-sm font-medium text-ink">Mouse speed</div>
                <div className="text-[13px] text-muted">
                  How fast the cursor moves while it's on {peerName}
                </div>
              </div>
            </div>
            <span className="w-14 shrink-0 text-right font-mono text-[13px] tabular-nums text-ink">
              {speed.toFixed(2)}×
            </span>
          </div>
          <div className="mt-3 flex items-center gap-3 pl-[42px]">
            <span className="text-[11px] text-faint">Slower</span>
            <Slider
              aria-label="Mouse speed"
              min={MIN_SPEED}
              max={MAX_SPEED}
              step={SPEED_STEP}
              value={speed}
              onChange={(v) => {
                dragging.current = true;
                setSpeed(v);
              }}
              onCommit={commitSpeed}
            />
            <span className="text-[11px] text-faint">Faster</span>
          </div>
        </div>

        {/* Modifier keys */}
        <div>
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-2.5">
              <div className="flex h-8 w-8 items-center justify-center rounded-lg bg-surface2 text-muted">
                <Keyboard size={15} />
              </div>
              <div>
                <div className="text-sm font-medium text-ink">Modifier keys</div>
                <div className="text-[13px] text-muted">
                  What each key on this keyboard does on {peerName}
                </div>
              </div>
            </div>
            <Button
              size="sm"
              variant="secondary"
              title="Control acts as Win/Command and Command acts as Ctrl"
              onClick={() =>
                setMap({
                  ...modifiers,
                  ctrl: modifiers.ctrl === "meta" && modifiers.meta === "ctrl" ? "ctrl" : "meta",
                  meta: modifiers.ctrl === "meta" && modifiers.meta === "ctrl" ? "meta" : "ctrl",
                })
              }
            >
              <ArrowLeftRight size={14} />
              {modifiers.ctrl === "meta" && modifiers.meta === "ctrl"
                ? "Unswap"
                : platform === "macos"
                  ? "Swap ⌃ and ⌘"
                  : "Swap Ctrl and Win"}
            </Button>
          </div>
          <div className="mt-3 grid grid-cols-[1fr_auto_1fr] items-center gap-x-3 gap-y-2 pl-[42px]">
            {ROLES.map((role) => (
              <RoleRow
                key={role}
                label={localKeyName(role, platform)}
                value={modifiers[role]}
                onChange={(v) => setMap({ ...modifiers, [role]: v })}
              />
            ))}
          </div>
          <p className="mt-3 pl-[42px] text-xs leading-5 text-faint">
            Changes apply instantly and only affect keys sent from this machine. Set the
            other direction on {peerName}.
          </p>
        </div>
      </CardBody>
    </Card>
  );
}

function RoleRow({
  label,
  value,
  onChange,
}: {
  label: string;
  value: Modifier;
  onChange: (v: Modifier) => void;
}) {
  return (
    <>
      <span className="truncate text-[13px] text-ink">{label}</span>
      <span className="text-xs text-faint">acts as</span>
      <Select
        aria-label={`${label} acts as`}
        value={value}
        onChange={(e) => onChange(e.target.value as Modifier)}
      >
        {ROLES.map((r) => (
          <option key={r} value={r}>
            {targetName(r)}
          </option>
        ))}
      </Select>
    </>
  );
}
