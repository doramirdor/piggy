import { useStore } from "../store";
import { formatTokens } from "../lib/format";

/** Persistent panel header: the Piggy wordmark + today's tokens / measured savings. */
export function Header() {
  const stats = useStore((s) => s.stats);

  let today: string | null = null;
  let savedPct: number | null = null;
  if (stats) {
    today = formatTokens(stats.todayTokens);
    const h = stats.headline;
    // Only show a savings % when it is genuinely measured (never fabricated).
    if (h.label === "measured" && h.value != null && h.value > 0) {
      savedPct = Math.round((1 - 1 / h.value) * 100);
    }
  }

  return (
    <header className="header">
      <div className="logo">
        <span>🐷</span>Piggy
      </div>
      {today != null && (
        <div className="today">
          today <b>{today}</b> tokens
          {savedPct != null && (
            <>
              {" · saved "}
              <b className="green">{savedPct}%</b>
            </>
          )}
        </div>
      )}
    </header>
  );
}
