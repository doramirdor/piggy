import type { UsageSeries } from "../types";
import { analyzeUsage, shortDay } from "../lib/usage";
import { formatTokens } from "../lib/format";

// Same four-stream palette as StreamBars, so a day's bar reads the same as the
// hero's stacked totals. cacheRead is the neutral gray — the cheap, reused
// portion the analysis highlights.
const COLORS = {
  input: "#3b82f6",
  output: "#8b5cf6",
  cacheWrite: "#22c55e",
  cacheRead: "#6b7280",
};

// Bottom→top stack order matches the StreamBars legend.
const STACK = [
  { key: "cacheRead", color: COLORS.cacheRead },
  { key: "cacheWrite", color: COLORS.cacheWrite },
  { key: "output", color: COLORS.output },
  { key: "input", color: COLORS.input },
] as const;

/**
 * Day-over-day stacked bar chart. Pure inline SVG (no chart lib, CSP-safe),
 * stretched to the container width via a non-uniform viewBox; day labels and the
 * average line live in the same coordinate space. Each bar has a native tooltip.
 */
export function UsageChart({ series }: { series: UsageSeries }) {
  const points = series.points;
  const n = points.length;
  const H = 100;
  const max = Math.max(1, ...points.map((p) => p.totalTokens));
  const { dailyAvg } = analyzeUsage(series);
  const avgY = dailyAvg > 0 ? H - (dailyAvg / max) * H : null;

  const slot = 100 / Math.max(1, n);
  const barW = Math.min(slot * 0.68, 6);

  // Sparse x labels: first, middle, last active/edge days — enough to orient
  // without crowding a 30- or 120-day window.
  const labelIdx = new Set([0, Math.floor((n - 1) / 2), n - 1].filter((i) => i >= 0));

  return (
    <div className="uchart">
      <svg viewBox={`0 0 100 ${H}`} preserveAspectRatio="none" className="uchart-svg" aria-hidden>
        {avgY != null && (
          <line x1="0" y1={avgY} x2="100" y2={avgY} className="uchart-avg" />
        )}
        {points.map((p, i) => {
          const cx = i * slot + slot / 2;
          const x = cx - barW / 2;
          let yTop = H;
          const segs = STACK.map((s) => {
            const v = p[s.key];
            const h = (v / max) * H;
            yTop -= h;
            return { color: s.color, y: yTop, h };
          });
          const idle = p.totalTokens === 0;
          return (
            <g key={p.date}>
              {idle ? (
                // A zero day still gets a faint tick so the gap is legible.
                <rect x={x} y={H - 0.8} width={barW} height={0.8} className="uchart-zero" />
              ) : (
                segs.map((seg, si) =>
                  seg.h > 0 ? (
                    <rect key={si} x={x} y={seg.y} width={barW} height={seg.h} fill={seg.color} />
                  ) : null,
                )
              )}
              <rect x={i * slot} y={0} width={slot} height={H} fill="transparent">
                <title>{`${shortDay(p.date)} · ${formatTokens(p.totalTokens)} tokens · ${p.sessions} session${p.sessions === 1 ? "" : "s"}`}</title>
              </rect>
            </g>
          );
        })}
      </svg>
      <div className="uchart-x">
        {points.map((p, i) => (
          <span key={p.date} className={labelIdx.has(i) ? "" : "hide"}>
            {labelIdx.has(i) ? shortDay(p.date) : ""}
          </span>
        ))}
      </div>
      <div className="uchart-legend">
        <span><i style={{ background: COLORS.input }} />input</span>
        <span><i style={{ background: COLORS.output }} />output</span>
        <span><i style={{ background: COLORS.cacheWrite }} />cache write</span>
        <span><i style={{ background: COLORS.cacheRead }} />cache read</span>
        {avgY != null && <span className="uchart-avg-key"><i />daily avg</span>}
      </div>
    </div>
  );
}
