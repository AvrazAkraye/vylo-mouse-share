import { forwardRef, type ButtonHTMLAttributes } from "react";
import { cn } from "../../lib/utils";

type Variant = "primary" | "secondary" | "ghost" | "danger";
type Size = "sm" | "md" | "lg";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant;
  size?: Size;
}

const variants: Record<Variant, string> = {
  primary:
    "bg-accent text-on-accent hover:bg-accent-hover active:opacity-90 shadow-card",
  secondary:
    "bg-surface text-ink border border-line hover:bg-surface2 active:bg-surface2",
  ghost: "text-ink hover:bg-surface2 active:bg-surface2",
  danger: "bg-err text-white hover:opacity-90 active:opacity-80",
};

const sizes: Record<Size, string> = {
  sm: "h-7 px-2.5 text-[13px] gap-1.5 rounded-md",
  md: "h-8.5 px-3.5 text-sm gap-2 rounded-lg",
  lg: "h-10 px-5 text-sm gap-2 rounded-lg",
};

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant = "secondary", size = "md", type = "button", ...props }, ref) => (
    <button
      ref={ref}
      type={type}
      className={cn(
        "inline-flex items-center justify-center font-medium whitespace-nowrap select-none",
        "transition-colors duration-100 outline-none",
        "focus-visible:ring-2 focus-visible:ring-accent/50",
        "disabled:opacity-50 disabled:pointer-events-none",
        variants[variant],
        sizes[size],
        className,
      )}
      {...props}
    />
  ),
);
Button.displayName = "Button";
