import { useEffect, useRef, useState } from "react";
import { Link2, Monitor } from "lucide-react";
import { Card, CardBody, CardHeader } from "../components/ui/card";
import { Button } from "../components/ui/button";
import { requests, type Position } from "../lib/ipc";
import { useDaemon, usePeerClient } from "../lib/store";
import { cn } from "../lib/utils";
import type { TabId } from "../App";

/* Geometry (pixels, relative to canvas center). */
const RW = 150; // machine rect width
const RH = 100; // machine rect height
const GAP = 26;

const SLOTS: Record<Position, { dx: number; dy: number }> = {
  left: { dx: -(RW + GAP), dy: 0 },
  right: { dx: RW + GAP, dy: 0 },
  top: { dx: 0, dy: -(RH + GAP) },
  bottom: { dx: 0, dy: RH + GAP },
};

function nearestSlot(dx: number, dy: number): Position {
  let best: Position = "right";
  let bestDist = Number.POSITIVE_INFINITY;
  for (const pos of Object.keys(SLOTS) as Position[]) {
    const s = SLOTS[pos];
    const d = (s.dx - dx) ** 2 + (s.dy - dy) ** 2;
    if (d < bestDist) {
      bestDist = d;
      best = pos;
    }
  }
  return best;
}

function MachineRect({
  label,
  sublabel,
  accent,
  className,
  style,
  ...rest
}: {
  label: string;
  sublabel?: string;
  accent?: boolean;
} & React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn(
        "absolute left-1/2 top-1/2 flex flex-col items-center justify-center gap-1 rounded-xl border",
        accent
          ? "border-accent bg-accent-soft text-ink"
          : "border-line-strong bg-surface text-ink shadow-card",
        className,
      )}
      style={{ width: RW, height: RH, ...style }}
      {...rest}
    >
      <Monitor size={18} className={accent ? "text-accent" : "text-muted"} />
      <div className="max-w-[130px] truncate px-2 text-[13px] font-semibold">{label}</div>
      {sublabel && <div className="text-[11px] text-muted">{sublabel}</div>}
    </div>
  );
}

export function LayoutScreen({ onNavigate }: { onNavigate: (tab: TabId) => void }) {
  const s = useDaemon();
  const peer = usePeerClient();

  const [pendingPos, setPendingPos] = useState<Position | null>(null);
  const [drag, setDrag] = useState<{ dx: number; dy: number } | null>(null);
  const dragStart = useRef<{ x: number; y: number } | null>(null);

  const confirmedPos = peer?.config.pos ?? "right";
  const shownPos = pendingPos ?? confirmedPos;

  // Clear the optimistic position once the daemon confirms it.
  useEffect(() => {
    if (pendingPos !== null && confirmedPos === pendingPos) setPendingPos(null);
  }, [confirmedPos, pendingPos]);

  const localName = s.vylo?.device_name ?? "This device";
  const peerName = peer?.config.hostname || s.sync.peer_name || "Peer device";

  if (!peer) {
    return (
      <Card>
        <CardBody className="flex flex-col items-center gap-3 py-14 text-center">
          <div className="flex h-12 w-12 items-center justify-center rounded-xl bg-surface2 text-faint">
            <Link2 size={20} />
          </div>
          <div className="text-sm font-semibold text-ink">No peer to arrange yet</div>
          <p className="max-w-sm text-[13px] text-muted">
            Once you pair another machine it appears here, and you can place it on
            whichever edge of this screen you flick your mouse against.
          </p>
          <Button variant="primary" onClick={() => onNavigate("pairing")}>
            Go to pairing
          </Button>
        </CardBody>
      </Card>
    );
  }

  const slot = SLOTS[shownPos];
  const peerDx = slot.dx + (drag?.dx ?? 0);
  const peerDy = slot.dy + (drag?.dy ?? 0);
  const highlight = drag ? nearestSlot(peerDx, peerDy) : null;

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    e.currentTarget.setPointerCapture(e.pointerId);
    dragStart.current = { x: e.clientX, y: e.clientY };
    setDrag({ dx: 0, dy: 0 });
  };
  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!dragStart.current) return;
    setDrag({ dx: e.clientX - dragStart.current.x, dy: e.clientY - dragStart.current.y });
  };
  const onPointerUp = () => {
    if (!dragStart.current || !drag) {
      dragStart.current = null;
      setDrag(null);
      return;
    }
    const target = nearestSlot(slot.dx + drag.dx, slot.dy + drag.dy);
    dragStart.current = null;
    setDrag(null);
    if (target !== confirmedPos) {
      setPendingPos(target);
      requests.updatePosition(peer.handle, target).catch(() => setPendingPos(null));
    }
  };

  return (
    <div className="flex flex-col gap-4">
      <Card>
        <CardHeader
          title="Screen layout"
          subtitle={`Drag ${peerName} to the edge where it sits relative to this screen.`}
        />
        <CardBody>
          <div
            className="relative mx-auto overflow-hidden rounded-xl border border-line bg-surface2/50"
            style={{
              height: 2 * (RH + GAP) + RH + 48,
              backgroundImage:
                "radial-gradient(circle, var(--line) 1px, transparent 1px)",
              backgroundSize: "18px 18px",
            }}
          >
            {/* Slot outlines while dragging */}
            {drag &&
              (Object.keys(SLOTS) as Position[]).map((pos) => (
                <div
                  key={pos}
                  className={cn(
                    "absolute left-1/2 top-1/2 rounded-xl border-2 border-dashed transition-colors",
                    highlight === pos ? "border-accent bg-accent-soft/60" : "border-line-strong/60",
                  )}
                  style={{
                    width: RW,
                    height: RH,
                    transform: `translate(calc(-50% + ${SLOTS[pos].dx}px), calc(-50% + ${SLOTS[pos].dy}px))`,
                  }}
                />
              ))}

            {/* Local machine, fixed center */}
            <MachineRect
              label={localName}
              sublabel="this screen"
              style={{ transform: "translate(-50%, -50%)" }}
            />

            {/* Peer, draggable */}
            <MachineRect
              label={peerName}
              accent
              className={cn(
                "cursor-grab touch-none select-none",
                drag ? "z-10 cursor-grabbing shadow-xl transition-none" : "transition-transform duration-200",
              )}
              style={{
                transform: `translate(calc(-50% + ${peerDx}px), calc(-50% + ${peerDy}px))`,
              }}
              onPointerDown={onPointerDown}
              onPointerMove={onPointerMove}
              onPointerUp={onPointerUp}
              onPointerCancel={onPointerUp}
            />
          </div>

          <div className="mt-3 flex items-center justify-between text-[13px] text-muted">
            <span>
              {peerName} is on the{" "}
              <span className="font-medium text-ink">{shownPos}</span>
              {pendingPos !== null && " (saving…)"}
            </span>
            <span className="text-xs text-faint">
              Cursor switches when it crosses that edge
            </span>
          </div>
        </CardBody>
      </Card>
    </div>
  );
}
