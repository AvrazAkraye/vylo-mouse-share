import { forwardRef, type InputHTMLAttributes } from "react";
import { cn } from "../../lib/utils";

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(
  ({ className, ...props }, ref) => (
    <input
      ref={ref}
      className={cn(
        "h-8.5 w-full rounded-lg border border-line bg-surface px-3 text-sm text-ink",
        "placeholder:text-faint outline-none transition-shadow",
        "focus:border-accent focus:ring-2 focus:ring-accent/25",
        "disabled:opacity-50",
        className,
      )}
      {...props}
    />
  ),
);
Input.displayName = "Input";
