// What one MCP server's row in Settings says, as a pure function.
//
// The whole point of this file is section 1.3 of the spec: a probe row's figures
// do NOT share one label. The schema bytes are a real byte count of what the
// server sent; turning those bytes into tokens is a separate step, and the
// shipped tokenizer divides by 3.5. Reading one label off the other printed
// every probed row as an exact count it never was.

import { commafy } from "./format";
import type { ProbeServer } from "../types";

/** The engine's own basis words, so the chip renders through `lib/advice.ts`
 *  unchanged. Never a synonym: these strings are compared, not just printed. */
export const BASIS_MANIFEST = "measured manifest";
export const BASIS_ESTIMATED = "estimated";

export interface ProbeFig {
  label: string;
  /** Already formatted for display. */
  value: string;
  /** One of `piggy_core::advice::basis`. */
  basis: string;
}

export interface ProbeRowView {
  /** The row's explanation, one sentence. */
  note: string;
  /** The button's label, or null when there is no button because there is
   *  nothing Piggy will run. */
  action: string | null;
  /** Empty unless there are figures that describe what runs today. */
  figures: ProbeFig[];
}

export function probeRow(s: ProbeServer): ProbeRowView {
  switch (s.measurement) {
    case "measured":
      return {
        note: `Measured ${s.measuredAt ?? "earlier"}.`,
        action: "Measure again",
        figures: measuredFigures(s),
      };

    // A stale row's stored numbers describe a command that is not what runs
    // today. There is no label under which showing them is true, so the row
    // shows the explanation and the button and nothing else.
    case "stale":
      return {
        note:
          "This server's command changed since Piggy measured it, so the old numbers do not " +
          "describe what runs today.",
        action: "Measure again",
        figures: [],
      };

    case "failed":
      return {
        note: s.measuredAt
          ? `Piggy started it on ${s.measuredAt} and ${failureReason(s)}.`
          : `Piggy started it and ${failureReason(s)}.`,
        action: "Try again",
        figures: [],
      };

    // http/sse. No button, because there is nothing Piggy will run.
    case "deferred":
      return {
        note:
          "Piggy does not measure http servers yet, because signing in as you is a different " +
          "problem. Sweep keeps its estimate for this one.",
        action: null,
        figures: [],
      };

    default:
      return {
        note: "Not measured yet. Sweep is using a size estimate for this one.",
        action: "Measure",
        figures: [],
      };
  }
}

/** The three figures a measured row shows, each with its own basis. */
function measuredFigures(s: ProbeServer): ProbeFig[] {
  const out: ProbeFig[] = [];
  if (s.toolCount != null) {
    out.push({
      label: "tools",
      value: commafy(s.toolCount),
      basis: BASIS_MANIFEST,
    });
  }
  if (s.schemaBytes != null) {
    out.push({
      label: "schema bytes",
      value: commafy(s.schemaBytes),
      basis: BASIS_MANIFEST,
    });
  }
  if (s.schemaTokens != null) {
    // The one figure whose label depends on the tokenizer rather than on the
    // measurement: the bytes above are real either way.
    out.push({
      label: "tokens",
      value: `~${commafy(s.schemaTokens)}`,
      basis: s.tokensEstimated ? BASIS_ESTIMATED : BASIS_MANIFEST,
    });
  }
  return out;
}

/** The probe's own sentence fragment, already redacted. Printed, not rewritten. */
function failureReason(s: ProbeServer): string {
  return s.error ?? "recorded no reason";
}
