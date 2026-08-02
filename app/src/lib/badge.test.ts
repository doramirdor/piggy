import { describe, it, expect } from "vitest";
import { MEASURE_TARGET, badgeView, statusView } from "./badge";
import type { Badge } from "../types";

describe("badgeView", () => {
  it("renders a measured badge with the delta magnitude and green tone", () => {
    const b: Badge = { kind: "measured", delta: -0.22, n: 41, nOn: 41, nOff: 0 };
    const v = badgeView(b);
    expect(v.text).toBe("−22% measured");
    expect(v.tone).toBe("measured");
    expect(v.title).toContain("41 sessions");
  });

  it("singularizes the session count in the measured title", () => {
    const v = badgeView({ kind: "measured", delta: -0.09, n: 1, nOn: 1, nOff: 0 });
    expect(v.title).toContain("1 session vs");
  });

  it("falls back to measuring when a measured badge has no delta", () => {
    const v = badgeView({ kind: "measured", delta: null, n: 6, nOn: 6, nOff: 0 });
    expect(v.tone).toBe("nodata");
    expect(v.text).toBe("measuring · 6 sessions");
  });

  it("shows a '+' sign when a measured saver increased tokens", () => {
    const v = badgeView({ kind: "measured", delta: 0.05, n: 22, nOn: 22, nOff: 0 });
    expect(v.text).toBe("+5% measured");
    expect(v.tone).toBe("measured");
  });

  it("renders an estimated badge with the ≈ hedge and gray-blue tone", () => {
    const v = badgeView({ kind: "estimated", delta: -0.12, n: 15, nOn: 15, nOff: 0 });
    expect(v.text).toBe("≈ −12% estimated");
    expect(v.tone).toBe("estimated");
    expect(v.title).toContain("15 sessions");
    expect(v.title.toLowerCase()).toContain("holdout measurement in progress");
  });

  it("singularizes the session count in the estimated title", () => {
    const v = badgeView({ kind: "estimated", delta: -0.08, n: 1, nOn: 1, nOff: 0 });
    expect(v.title).toContain("1 session of");
  });

  it("falls back to measuring when an estimated badge has no delta", () => {
    const v = badgeView({ kind: "estimated", delta: null, n: 4, nOn: 4, nOff: 0 });
    expect(v.tone).toBe("nodata");
    expect(v.text).toBe("measuring · 4 sessions");
  });

  it("renders a measuring badge with the honest session progress", () => {
    const v = badgeView({ kind: "measuring", delta: null, n: 6, nOn: 6, nOff: 0 });
    expect(v.text).toBe("measuring · 6 sessions");
    expect(v.tone).toBe("nodata");
  });

  it("handles zero sessions in a measuring badge", () => {
    const v = badgeView({ kind: "measuring", delta: null, n: 0, nOn: 0, nOff: 0 });
    expect(v.text).toBe("measuring · 0 sessions");
  });

  it("renders a claimed badge labelled as a claim", () => {
    const v = badgeView({ kind: "claimed", delta: null, n: 0, nOn: 0, nOff: 0 });
    expect(v.text).toBe("author claims");
    expect(v.tone).toBe("claimed");
    expect(v.title.toLowerCase()).toContain("marketing");
  });
});

describe("statusView progress", () => {
  const measuring = (nOn: number, nOff: number): Badge => ({
    kind: "measuring",
    delta: null,
    n: nOn + nOff,
    nOn,
    nOff,
  });

  it("fills on the weaker arm, not the total", () => {
    // The regression: 14 on / 0 off summed to 14, painted a full bar, and could
    // never settle - promotion needs MEASURE_TARGET sessions on BOTH sides.
    expect(statusView(measuring(14, 0)).progress).toBe(0);
    expect(statusView(measuring(9, 9)).progress).toBeCloseTo(0.9);
    expect(statusView(measuring(40, MEASURE_TARGET)).progress).toBe(1);
  });

  it("keeps No data for a saver with nothing observed at all", () => {
    expect(statusView(measuring(0, 0)).label).toBe("No data");
    expect(statusView(measuring(1, 0)).label).toBe("Measuring");
  });
});
