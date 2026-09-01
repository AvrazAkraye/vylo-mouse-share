import { FileUp } from "lucide-react";
import { useDaemon } from "../lib/store";

/** Full-window overlay shown while files are dragged over the window. */
export function DropOverlay() {
  const { dragHover } = useDaemon();
  if (!dragHover) return null;
  return (
    <div className="pointer-events-none fixed inset-0 z-40 flex items-center justify-center bg-accent/10 backdrop-blur-[2px]">
      <div className="flex flex-col items-center gap-3 rounded-2xl border-2 border-dashed border-accent bg-surface/95 px-10 py-8 shadow-xl">
        <FileUp size={28} className="text-accent" />
        <div className="text-sm font-semibold text-ink">Drop to send to your peer</div>
        <div className="text-[13px] text-muted">Files are transferred over the encrypted sync channel</div>
      </div>
    </div>
  );
}
