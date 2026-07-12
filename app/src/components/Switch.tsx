interface SwitchProps {
  on: boolean;
  onChange: (next: boolean) => void;
  sm?: boolean;
  disabled?: boolean;
  busy?: boolean;
  label?: string;
}

/** A macOS-style toggle switch (40×24, or 34×20 with `sm`), matching the mockup. */
export function Switch({ on, onChange, sm, disabled, busy, label }: SwitchProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      disabled={disabled || busy}
      className={`switch ${sm ? "sm" : ""} ${on ? "on" : ""} ${busy ? "busy" : ""}`}
      onClick={() => onChange(!on)}
    >
      <span className="knob" />
    </button>
  );
}
