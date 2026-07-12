import { useEffect, useState } from "react";
import { useStore } from "../store";
import { api } from "../ipc";
import { StreamBars } from "../components/StreamBars";
import { SaverIcon } from "../components/SaverIcon";
import { badgeView } from "../lib/badge";
import { formatTokens, commafy } from "../lib/format";
import { SweepSheet } from "./SweepSheet";
import type { SaverRow, SweepReport } from "../types";

/** Time-of-day greeting (no name — Piggy doesn't know it and won't invent one). */
function greeting(): string {
  const h = new Date().getHours();
  if (h < 12) return "Good morning";
  if (h < 18) return "Good afternoon";
  return "Good evening";
}

/** One "recent proof" row per saver that has actually produced a number. */
function ProofRow({ saver }: { saver: SaverRow }) {
  const v = badgeView(saver.badge);
  const measured = v.tone === "measured";
  const estimated = v.tone === "estimated";
  const delta = saver.badge.delta;
  const detail = measured
    ? `measured across ${saver.badge.n} sessions`
    : `estimated vs your history · ${saver.badge.n} sessions`;
  return (
    <div className="feed-row">
      <SaverIcon id={saver.id} size={32} />
      <div>
        <b>{saver.plainLabel ?? saver.name}</b>
        <small>{detail}</small>
      </div>
      <em className={estimated ? "est" : ""}>
        {delta != null ? `${delta > 0 ? "+" : "−"}${Math.round(Math.abs(delta) * 100)}%` : "—"}
      </em>
    </div>
  );
}

export function Overview() {
  const stats = useStore((s) => s.stats);
  const savers = useStore((s) => s.savers);
  const showError = useStore((s) => s.showError);

  const [sweep, setSweep] = useState<SweepReport | null>(null);
  const [sweepOpen, setSweepOpen] = useState(false);

  useEffect(() => {
    api.sweepReport().then(setSweep).catch((e) => showError(e));
  }, [showError, sweepOpen]);

  const h = stats?.headline;
  const measured = h && h.label === "measured" && h.value != null;
  const estimated = h && h.label === "estimated" && h.value != null;
  const mult = measured || estimated ? h!.value! : null;

  // Savings derived from the multiplier: to do the work you did (totalTokens,
  // measured) without savers you'd have spent mult× as much, so saved =
  // total×(mult−1). Tokens are measured; cost is always an estimate.
  const savedTokens = mult && stats ? stats.totalTokens * (mult - 1) : null;
  const savedPct = mult ? Math.round((1 - 1 / mult) * 100) : null;
  const moneyAvoided = mult && stats ? stats.costUsdEst * (mult - 1) : null;

  const proofSavers = (savers?.savers ?? []).filter((s) => {
    const t = badgeView(s.badge).tone;
    return t === "measured" || t === "estimated";
  });

  const recommended = sweep?.items.filter((i) => i.recommendDisable) ?? [];

  return (
    <>
      <div className="head">
        <div>
          <h1>{greeting()}</h1>
          <div className="sub">
            {stats
              ? `Piggy saw ${commafy(stats.totalTokens)} tokens across ${stats.sessions} sessions ${stats.periodLabel.toLowerCase()}.`
              : "Reading your Claude Code history…"}
          </div>
        </div>
        {stats && (
          <div className="today">
            today <b>{formatTokens(stats.todayTokens)}</b> tokens
            {savedPct != null && (
              <>
                {" · saved "}
                <b className="green">{savedPct}%</b>
              </>
            )}
          </div>
        )}
      </div>

      <div className="hero">
        <div className="hero-top">
          <div>
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
          </div>
          {savedPct != null && <span className="delta">saved {savedPct}%</span>}
        </div>
        <div className="sub">
          {measured
            ? `measured against ${h!.nHoldout} holdout sessions · ${stats!.periodLabel.toLowerCase()}`
            : estimated
              ? "estimated vs your history · holdout measurement in progress"
              : `${h?.nHoldout ?? 0} of 10 holdout sessions so far — no number faked`}
        </div>
        {stats && <StreamBars streams={stats.streams} tall />}
      </div>

      <div className="metric-grid">
        <div className="metric">
          <small>Tokens saved</small>
          <strong>{savedTokens != null ? formatTokens(savedTokens) : "—"}</strong>
          <p>
            {savedPct != null ? (
              <>
                {savedPct}% of what you'd have spent · <span className="meas">measured</span>
              </>
            ) : (
              "measuring your first holdout"
            )}
          </p>
        </div>
        <div className="metric">
          <small>Money avoided</small>
          <strong>{moneyAvoided != null ? `$${moneyAvoided.toFixed(2)}` : "—"}</strong>
          <p>
            <span className="est">estimated</span> from your plan pricing
          </p>
        </div>
      </div>

      {recommended.length > 0 && sweep && (
        <div className="hint">
          <div className="t">
            <b>
              {recommended.length} add-on{recommended.length === 1 ? "" : "s"} you never use
            </b>{" "}
            {recommended.length === 1 ? "is" : "are"} costing ~
            {formatTokens(sweep.estRecoverableTokens)} tokens per request. <small>estimated</small>
          </div>
          <button className="btn" onClick={() => setSweepOpen(true)}>
            Review
          </button>
        </div>
      )}

      <div className="sect">Recent proof</div>
      {proofSavers.length > 0 ? (
        <div className="feed">
          {proofSavers.map((s) => (
            <ProofRow key={s.id} saver={s} />
          ))}
        </div>
      ) : (
        <div className="foot-note" style={{ marginTop: 0 }}>
          No measured savings yet — Piggy is still gathering honest holdout data. Turn on a saver
          in the Savers tab to get started.
        </div>
      )}

      {sweepOpen && <SweepSheet onClose={() => setSweepOpen(false)} />}
    </>
  );
}
