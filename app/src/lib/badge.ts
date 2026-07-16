// Badge-state mapper: turn a per-saver `Badge` into the text + tone the row
// renders. Kept pure and dependency-free so it is unit-testable in isolation.
//
// DESIGN.md: measured, estimated, and claimed numbers are NEVER blended. A
// "measured" badge shows Piggy's own holdout (randomized) delta; "estimated"
// shows the same math against your pre-install history (observational), clearly
// labelled; "measuring" shows honest progress; "claimed" shows the author's
// number, clearly labelled as a claim.

import type { Badge } from "../types";
import { pctMagnitude } from "./format";

export type BadgeTone = "measured" | "estimated" | "nodata" | "claimed";

export interface BadgeView {
  text: string;
  tone: BadgeTone;
  /** Hover text with the honest caveat. */
  title: string;
}

/** A delta fraction as a signed percentage: negative = saving → "−22%". */
function signedPct(delta: number): string {
  const sign = delta > 0 ? "+" : "−";
  return `${sign}${pctMagnitude(delta)}`;
}

export function badgeView(b: Badge): BadgeView {
  switch (b.kind) {
    case "measured": {
      // A measured badge without a delta is not really measured - fall back to
      // the honest "measuring" state rather than printing "−0%".
      if (b.delta == null) {
        return measuring(b.n);
      }
      const sessions = `${b.n} session${b.n === 1 ? "" : "s"}`;
      return {
        text: `${signedPct(b.delta)} measured`,
        tone: "measured",
        title: `Measured across ${sessions} vs. a holdout`,
      };
    }
    case "estimated": {
      // Same as measured, but the baseline is observational history, not a live
      // holdout - surfaced with the "≈" hedge and its own gray-blue tone.
      if (b.delta == null) {
        return measuring(b.n);
      }
      const sessions = `${b.n} session${b.n === 1 ? "" : "s"}`;
      return {
        text: `≈ ${signedPct(b.delta)} estimated`,
        tone: "estimated",
        title: `Estimated from ${sessions} of your history - holdout measurement in progress`,
      };
    }
    case "claimed":
      return {
        text: "author claims",
        tone: "claimed",
        title: "The author's own number - treat as marketing until Piggy measures it",
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

export type StatusTone = "measured" | "estimated" | "measuring" | "nodata" | "claimed";

/** Holdout sessions Piggy needs before a saver flips from "Measuring" to a
 *  settled "Measured" delta. Mirrors the "N of 10 holdout sessions" copy. */
export const MEASURE_TARGET = 10;

export interface StatusView {
  label: string;
  tone: StatusTone;
  title: string;
  /** For the "measuring" tone: fraction toward MEASURE_TARGET, 0..1. */
  progress?: number;
}

/** A saver's status as a short WORD + tone (for the status chip): "Measured"
 *  (holdout delta in hand), "Estimating" (observational), "Measuring" (holdout
 *  in progress, n>0), or "No data" (nothing observed yet). Savings numbers are
 *  shown separately - this is only the state. */
export function statusView(b: Badge): StatusView {
  if (b.kind === "measured" && b.delta != null) {
    return { label: "Measured", tone: "measured", title: `Measured across ${b.n} sessions vs. a holdout` };
  }
  if (b.kind === "estimated" && b.delta != null) {
    return { label: "Estimating", tone: "estimated", title: `Estimated from ${b.n} sessions of your history` };
  }
  if (b.kind === "claimed") {
    return { label: "Claimed", tone: "claimed", title: "The author's own number - not yet measured" };
  }
  if (b.n > 0) {
    return {
      label: "Measuring",
      tone: "measuring",
      title: `Gathering holdout data - ${b.n} of ${MEASURE_TARGET} holdout sessions so far`,
      progress: Math.min(1, b.n / MEASURE_TARGET),
    };
  }
  return { label: "No data", tone: "nodata", title: "No sessions observed yet" };
}
