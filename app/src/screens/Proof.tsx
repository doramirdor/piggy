import { useState } from "react";
import { useStore } from "../store";
import { StreamBars } from "../components/StreamBars";
import { badgeView } from "../lib/badge";
import { commafy } from "../lib/format";
import { ShareSheet } from "./ShareSheet";
import type { Period } from "../types";

const PERIODS: { key: Period; label: string }[] = [
  { key: "today", label: "Day" },
  { key: "week", label: "Week" },
  { key: "month", label: "Month" },
  { key: "all", label: "All" },
];

export function Proof() {
  const stats = useStore((s) => s.stats);
  const savers = useStore((s) => s.savers);
  const period = useStore((s) => s.period);
  const setPeriod = useStore((s) => s.setPeriod);
  const [shareOpen, setShareOpen] = useState(false);

  const h = stats?.headline;
  const measured = h && h.label === "measured" && h.value != null;
  const estimated = h && h.label === "estimated" && h.value != null;

  return (
    <>
      <div className="head">
        <div>
          <h1>Proof</h1>
          <div className="sub">Holdout sessions show what Piggy really saved — measured, not vibes.</div>
        </div>
        <button className="btn primary" onClick={() => setShareOpen(true)}>
          Share
        </button>
      </div>

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

      <div className="hero">
        <div className="eyebrow">Your Claude plan lasts</div>
        {measured ? (
          <div className="big">
            <em>{h!.value!.toFixed(1)}×</em> longer
          </div>
        ) : estimated ? (
          <div className="big">
            <em>~{h!.value!.toFixed(1)}×</em> longer
          </div>
        ) : (
          <div className="big measuring">measuring…</div>
        )}
        <div className="sub">
          {measured
            ? `measured against ${h!.nHoldout} holdout sessions`
            : estimated
              ? "estimated vs your history · holdout measurement in progress"
              : `${h?.nHoldout ?? 0} of 10 holdout sessions so far — no number faked`}
        </div>
        {stats && <StreamBars streams={stats.streams} tall />}
      </div>

      {stats && (
        <div className="metric-grid">
          <div className="metric">
            <small>Total this period</small>
            <strong>{commafy(stats.totalTokens)}</strong>
            <p>
              {stats.sessions.toLocaleString("en-US")} sessions · <span className="meas">tokens measured</span>
            </p>
          </div>
          <div className="metric">
            <small>Estimated cost</small>
            <strong>${stats.costUsdEst.toFixed(2)}</strong>
            <p>
              <span className="est">estimated</span>
              {stats.fullyPriced ? " from plan pricing" : " (some tokens unpriced)"}
            </p>
          </div>
        </div>
      )}

      <div className="sect">Per-saver attribution</div>
      <div className="attr">
        {savers?.savers.map((s) => {
          const v = badgeView(s.badge);
          const hasDelta = v.tone === "measured" || v.tone === "estimated";
          return (
            <div className="arow" key={s.id}>
              <div className="aname">{s.plainLabel ?? s.name}</div>
              {hasDelta ? (
                <span className={`adelta${v.tone === "estimated" ? " est" : ""}`}>
                  {v.text.replace(/ (measured|estimated)$/, "")}
                </span>
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

      {shareOpen && <ShareSheet period={period} onClose={() => setShareOpen(false)} />}
    </>
  );
}
