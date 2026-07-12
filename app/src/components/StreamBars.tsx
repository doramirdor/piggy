import type { Streams } from "../types";
import { formatTokens } from "../lib/format";

// Four-stream palette from the mockup. cacheRead uses a neutral gray (instead of
// the mockup's translucent white) so the swatch stays visible in light mode too.
const COLORS = {
  input: "#0a84ff",
  output: "#5e5ce6",
  cacheWrite: "#30d158",
  cacheRead: "#8e8e93",
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
