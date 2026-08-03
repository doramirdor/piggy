// The Piggy brand mark: the coin from the wordmark lockup.
//
// The full pig belongs on the app icon at 512px, where its geometry reads. At
// interface scale it collapses into a pink blob, so the mark here is the coin
// the wordmark already carries: a gold disc inside a coral ring, which stays
// legible at 12px and is unmistakable in a tab strip or a menu bar.
//
// Solid fills only, no gradients, so it is crisp at every size and survives a
// monochrome render.

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
      {/* coral ring */}
      <circle cx="12" cy="12" r="9.5" fill="var(--accent-brand, #e85b6a)" />
      {/* the paper showing through, so the ring reads as a ring on any ground */}
      <circle cx="12" cy="12" r="7" fill="var(--sheet, #ffffff)" />
      {/* the coin */}
      <circle cx="12" cy="12" r="5.6" fill="var(--coin, #d4a017)" />
    </svg>
  );
}
