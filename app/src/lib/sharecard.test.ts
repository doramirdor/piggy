import { describe, it, expect } from "vitest";
import { shareCardText } from "./sharecard";
import type { ShareCardData } from "../types";

const base: ShareCardData = {
  period: "week",
  weekLabel: "Jul 6 – Jul 12",
  tokensSaved: 1_200_000,
  multiplier: 1.7,
  headlineLabel: "measured",
  nHoldout: 12,
  shareable: true,
};

describe("shareCardText", () => {
  it("maps measured data to the banked-tokens headline and honest proof line", () => {
    const t = shareCardText(base);
    expect(t.week).toBe("Jul 6 – Jul 12");
    expect(t.kicker).toBe("My savers banked");
    expect(t.big).toBe("1.2M tokens");
    expect(t.sub).toBe("this week - my Claude plan lasts 1.7× longer");
    expect(t.proof).toBe("measured with holdout sessions, not vibes");
    expect(t.url).toBe("piggy.app");
  });

  it("uses the correct period word", () => {
    expect(shareCardText({ ...base, period: "month" }).sub).toContain("this month");
    expect(shareCardText({ ...base, period: "today" }).sub).toContain("this day");
  });

  it("labels estimated data as estimated in both the card and the proof line", () => {
    const t = shareCardText({
      ...base,
      headlineLabel: "estimated",
      tokensSaved: 800_000,
      multiplier: 1.4,
    });
    expect(t.kicker).toContain("estimated");
    expect(t.big).toBe("~800k tokens");
    expect(t.proof).toBe("estimated from my usage history - holdout measurement in progress");
  });

  it("never fabricates numbers when there is not enough data", () => {
    const t = shareCardText({
      ...base,
      headlineLabel: "not_enough_data",
      tokensSaved: null,
      multiplier: null,
      shareable: false,
    });
    expect(t.big).toBe("Still measuring");
    expect(t.proof).toBe("no holdout data yet - nothing to fake");
    expect(t.sub).not.toMatch(/\d/);
  });

  it("does not claim a multiplier that is missing even when measured", () => {
    const t = shareCardText({ ...base, multiplier: null });
    expect(t.sub).toBe("this week - my Claude plan lasts longer");
  });
});
