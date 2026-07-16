// Day-over-day usage analytics, derived from the raw `UsageSeries` points.
// Pure functions so the numbers are unit-tested and the UI stays declarative.

import type { DailyPoint, UsageSeries } from "../types";

/** Token-maximization + day-over-day rollups over a usage series. */
export interface UsageAnalysis {
  /** Total tokens across the window. */
  totalTokens: number;
  /** Days that saw at least one token (zero-filled gaps excluded). */
  activeDays: number;
  /** Mean tokens over active days (0 when there are none). */
  dailyAvg: number;
  /** The single heaviest day, or null when the window is empty. */
  busiest: DailyPoint | null;
  /**
   * Of every context token Claude ingested (input + cache write + cache read),
   * the fraction served cheaply from cache instead of re-sent. Higher = more
   * reuse = the core token-maximization lever. Null when no context tokens.
   */
  cacheHitRate: number | null;
  /** Most recent day with usage. */
  latest: DailyPoint | null;
  /** The active day before `latest` (for the day-over-day delta). */
  prev: DailyPoint | null;
  /** `latest/prev - 1`, signed; null when there isn't a prior active day. */
  trendPct: number | null;
}

const contextTokens = (p: DailyPoint) => p.input + p.cacheWrite + p.cacheRead;

export function analyzeUsage(series: UsageSeries | null): UsageAnalysis {
  const points = series?.points ?? [];
  const active = points.filter((p) => p.totalTokens > 0);

  const totalTokens = points.reduce((s, p) => s + p.totalTokens, 0);
  const dailyAvg = active.length ? Math.round(totalTokens / active.length) : 0;

  const busiest = active.reduce<DailyPoint | null>(
    (best, p) => (best == null || p.totalTokens > best.totalTokens ? p : best),
    null,
  );

  const ctx = points.reduce((s, p) => s + contextTokens(p), 0);
  const reads = points.reduce((s, p) => s + p.cacheRead, 0);
  const cacheHitRate = ctx > 0 ? reads / ctx : null;

  const latest = active.length ? active[active.length - 1] : null;
  const prev = active.length > 1 ? active[active.length - 2] : null;
  const trendPct = latest && prev && prev.totalTokens > 0
    ? latest.totalTokens / prev.totalTokens - 1
    : null;

  return { totalTokens, activeDays: active.length, dailyAvg, busiest, cacheHitRate, latest, prev, trendPct };
}

/** `2026-07-14` → `Jul 14` (UTC, no locale surprises). */
export function shortDay(iso: string): string {
  const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const [, m, d] = iso.split("-");
  const mi = Number(m) - 1;
  if (mi < 0 || mi > 11) return iso;
  return `${MONTHS[mi]} ${Number(d)}`;
}
