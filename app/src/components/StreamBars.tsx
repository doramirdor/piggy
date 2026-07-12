import type { Streams } from "../types";
import { formatTokens } from "../lib/format";

// Four-stream palette from the mockup. cacheRead uses a neutral gray (instead of
// the mockup's translucent white) so the swatch stays visible in light mode too.
const COLORS = {
  input: "#3b82f6",
  output: "#8b5cf6",
  cacheWrite: "#22c55e",
  cacheRead: "#6b7280",
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
