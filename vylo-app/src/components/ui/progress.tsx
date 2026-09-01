import { cn } from "../../lib/utils";

export function Progress({
  value,
  max = 100,
  tone = "accent",
  className,
}: {
  value: number;
  max?: number;
  tone?: "accent" | "ok" | "err";
  className?: string;
}) {
  const pct = max > 0 ? Math.min(100, Math.max(0, (value / max) * 100)) : 0;
  const bar =
    tone === "ok" ? "bg-ok" : tone === "err" ? "bg-err" : "bg-accent";
  return (
    <div
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={max}
      aria-valuenow={value}
      className={cn("h-1.5 w-full overflow-hidden rounded-full bg-surface2", className)}
    >
      <div
        className={cn("h-full rounded-full transition-[width] duration-200", bar)}
        style={{ width: `${pct}%` }}
      />
    </div>
  );
}
