// The Piggy brand mark - a compact vector piggy bank (pink body + gold star
// coin) distilled from the app icon (docs/mockups/icon.svg). Used in the header
// wordmark in place of the 🐷 emoji so the brand reads the same in every theme.
// Solid fills only (no gradients) to stay crisp at ~18px.

export function PiggyMark({ size = 18, className }: { size?: number; className?: string }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      role="img"
      aria-label="Piggy"
    >
      {/* coin dropping into the slot */}
      <g transform="rotate(-14 12 6.2)">
        <circle cx="12" cy="6.2" r="2.5" fill="#ffd60a" />
        <path
          d="M12 4.7 L12.55 5.85 L13.8 5.95 L12.85 6.75 L13.15 8 L12 7.3 L10.85 8 L11.15 6.75 L10.2 5.95 L11.45 5.85 Z"
          fill="#c8930a"
        />
      </g>
      {/* tail */}
      <path
        d="M6.1 14.3 q-1.4 -0.2 -1.15 -1.5 q0.2 -1.1 1.4 -0.8"
        fill="none"
        stroke="#ee5a7d"
        strokeWidth="0.9"
        strokeLinecap="round"
      />
      {/* body */}
      <ellipse cx="12.1" cy="14.4" rx="6.9" ry="5.3" fill="#ff7da8" />
      {/* ear */}
      <path d="M14.1 9.2 q0.8 -1.4 2.3 -1.3 q0.2 1.5 -0.9 2.4 Z" fill="#ee5a7d" />
      {/* coin slot */}
      <rect x="10.1" y="9.7" width="3.8" height="0.9" rx="0.45" fill="#b45a72" />
      {/* snout */}
      <ellipse cx="17.9" cy="14.6" rx="2" ry="1.55" fill="#ee5a7d" />
      <ellipse cx="17.35" cy="14.6" rx="0.32" ry="0.55" fill="#8f3a52" />
      <ellipse cx="18.5" cy="14.6" rx="0.32" ry="0.55" fill="#8f3a52" />
      {/* eye */}
      <circle cx="15.2" cy="12.4" r="0.72" fill="#301820" />
      {/* legs */}
      <rect x="9" y="18.7" width="1.7" height="1.9" rx="0.8" fill="#ee5a7d" />
      <rect x="13.4" y="18.7" width="1.7" height="1.9" rx="0.8" fill="#ee5a7d" />
    </svg>
  );
}
