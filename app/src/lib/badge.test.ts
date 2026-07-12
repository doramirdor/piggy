import { describe, it, expect } from "vitest";
import { badgeView } from "./badge";
import type { Badge } from "../types";

describe("badgeView", () => {
  it("renders a measured badge with the delta magnitude and green tone", () => {
    const b: Badge = { kind: "measured", delta: -0.22, n: 41 };
    const v = badgeView(b);
    expect(v.text).toBe("−22% measured");
    expect(v.tone).toBe("measured");
    expect(v.title).toContain("41 sessions");
  });

  it("singularizes the session count in the measured title", () => {
    const v = badgeView({ kind: "measured", delta: -0.09, n: 1 });
    expect(v.title).toContain("1 session vs");
  });

  it("falls back to measuring when a measured badge has no delta", () => {
    const v = badgeView({ kind: "measured", delta: null, n: 6 });
    expect(v.tone).toBe("nodata");
    expect(v.text).toBe("measuring · 6 sessions");
  });

  it("renders a measuring badge with the honest session progress", () => {
    const v = badgeView({ kind: "measuring", delta: null, n: 6 });
    expect(v.text).toBe("measuring · 6 sessions");
    expect(v.tone).toBe("nodata");
  });

  it("handles zero sessions in a measuring badge", () => {
    const v = badgeView({ kind: "measuring", delta: null, n: 0 });
    expect(v.text).toBe("measuring · 0 sessions");
  });

  it("renders a claimed badge labelled as a claim", () => {
    const v = badgeView({ kind: "claimed", delta: null, n: 0 });
    expect(v.text).toBe("author claims");
    expect(v.tone).toBe("claimed");
    expect(v.title.toLowerCase()).toContain("marketing");
  });
});
