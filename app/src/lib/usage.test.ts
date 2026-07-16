import { describe, it, expect } from "vitest";
import { analyzeUsage, shortDay } from "./usage";
import type { DailyPoint, UsageSeries } from "../types";

function pt(date: string, input: number, output: number, cacheWrite: number, cacheRead: number, sessions = 1): DailyPoint {
  return { date, input, output, cacheWrite, cacheRead, sessions, totalTokens: input + output + cacheWrite + cacheRead, costUsdEst: 0 };
}

function series(points: DailyPoint[]): UsageSeries {
  return { period: "week", periodLabel: "Last 7 days", points };
}

describe("analyzeUsage", () => {
  it("ignores zero-filled gap days for active-day math", () => {
    const a = analyzeUsage(series([
      pt("2026-07-10", 100, 40, 60, 300),
      pt("2026-07-11", 0, 0, 0, 0), // gap
      pt("2026-07-12", 200, 60, 40, 100),
    ]));
    expect(a.activeDays).toBe(2);
    expect(a.totalTokens).toBe(500 + 400);
    expect(a.dailyAvg).toBe(450); // over active days only
    expect(a.busiest?.date).toBe("2026-07-10"); // 500 > 400
  });

  it("computes cache hit rate as reads over all context tokens", () => {
    const a = analyzeUsage(series([pt("2026-07-12", 100, 999, 100, 300)]));
    // reads 300 / (input 100 + write 100 + read 300) = 0.6; output is not context
    expect(a.cacheHitRate).toBeCloseTo(0.6, 5);
  });

  it("day-over-day trend compares the two most recent active days", () => {
    const a = analyzeUsage(series([
      pt("2026-07-10", 100, 0, 0, 0), // prev active = 100
      pt("2026-07-11", 0, 0, 0, 0), // gap skipped
      pt("2026-07-12", 150, 0, 0, 0), // latest active = 150
    ]));
    expect(a.trendPct).toBeCloseTo(0.5, 5); // +50%
    expect(a.latest?.date).toBe("2026-07-12");
    expect(a.prev?.date).toBe("2026-07-10");
  });

  it("is null-safe on an empty or all-zero series", () => {
    const a = analyzeUsage(series([pt("2026-07-12", 0, 0, 0, 0)]));
    expect(a.activeDays).toBe(0);
    expect(a.busiest).toBeNull();
    expect(a.cacheHitRate).toBeNull();
    expect(a.trendPct).toBeNull();
    expect(analyzeUsage(null).totalTokens).toBe(0);
  });
});

describe("shortDay", () => {
  it("formats an ISO date as a compact month/day", () => {
    expect(shortDay("2026-07-14")).toBe("Jul 14");
    expect(shortDay("2026-01-01")).toBe("Jan 1");
  });
});
