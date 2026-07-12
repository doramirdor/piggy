import { badgeView } from "../lib/badge";
import type { Badge } from "../types";

/** Per-saver measured/measuring/claimed badge — never blends the three. */
export function BadgeChip({ badge }: { badge: Badge }) {
  const v = badgeView(badge);
  const cls = v.tone === "measured" ? "measured" : v.tone === "claimed" ? "claimed" : "nodata";
  return (
    <span className={`badge ${cls}`} title={v.title}>
      {v.text}
    </span>
  );
}
