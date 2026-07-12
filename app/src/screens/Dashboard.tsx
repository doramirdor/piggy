import { useState } from "react";
import { useStore } from "../store";
import { StreamBars } from "../components/StreamBars";
import { badgeView } from "../lib/badge";
import { commafy } from "../lib/format";
import { ShareSheet } from "./ShareSheet";
import type { Period } from "../types";

const PERIODS: { key: Period; label: string }[] = [
  { key: "week", label: "7 days" },
  { key: "month", label: "30 days" },
  { key: "all", label: "All" },
];

export function Dashboard() {
  const stats = useStore((s) => s.stats);
  const savers = useStore((s) => s.savers);
  const period = useStore((s) => s.period);
  const setPeriod = useStore((s) => s.setPeriod);
  const [shareOpen, setShareOpen] = useState(false);

  const h = stats?.headline;
  const measured = h && h.label === "measured" && h.value != null;

  return (
    <div className="scroll page">
      <div className="page-title">Dashboard</div>

      <div className="period-picker">
        {PERIODS.map((p) => (
          <button
            key={p.key}
            className={period === p.key ? "active" : ""}
            onClick={() => setPeriod(p.key)}
          >
            {p.label}
          </button>
        ))}
      </div>

      <div className="dash">
        {measured ? (
          <>
            <div className="headline">Your Claude plan lasts</div>
            <div className="big">
              <em>{h!.value!.toFixed(1)}×</em> longer
            </div>
            <div className="sub">measured against {h!.nHoldout} holdout sessions</div>
          </>
        ) : (
          <>
            <div className="headline">Your Claude plan lasts</div>
            <div className="big" style={{ fontSize: 20, color: "var(--text-2)" }}>
              measuring…
            </div>
            <div className="sub">{h?.nHoldout ?? 0} of 10 holdout sessions so far — no number faked</div>
          </>
        )}
        {stats && <StreamBars streams={stats.streams} tall />}
      </div>

      {stats && (
        <div className="dash" style={{ marginTop: 0 }}>
          <div className="headline">
            {stats.sessions.toLocaleString("en-US")} sessions · {commafy(stats.totalTokens)} tokens
          </div>
          <div className="sub">
            estimated cost ${stats.costUsdEst.toFixed(2)}
            {!stats.fullyPriced && " (some tokens unpriced)"} — tokens measured, cost estimated
          </div>
        </div>
      )}

      <div className="sect">Per-saver attribution</div>
      <div className="attr">
        {savers?.savers.map((s) => {
          const v = badgeView(s.badge);
          return (
            <div className="arow" key={s.id}>
              <div className="aname">{s.plainLabel ?? s.name}</div>
              {v.tone === "measured" ? (
                <span className="adelta">{v.text.replace(" measured", "")}</span>
              ) : (
                <span className="an">{v.text}</span>
              )}
              <span className="an">n={s.badge.n}</span>
            </div>
          );
        })}
        {(!savers || savers.savers.length === 0) && (
          <div className="arow">
            <span className="an">No savers on yet.</span>
          </div>
        )}
      </div>

      <div style={{ margin: "12px 12px 0" }}>
        <button className="btn primary wide" onClick={() => setShareOpen(true)}>
          Share
        </button>
      </div>

      {shareOpen && <ShareSheet period={period} onClose={() => setShareOpen(false)} />}
    </div>
  );
}
