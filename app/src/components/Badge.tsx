import { badgeView } from "../lib/badge";
import type { Badge } from "../types";

const TONE_CLASS = {
  measured: "measured",
  estimated: "estimated",
  claimed: "claimed",
  nodata: "nodata",
} as const;

/** Per-saver measured/estimated/measuring/claimed badge - never blends them. */
export function BadgeChip({ badge }: { badge: Badge }) {
  const v = badgeView(badge);
  return (
    <span className={`badge ${TONE_CLASS[v.tone]}`} title={v.title}>
      {v.text}
    </span>
  );
}
