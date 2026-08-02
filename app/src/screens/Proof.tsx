import { useState } from "react";
import { useStore } from "../store";
import { StatusChip } from "../components/StatusChip";
import { badgeView } from "../lib/badge";
import { proofView, type ProofArm } from "../lib/proof";
import { commafy, pctMagnitude } from "../lib/format";
import { useStampOnce } from "../lib/motion";
import { ShareSheet } from "./ShareSheet";
import type { Badge, HeadlineStream, Period, SaverRow } from "../types";

const PERIODS: { key: Period; label: string }[] = [
  { key: "today", label: "Day" },
  { key: "week", label: "Week" },
  { key: "month", label: "Month" },
  { key: "all", label: "All" },
];

/** A signed delta, in the app's negative-is-a-saving convention. */
function signed(delta: number): string {
  return `${delta > 0 ? "+" : "−"}${pctMagnitude(delta)}`;
}

/** One arm of the experiment: how many sessions it has, and whether that is
 *  enough. Two of these side by side are the whole story when the headline is
 *  stuck - one full rail and one empty one says "Piggy has half a comparison"
 *  faster than any sentence can. */
function Arm({ arm }: { arm: ProofArm }) {
  const sessions = `${commafy(arm.n)} session${arm.n === 1 ? "" : "s"}`;
  // "N of 10" is only meaningful while waiting is the fix. An arm that is
  // unusable for a reason waiting cannot solve just states its size and lets
  // `qual` say why the size is beside the point.
  const count = arm.state === "short" ? `${commafy(arm.n)} of ${arm.target}` : sessions;
  return (
    <div className="arm">
      <div className="arm-top">
        <span className="anm">{arm.label}</span>
        <span className="aq">{arm.qual}</span>
        <span className={`acnt ${arm.state}`}>{count}</span>
      </div>
      <div
        className={`arm-bar ${arm.state}`}
        role="progressbar"
        aria-label={`${arm.label}: ${sessions}${arm.state === "ready" ? "" : ", not usable yet"}`}
        aria-valuemin={0}
        aria-valuemax={arm.target}
        aria-valuenow={Math.min(arm.n, arm.target)}
      >
        {/* Hatched rather than filled when the sessions exist but cannot count:
            a solid bar at full width would read as "done". An arm with no
            sessions gets no sliver at all - "almost nothing" and "nothing at
            all" are different states, and only one of them is the blocker. */}
        {arm.state === "unusable" ? (
          <i className="ghost" />
        ) : (
          arm.n > 0 && <i style={{ width: `${Math.max(Math.min(1, arm.n / arm.target) * 100, 3)}%` }} />
        )}
      </div>
    </div>
  );
}

/** One stream's side-by-side comparison: the two medians the delta is a ratio
 *  of. These are the only numbers on this screen with no pricing in them, which
 *  is why they sit above the × rather than below it (docs/measurement.md).
 *
 *  Bars are scaled within the row, not across rows: cache write runs two orders
 *  of magnitude above input, so a shared scale would flatten three of the four
 *  rows into invisible slivers and hide the very comparison the row exists for. */
function StreamRow({ s }: { s: HeadlineStream }) {
  const badge: Badge = {
    kind: s.kind,
    delta: s.delta,
    n: s.nOn + s.nOff,
    nOn: s.nOn,
    nOff: s.nOff,
  };
  const max = Math.max(s.medianOff, s.medianOn);
  const width = (v: number) => (max > 0 ? `${Math.max((v / max) * 100, 2)}%` : "0%");
  // Green is a claim, not a row colour. Cache write currently runs HIGHER with
  // savers on than off, and painting that bar green would render a regression
  // as a saving on the one screen that exists to not do that. A tie is neither:
  // it gets the same neutral as the OFF arm, because amber on "identical" reads
  // as a warning about nothing.
  const dir =
    s.nOn === 0 || s.nOff === 0 || s.medianOn === s.medianOff
      ? "same"
      : s.medianOn < s.medianOff
        ? "on"
        : "worse";
  return (
    <div className="srow">
      <div className="srow-top">
        <span className="snm">{s.stream}</span>
        {s.delta != null && (
          <span className={`adelta${s.kind === "estimated" ? " est" : ""}`}>{signed(s.delta)}</span>
        )}
        <StatusChip badge={badge} />
      </div>
      <div className="sbars">
        <div className="sbar">
          <span className="sb-lab">off</span>
          <span className="sb-track">
            {s.nOff > 0 ? <i className="off" style={{ width: width(s.medianOff) }} /> : <i className="ghost" />}
          </span>
          <span className={`sb-val${s.nOff > 0 ? "" : " an"}`}>
            {s.nOff > 0 ? commafy(s.medianOff) : "no sessions"}
          </span>
        </div>
        <div className="sbar">
          <span className="sb-lab">on</span>
          <span className="sb-track">
            {s.nOn > 0 ? (
              <i className={dir} style={{ width: width(s.medianOn) }} />
            ) : (
              <i className="ghost" />
            )}
          </span>
          <span className={`sb-val${s.nOn > 0 ? "" : " an"}`}>
            {s.nOn > 0 ? commafy(s.medianOn) : "no sessions"}
          </span>
        </div>
      </div>
    </div>
  );
}

/** Per-saver evidence: both arms and the state, never a number without its n. */
function SaverRowEvidence({ saver }: { saver: SaverRow }) {
  const v = badgeView(saver.badge);
  const hasDelta = (v.tone === "measured" || v.tone === "estimated") && saver.badge.delta != null;
  return (
    <div className="arow">
      <div className="aname">{saver.name}</div>
      <span className="an">
        {commafy(saver.badge.nOn)} on · {commafy(saver.badge.nOff)} off
      </span>
      {hasDelta && (
        <span className={`adelta${v.tone === "estimated" ? " est" : ""}`}>
          {signed(saver.badge.delta!)}
        </span>
      )}
      <StatusChip badge={saver.badge} />
    </div>
  );
}

export function Proof() {
  const stats = useStore((s) => s.stats);
  const savers = useStore((s) => s.savers);
  const period = useStore((s) => s.period);
  const setPeriod = useStore((s) => s.setPeriod);
  const setTab = useStore((s) => s.setTab);
  const unpin = useStore((s) => s.unpinSaver);
  const busySavers = useStore((s) => s.busySavers);
  const [shareOpen, setShareOpen] = useState(false);

  const rows = savers?.savers ?? [];
  const view = proofView(stats?.headline ?? null, rows);
  // THE STAMP. Keyed by the identity of the claim, not by mount: the verdict
  // presses on the first time a given claim is earned and never again. Period
  // plus tone plus the value is enough to tell one claim from the next, and the
  // flag lives outside React because Proof remounts on every tab switch and
  // re-renders every few seconds off the session watcher.
  const claimId =
    view && view.tone !== "waiting" && view.multiplier != null
      ? `${period}:${view.tone}:${view.multiplier.toFixed(2)}`
      : null;
  const pressStamp = useStampOnce(claimId);
  // Cache read is excluded from the price-weighted spend the × is built on
  // (docs/measurement.md), so it is not evidence for the claim this screen
  // makes. The payload still carries it for anything that wants all four.
  const streams = (stats?.headline.streams ?? []).filter((s) => s.stream !== "cache read");
  const blocker = view?.blocker ?? null;
  const unpinIds = blocker?.unpin ?? [];
  const busy = unpinIds.some((id) => busySavers.includes(id));

  // Sequentially, not in parallel: each un-pin rewrites the same state file on
  // the Rust side, and firing four at once races them against each other.
  const handOff = async () => {
    for (const id of unpinIds) await unpin(id);
  };

  return (
    <>
      <div className="head">
        <div>
          <h1>Proof</h1>
          <div className="sub">
            Did your savers actually save anything? Piggy only says yes when it can show the
            comparison it measured.
          </div>
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

      {/* `.hero.off` is the neutral surface with no green glow: the celebratory
          gradient belongs to a proven saving, not to an experiment still
          waiting for its second arm. */}
      <div className={`hero ${view?.tone === "measured" ? "" : "off"}`}>
        {view ? (
          <>
            <span
              className={`status-chip stamp ${view.tone === "waiting" ? "waiting" : view.tone}${pressStamp ? " press" : ""}`}
            >
              <span className="chip-dot" aria-hidden />
              {view.verdict}
            </span>
            {view.multiplier != null ? (
              <div className="big">
                <em>
                  {view.tone === "estimated" ? "~" : ""}
                  {view.multiplier.toFixed(1)}×
                </em>{" "}
                longer
              </div>
            ) : (
              <div className="big claim">{view.claim}</div>
            )}
            <div className="sub">{view.sub}</div>
            <div className="arms">
              {view.arms.map((a) => (
                <Arm key={a.key} arm={a} />
              ))}
            </div>
            {blocker && (
              <div className="hint">
                <div className="t">
                  <b>{blocker.title}</b> <small>{blocker.detail}</small>
                </div>
                {unpinIds.length > 0 && (
                  <button className="btn" disabled={busy} onClick={() => void handOff()}>
                    {busy
                      ? "Handing back…"
                      : `Let Piggy rotate ${unpinIds.length === 1 ? "it" : "them"}`}
                  </button>
                )}
              </div>
            )}
          </>
        ) : (
          <>
            <div className="eyebrow">Your Claude plan lasts</div>
            <div className="big measuring">reading your sessions…</div>
          </>
        )}
      </div>

      {streams.length > 0 && (
        <>
          <div className="sect">
            Per stream
            <span className="sect-sub">
              tokens per turn, savers off vs on · measured, with no pricing in them
            </span>
          </div>
          <div className="streams">
            {streams.map((s) => (
              <StreamRow key={s.stream} s={s} />
            ))}
          </div>
        </>
      )}

      <div className="sect">
        Per-saver evidence
        <span className="sect-sub">each saver on its own, against the same setup with it off</span>
      </div>
      <div className="attr">
        {rows.map((s) => (
          <SaverRowEvidence key={s.id} saver={s} />
        ))}
        {rows.length === 0 && (
          <div className="arow">
            <span className="an">No savers installed yet.</span>
            <button className="btn" onClick={() => setTab("savers")}>
              Choose a saver
            </button>
          </div>
        )}
      </div>

      {shareOpen && <ShareSheet period={period} onClose={() => setShareOpen(false)} />}
    </>
  );
}
