import type { HTMLAttributes } from "react";
import { cn } from "../../lib/utils";

type Tone = "neutral" | "ok" | "warn" | "err" | "accent";

const tones: Record<Tone, string> = {
  neutral: "bg-surface2 text-muted border-line",
  ok: "bg-ok-soft text-ok border-ok/25",
  warn: "bg-warn-soft text-warn border-warn/25",
  err: "bg-err-soft text-err border-err/25",
  accent: "bg-accent-soft text-accent border-accent/25",
};

export function Badge({
  tone = "neutral",
  className,
  ...props
}: HTMLAttributes<HTMLSpanElement> & { tone?: Tone }) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-medium",
        tones[tone],
        className,
      )}
      {...props}
    />
  );
}

/** Small colored status dot, used inside badges and lists. */
export function Dot({ tone = "neutral" }: { tone?: Tone }) {
  const colors: Record<Tone, string> = {
    neutral: "bg-faint",
    ok: "bg-ok",
    warn: "bg-warn",
    err: "bg-err",
    accent: "bg-accent",
  };
  return <span className={cn("inline-block h-1.5 w-1.5 rounded-full", colors[tone])} />;
}
