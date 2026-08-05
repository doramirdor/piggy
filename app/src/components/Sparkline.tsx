import { formatTokens } from "../lib/format";

interface SparklineProps {
  /** The real per-day series, oldest first. */
  points: number[];
  label: string;
}

/**
 * A row's own history, drawn from its own numbers.
 *
 * Deliberately returns nothing when the series cannot support a line. The
 * Reports screen this replaces drew its trend from the badge tone, a shape
 * derived from a colour, which is an illustration of a conclusion rather than
 * evidence for it. A sparkline here is the daily series or it is absent.
 */
export function Sparkline({ points, label }: SparklineProps) {
  // Two points make a line; one makes a dot that implies a direction it cannot
  // know. All-zero is a real answer, but it is a flat rule, not a trend.
  const max = Math.max(...points, 0);
  if (points.length < 2 || max === 0) return null;

  const w = 64;
  const h = 18;
  const step = w / (points.length - 1);
  const d = points
    .map((v, i) => `${i === 0 ? "M" : "L"}${(i * step).toFixed(1)} ${(h - (v / max) * h).toFixed(1)}`)
    .join(" ");
  const last = points[points.length - 1];

  return (
    <svg
      className="spark"
      viewBox={`0 0 ${w} ${h}`}
      width={w}
      height={h}
      role="img"
      aria-label={`${label}: ${points.length} days, peak ${formatTokens(max)}, latest ${formatTokens(last)}`}
    >
      <title>{`${points.length} days · peak ${formatTokens(max)} · latest ${formatTokens(last)}`}</title>
      <path d={d} fill="none" strokeWidth="1.25" vectorEffect="non-scaling-stroke" />
      {/* The end point, so a series ending on a quiet day still reads as
          ending rather than trailing off the edge. */}
      <circle cx={w} cy={h - (last / max) * h} r="1.75" />
    </svg>
  );
}
