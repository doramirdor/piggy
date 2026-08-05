interface SwitchProps {
  on: boolean;
  onChange: (next: boolean) => void;
  disabled?: boolean;
  busy?: boolean;
  label?: string;
}

/** A macOS-style toggle switch (40×22), matching the toggle sheet. */
export function Switch({ on, onChange, disabled, busy, label }: SwitchProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      disabled={disabled || busy}
      className={`switch ${on ? "on" : ""} ${busy ? "busy" : ""}`}
      onClick={() => onChange(!on)}
    >
      <span className="knob" />
    </button>
  );
}
