import { statusView } from "../lib/badge";
import type { Badge } from "../types";

/** Status chip with a leading indicator: a determinate progress bar for
 *  "Measuring" (honest n-of-10 holdout progress, never an endless spinner), a
 *  small icon or dot for the settled states. Mirrors the v1.0 badge set. */
export function StatusChip({ badge }: { badge: Badge }) {
  const v = statusView(badge);
  const pct = Math.round((v.progress ?? 0) * 100);
  return (
    <span className={`status-chip ${v.tone}`} title={v.title}>
      {v.tone === "measuring" ? (
        <span
          className="chip-progress"
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={pct}
        >
          <span className="chip-progress-fill" style={{ width: `${pct}%` }} />
        </span>
      ) : v.tone === "measured" ? (
        <svg className="chip-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
          <path d="M20 6 9 17l-5-5" />
        </svg>
      ) : v.tone === "nodata" ? (
        <svg className="chip-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
          <path d="M3 15 8 10l4 3 5-6 4 4" />
        </svg>
      ) : (
        <span className="chip-dot" aria-hidden />
      )}
      {v.label}
    </span>
  );
}
