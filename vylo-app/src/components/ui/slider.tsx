import { useRef } from "react";
import { cn } from "../../lib/utils";

/** Native range input styled to the design tokens (accent track fill). */
export function Slider({
  value,
  min,
  max,
  step,
  onChange,
  onCommit,
  disabled,
  className,
  "aria-label": ariaLabel,
}: {
  value: number;
  min: number;
  max: number;
  step: number;
  /** fires continuously while dragging */
  onChange: (value: number) => void;
  /** fires once the interaction that changed the value ends */
  onCommit?: (value: number) => void;
  disabled?: boolean;
  className?: string;
  "aria-label"?: string;
}) {
  const pct = max > min ? ((value - min) / (max - min)) * 100 : 0;

  // Only commit after the user actually moved the thumb: pointerup /
  // keyup / blur also fire for a plain click or a Tab through the page,
  // and the browser snaps the DOM value to the step grid, so committing
  // unconditionally would "change" a value the user never touched.
  const dirty = useRef(false);
  const commit = (el: HTMLInputElement) => {
    if (!dirty.current) return;
    dirty.current = false;
    onCommit?.(Number(el.value));
  };

  return (
    <input
      type="range"
      min={min}
      max={max}
      step={step}
      value={value}
      disabled={disabled}
      aria-label={ariaLabel}
      onChange={(e) => {
        dirty.current = true;
        onChange(Number(e.target.value));
      }}
      onPointerUp={(e) => commit(e.target as HTMLInputElement)}
      onKeyUp={(e) => commit(e.target as HTMLInputElement)}
      onBlur={(e) => commit(e.target)}
      style={{
        background: `linear-gradient(to right, var(--accent) ${pct}%, var(--line-strong) ${pct}%)`,
      }}
      className={cn("vylo-slider w-full", disabled && "opacity-50", className)}
    />
  );
}
