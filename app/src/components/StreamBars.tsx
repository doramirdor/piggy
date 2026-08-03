import type { Streams } from "../types";
import { formatTokens } from "../lib/format";

// Four-stream palette from the mockup. cacheRead uses a neutral gray (instead of
// the mockup's translucent white) so the swatch stays visible in light mode too.
const COLORS = {
  // The four streams are CATEGORIES, not verdicts, so they take the neutral
  // ramp. The old palette spent a saturated green on cache write, which is a
  // quantity nobody proved anything about.
  input: "var(--cat-1)",
  output: "var(--cat-2)",
  cacheWrite: "var(--cat-warm)",
  cacheRead: "var(--cat-4)",
};

export function StreamBars({ streams, tall }: { streams: Streams; tall?: boolean }) {
  const total =
    streams.input + streams.output + streams.cacheWrite + streams.cacheRead || 1;
  const w = (n: number) => `${(n / total) * 100}%`;
  return (
    <>
      <div className={`bars ${tall ? "tall" : ""}`}>
        <div style={{ width: w(streams.input), background: COLORS.input }} />
        <div style={{ width: w(streams.output), background: COLORS.output }} />
        <div style={{ width: w(streams.cacheWrite), background: COLORS.cacheWrite }} />
        <div style={{ width: w(streams.cacheRead), background: COLORS.cacheRead }} />
      </div>
      <div className="legend">
        <span>
          <i style={{ background: COLORS.input }} />
          input {formatTokens(streams.input)}
        </span>
        <span>
          <i style={{ background: COLORS.output }} />
          output {formatTokens(streams.output)}
        </span>
        <span>
          <i style={{ background: COLORS.cacheWrite }} />
          cache write {formatTokens(streams.cacheWrite)}
        </span>
        <span>
          <i style={{ background: COLORS.cacheRead }} />
          cache read {formatTokens(streams.cacheRead)}
        </span>
      </div>
    </>
  );
}
