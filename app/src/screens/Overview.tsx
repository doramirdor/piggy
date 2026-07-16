import { useEffect, useState } from "react";
import { useStore } from "../store";
import { api } from "../ipc";
import { StreamBars } from "../components/StreamBars";
import { SourceGrid } from "../components/SourceGrid";
import { SaverIcon } from "../components/SaverIcon";
import { CopyCmd } from "../components/CopyCmd";
import { badgeView } from "../lib/badge";
import { formatTokens, commafy } from "../lib/format";
import { SweepSheet } from "./SweepSheet";
import type { SaverRow, SweepReport } from "../types";

/** Time-of-day greeting (no name - Piggy doesn't know it and won't invent one). */
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
        <b>{saver.name}</b>
        <small>{detail}</small>
      </div>
      <em className={estimated ? "est" : ""}>
        {delta != null ? `${delta > 0 ? "+" : "−"}${Math.round(Math.abs(delta) * 100)}%` : "-"}
      </em>
    </div>
  );
}

export function Overview() {
  const stats = useStore((s) => s.stats);
  const sources = useStore((s) => s.sources);
  const savers = useStore((s) => s.savers);
  const setTab = useStore((s) => s.setTab);
  const showError = useStore((s) => s.showError);

  const [sweep, setSweep] = useState<SweepReport | null>(null);
  const [sweepOpen, setSweepOpen] = useState(false);

  useEffect(() => {
    api.sweepReport().then(setSweep).catch((e) => showError(e));
  }, [showError, sweepOpen]);

  // Live gate: a saving figure is only shown when at least one saver is
  // actually enabled right now. When Piggy is off nothing is being saved, so the
  // dashboard shows the honest "off" state instead of a stale historical number.
  const live = (savers?.savers ?? []).some((s) => s.enabled);
  const off = savers != null && !live;
  const h = stats?.headline;
  const measured = live && h && h.label === "measured" && h.value != null;
  const estimated = live && h && h.label === "estimated" && h.value != null;
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

  // Wrapper-model savers that are on but have produced no delta yet: the user
  // may still be launching plain claude, which those savers never touch - keep
  // the launch instruction in view until real numbers arrive.
  const launchSavers = (savers?.savers ?? []).filter(
    (s) => s.enabled && s.launchCommand && s.badge.delta == null,
  );

  const recommended = sweep?.items.filter((i) => i.recommendDisable) ?? [];

  return (
    <>
      <div className="head">
        <div>
          <h1>{greeting()}</h1>
          <div className="sub">
            {stats
              ? `Piggy saw ${commafy(stats.totalTokens)} tokens across ${stats.sessions} sessions ${stats.periodLabel.toLowerCase()}.`
              : "Reading your session history…"}
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

      <div className={`hero ${off ? "off" : ""}`}>
        <div className="hero-top">
          <div>
            <div className="eyebrow">{off ? "Savings are live only" : "Your Claude plan lasts"}</div>
            {measured ? (
              <div className="big">
                <em>{h!.value!.toFixed(1)}×</em> longer
              </div>
            ) : estimated ? (
              <div className="big">
                <em>~{h!.value!.toFixed(1)}×</em> longer
              </div>
            ) : off ? (
              <div className="big measuring">Piggy is off</div>
            ) : (
              <div className="big measuring">measuring…</div>
            )}
          </div>
          {savedPct != null && <span className="delta">saved {savedPct}%</span>}
          {off && (
            <button className="btn" onClick={() => setTab("savers")}>
              Turn on
            </button>
          )}
        </div>
        <div className="sub">
          {measured
            ? `measured against ${h!.nHoldout} holdout sessions · ${stats!.periodLabel.toLowerCase()}`
            : estimated
              ? "estimated vs your history · holdout measurement in progress"
              : off
                ? "No savers are on, so nothing is saving right now. Turn one on to start banking tokens."
                : `${h?.nHoldout ?? 0} of 10 holdout sessions so far - no number faked`}
        </div>
        {stats && <StreamBars streams={stats.streams} tall />}
      </div>

      {launchSavers.map((s) => (
        <div className="hint launch" key={s.id}>
          <div className="t">
            <b>{s.name}</b> is on, but it only saves in sessions you start with{" "}
            <CopyCmd cmd={s.launchCommand!} />.{" "}
            <small>Start Claude with it to see measured savings here.</small>
          </div>
        </div>
      ))}

      {live && (
        <div className="metric-grid">
          <div className="metric">
            <small>Tokens saved</small>
            <strong>{savedTokens != null ? formatTokens(savedTokens) : "-"}</strong>
            <p>
              {savedPct != null ? (
                <>
                  {savedPct}% of what you'd have spent ·{" "}
                  {measured ? (
                    <span className="meas">measured</span>
                  ) : (
                    <span className="est">estimated</span>
                  )}
                </>
              ) : (
                "measuring your first holdout"
              )}
            </p>
          </div>
          <div className="metric">
            <small>Money avoided</small>
            <strong>{moneyAvoided != null ? `$${moneyAvoided.toFixed(2)}` : "-"}</strong>
            <p>
              <span className="est">estimated</span> from your plan pricing
            </p>
          </div>
        </div>
      )}

      {sources && (
        <>
          <div className="sect">
            Across your tools
            <span className="sect-sub">
              measured from each tool's own session logs · {stats?.periodLabel.toLowerCase()}
            </span>
          </div>
          <SourceGrid sources={sources} />
        </>
      )}

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
          No measured savings yet - Piggy is still gathering honest holdout data. Turn on a saver
          in the Savers tab to get started.
        </div>
      )}

      {sweepOpen && <SweepSheet onClose={() => setSweepOpen(false)} />}
    </>
  );
}
