import { formatTokens } from "../lib/format";
import type { SourceCell, SourcesOverview } from "../types";

/** Small line glyphs in the SF-Symbols spirit: a window for GUI surfaces, a
 *  terminal prompt for TUI ones. */
function SurfaceIcon({ kind }: { kind: "gui" | "tui" }) {
  if (kind === "gui") {
    return (
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
        <rect x="3" y="4.5" width="18" height="15" rx="3" />
        <path d="M3 9h18" />
        <circle cx="6.2" cy="6.8" r="0.5" fill="currentColor" />
        <circle cx="8.6" cy="6.8" r="0.5" fill="currentColor" />
      </svg>
    );
  }
  return (
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <rect x="3" y="4.5" width="18" height="15" rx="3" />
      <path d="M7 9.5l3 2.5-3 2.5" />
      <path d="M12.5 14.5H17" />
    </svg>
  );
}

const TOOL_NAME: Record<SourceCell["source"], string> = {
  "claude-code": "Claude Code",
  codex: "Codex",
};

const SURFACE_NAME: Record<SourceCell["interface"], string> = {
  gui: "App",
  tui: "Terminal",
};

function Cell({ cell }: { cell: SourceCell }) {
  const hasData = cell.sessions > 0;
  const idle = cell.toolPresent && !hasData;
  return (
    <div className={`src-cell ${hasData ? "" : "quiet"}`}>
      <div className="src-head">
        <span className={`src-ic ${cell.source === "codex" ? "codex" : "claude"}`}>
          <SurfaceIcon kind={cell.interface} />
        </span>
        <div className="src-name">
          <b>{TOOL_NAME[cell.source]}</b>
          <small>{SURFACE_NAME[cell.interface]}</small>
        </div>
      </div>
      {hasData ? (
        <>
          <strong>{formatTokens(cell.totalTokens)}</strong>
          <p>
            {cell.sessions} session{cell.sessions === 1 ? "" : "s"} · ${cell.costUsdEst.toFixed(2)}{" "}
            <span className="est">estimated</span>
          </p>
        </>
      ) : (
        <>
          <strong className="none">–</strong>
          <p>{idle ? "no sessions in this period" : "not detected on this Mac"}</p>
        </>
      )}
    </div>
  );
}

/** The per-tool observability grid: Claude Code and Codex, each split into the
 *  desktop/IDE app surface and the terminal. Token counts are measured from
 *  each tool's own session logs; costs are always labeled estimates. */
export function SourceGrid({ sources }: { sources: SourcesOverview }) {
  return (
    <>
      <div className="src-grid">
        {sources.cells.map((c) => (
          <Cell key={`${c.source}-${c.interface}`} cell={c} />
        ))}
      </div>
      {sources.unknownSessions > 0 && (
        <div className="foot-note" style={{ marginTop: 8 }}>
          {formatTokens(sources.unknownTokens)} tokens across {sources.unknownSessions} session
          {sources.unknownSessions === 1 ? "" : "s"} couldn't be matched to a surface (older logs
          without a client marker).
        </div>
      )}
    </>
  );
}
