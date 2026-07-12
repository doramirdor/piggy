// Badge-state mapper: turn a per-saver `Badge` into the text + tone the row
// renders. Kept pure and dependency-free so it is unit-testable in isolation.
//
// DESIGN.md: measured and claimed numbers are NEVER blended. A "measured" badge
// shows Piggy's own holdout delta; "measuring" shows honest progress; "claimed"
// shows the author's number, clearly labelled as a claim.

import type { Badge } from "../types";
import { pctMagnitude } from "./format";

export type BadgeTone = "measured" | "nodata" | "claimed";

export interface BadgeView {
  text: string;
  tone: BadgeTone;
  /** Hover text with the honest caveat. */
  title: string;
}

export function badgeView(b: Badge): BadgeView {
  switch (b.kind) {
    case "measured": {
      // A measured badge without a delta is not really measured — fall back to
      // the honest "measuring" state rather than printing "−0%".
      if (b.delta == null) {
        return measuring(b.n);
      }
      const pct = pctMagnitude(b.delta);
      const sessions = `${b.n} session${b.n === 1 ? "" : "s"}`;
      return {
        text: `−${pct} measured`,
        tone: "measured",
        title: `Measured across ${sessions} vs. a holdout`,
      };
    }
    case "claimed":
      return {
        text: "author claims",
        tone: "claimed",
        title: "The author's own number — treat as marketing until Piggy measures it",
      };
    case "measuring":
    default:
      return measuring(b.n);
  }
}

function measuring(n: number): BadgeView {
  return {
    text: `measuring · ${n} session${n === 1 ? "" : "s"}`,
    tone: "nodata",
    title: "Piggy is still gathering honest holdout data",
  };
}
