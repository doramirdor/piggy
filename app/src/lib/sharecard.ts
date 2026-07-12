// Share-card data → text mapping. This pure function is the single source of
// truth for every string drawn on the canvas (see sharecard-canvas.ts), so the
// honesty rules are unit-testable without a DOM:
//
//   * measured        → the proof line "measured with holdout sessions, not vibes"
//   * estimated       → the card says "estimated" and the proof line admits it
//   * not_enough_data → nothing is faked; the card says it is still measuring
//
// (docs/m4-spec.md §"Share card" + DESIGN.md positioning rule #2.)

import type { Period, ShareCardData } from "../types";
import { formatTokens } from "./format";

export interface ShareCardText {
  week: string;
  kicker: string;
  big: string;
  sub: string;
  proof: string;
  url: string;
}

const URL = "piggy.app";

function periodWord(p: Period): string {
  switch (p) {
    case "today":
      return "day";
    case "week":
      return "week";
    case "month":
      return "month";
    case "all":
      return "run";
  }
}

export function shareCardText(d: ShareCardData): ShareCardText {
  const week = d.weekLabel;

  if (d.headlineLabel === "measured" && d.tokensSaved != null) {
    const mult =
      d.multiplier != null ? `${d.multiplier.toFixed(1)}× longer` : "longer";
    return {
      week,
      kicker: "My savers banked",
      big: `${formatTokens(d.tokensSaved)} tokens`,
      sub: `this ${periodWord(d.period)} — my Claude plan lasts ${mult}`,
      proof: "measured with holdout sessions, not vibes",
      url: URL,
    };
  }

  if (d.headlineLabel === "estimated" && d.tokensSaved != null) {
    const mult =
      d.multiplier != null ? `~${d.multiplier.toFixed(1)}× longer` : "longer";
    return {
      week,
      kicker: "My savers banked (estimated)",
      big: `~${formatTokens(d.tokensSaved)} tokens`,
      sub: `this ${periodWord(d.period)} — plan lasts about ${mult}`,
      proof: "estimated from my usage history — holdout measurement in progress",
      url: URL,
    };
  }

  // not_enough_data (or missing numbers): never fabricate a savings figure.
  return {
    week,
    kicker: "My savers are warming up",
    big: "Still measuring",
    sub: "Piggy needs a few more sessions before it can prove the savings",
    proof: "no holdout data yet — nothing to fake",
    url: URL,
  };
}
