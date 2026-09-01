import { cn } from "../../lib/utils";

export interface TabItem<T extends string> {
  value: T;
  label: string;
}

/** Segmented control (shadcn Tabs-style trigger row). */
export function Tabs<T extends string>({
  items,
  value,
  onChange,
  className,
}: {
  items: Array<TabItem<T>>;
  value: T;
  onChange: (value: T) => void;
  className?: string;
}) {
  return (
    <div
      role="tablist"
      className={cn(
        "inline-flex items-center gap-0.5 rounded-lg bg-surface2 p-0.5 border border-line",
        className,
      )}
    >
      {items.map((item) => (
        <button
          key={item.value}
          role="tab"
          type="button"
          aria-selected={value === item.value}
          onClick={() => onChange(item.value)}
          className={cn(
            "rounded-md px-3 py-1 text-[13px] font-medium transition-colors",
            "outline-none focus-visible:ring-2 focus-visible:ring-accent/50",
            value === item.value
              ? "bg-surface text-ink shadow-card"
              : "text-muted hover:text-ink",
          )}
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}
