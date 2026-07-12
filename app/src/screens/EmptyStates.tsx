import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import { PiggyMark } from "../components/PiggyMark";

/** Shown when Claude Code isn't installed — friendly setup card. */
export function NoClaude() {
  return (
    <div className="empty">
      <PiggyMark size={56} className="mark" />
      <div className="etitle">Piggy needs Claude Code</div>
      <div className="ebody">
        Piggy makes your Claude Code plan last longer by measuring and turning on token savers.
        Install Claude Code first, then reopen Piggy.
      </div>
      <button className="btn primary" onClick={() => void api.openExternal("https://claude.com/claude-code")}>
        How to install Claude Code
      </button>
    </div>
  );
}

/** Shown on a fresh install before any sessions are indexed. */
export function FirstRun() {
  const boot = useStore((s) => s.boot);
  const [checking, setChecking] = useState(true);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        await api.reindex();
      } catch {
        // reindex failures surface on the next manual refresh
      }
      if (!cancelled) {
        await boot();
        setChecking(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [boot]);

  const refresh = async () => {
    setChecking(true);
    await api.reindex().catch(() => {});
    await boot();
    setChecking(false);
  };

  return (
    <div className="empty">
      {checking ? <div className="spinner" /> : <PiggyMark size={56} className="mark" />}
      <div className="etitle">{checking ? "Piggy is reading your history…" : "No sessions yet"}</div>
      <div className="ebody">
        {checking
          ? "Adding up the tokens you've already spent. This only takes a moment."
          : "Run a Claude Code session, then Piggy will start counting your savings."}
      </div>
      {!checking && (
        <button className="btn primary" onClick={refresh}>
          Check again
        </button>
      )}
    </div>
  );
}
