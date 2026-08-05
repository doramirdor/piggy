import { useEffect, useState } from "react";
import { useStore } from "../store";
import { StatusChip } from "../components/StatusChip";
import { PiggyMark } from "../components/PiggyMark";
import { badgeView } from "../lib/badge";
import { proofView, type ProofArm, type ProofBlocker } from "../lib/proof";
import { commafy, pctMagnitude } from "../lib/format";
import { useStampOnce } from "../lib/motion";
import { ShareSheet } from "./ShareSheet";
import type { Annotation, Badge, HeadlineStream, SaverRow } from "../types";

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
  //
  // Counted in USABLE sessions, never in held ones. On a pooled ON arm those
  // differ by three orders of magnitude, and the held count is the one that
  // looks reassuring: "9,792 sessions" over a full rail, for an arm five
  // sessions into a bar of ten.
  const count = arm.state === "short" ? `${commafy(arm.usable)} of ${arm.target}` : sessions;
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
        aria-valuenow={Math.min(arm.usable, arm.target)}
      >
        {/* Hatched rather than filled when the sessions exist but cannot count:
            a solid bar at full width would read as "done". An arm with no
            sessions gets no sliver at all - "almost nothing" and "nothing at
            all" are different states, and only one of them is the blocker. */}
        {arm.state === "unusable" ? (
          <i className="ghost" />
        ) : (
          arm.usable > 0 && (
            <i style={{ width: `${Math.max(Math.min(1, arm.usable / arm.target) * 100, 3)}%` }} />
          )
        )}
      </div>
    </div>
  );
}

/** One named blocker, and its fix when the fix is a button.
 *
 *  Its own component because there can be more than one at a time: the pin and
 *  the withheld estimate are separate facts with separate remedies, and the card
 *  used to be a single slot that silently dropped whichever came second. */
function BlockerCard({ blocker }: { blocker: ProofBlocker }) {
  const unpin = useStore((s) => s.unpinSaver);
  const busySavers = useStore((s) => s.busySavers);
  const ids = blocker.unpin;
  const busy = ids.some((id) => busySavers.includes(id));
  // Sequentially, not in parallel: each un-pin rewrites the same state file on
  // the Rust side, and firing four at once races them against each other.
  const handOff = async () => {
    for (const id of ids) await unpin(id);
  };
  return (
    <div className="hint">
      <div className="t">
        <b>{blocker.title}</b> <small>{blocker.detail}</small>
      </div>
      {ids.length > 0 && (
        <button className="btn" disabled={busy} onClick={() => void handOff()}>
          {busy ? "Handing over…" : `Let Piggy switch ${ids.length === 1 ? "it" : "them"} on and off`}
        </button>
      )}
    </div>
  );
}

/** The chip word for each non-numeric reading. Only `waiting` keeps
 *  "Measuring": it is the only one of the four that is actually still running,
 *  and it is the only one whose progress bar means anything. Absent keys (a
 *  settled `delta`, or an older payload with no reading) fall back to the
 *  badge's own word. */
const CHIP: Record<string, string | undefined> = {
  quiet: "Too small",
  no_change: "No change",
  inconclusive: "Too noisy",
};

/** One stream's side-by-side comparison: the two medians the delta is a ratio
 *  of. These are the only numbers on this screen with no pricing in them, which
 *  is why they sit above the × rather than below it (docs/measurement.md).
 *
 *  Bars are scaled within the row, not across rows: cache write runs two orders
 *  of magnitude above input, so a shared scale would flatten three of the four
 *  rows into invisible slivers and hide the very comparison the row exists for. */
function StreamRow({
  s,
  invert = false,
  subject = "your savers",
}: {
  s: HeadlineStream;
  invert?: boolean;
  /** Whose turns the warning is about: the whole set, or the one saver whose
   *  row this is nested under. */
  subject?: string;
}) {
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
  // For turns the arithmetic is identical but the stakes are not: this row is
  // the only place a "saving" on every other row can be revealed as a loss, so
  // it says so in words rather than leaving it to a bar colour.
  const regressed = invert && dir === "worse";
  return (
    <div className="srow">
      <div className="srow-top">
        <span className="snm">{s.stream}</span>
        {s.delta != null && (
          <span className={`adelta${s.kind === "estimated" ? " est" : ""}`}>{signed(s.delta)}</span>
        )}
        <StatusChip badge={badge} label={CHIP[s.reading ?? ""]} />
      </div>
      {/* Without this the row showed two medians under one word, and "we
          compared this and it moved nothing" was indistinguishable from "we
          have not compared it yet". */}
      {s.note && <div className="snote">{s.note}</div>}
      {regressed && (
        <div className="turns-warn">
          More turns with {subject} on. Every per-turn figure below divides by this, so a
          saving there can still be a loss overall.
        </div>
      )}
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

/** What the local model made of one saver's measurement, when the user has one
 *  switched on. Marked as generated: the numbers above it are measured, this
 *  wording was written on this Mac, and the two must never look alike. */
function SaverNote({ note }: { note: Annotation }) {
  return (
    <div className="lins-note">
      <span
        className="lins-note-tag"
        title={`Written on this Mac by ${note.model}. Piggy's numbers are measured; this wording is generated.`}
      >
        <PiggyMark size={12} />
        Piggy Insights
      </span>
      <div>
        <b>{note.headline}</b>
        <p>{note.why}</p>
      </div>
    </div>
  );
}

/** Per-saver evidence: both arms and the state, never a number without its n.
 *
 *  The row's own number is the output stream; opening it shows the same four
 *  streams the headline breaks out, for this saver alone. Cache read is dropped
 *  here for the same reason as up there: it is not in the price-weighted spend
 *  the claim is built on. */
function SaverRowEvidence({ saver, note }: { saver: SaverRow; note?: Annotation }) {
  const v = badgeView(saver.badge);
  const hasDelta = (v.tone === "measured" || v.tone === "estimated") && saver.badge.delta != null;
  // A saver Piggy has never seen either arm of has four all-zero streams; that
  // is a row with nothing behind the triangle, not a comparison.
  const streams = (saver.streams ?? []).filter(
    (s) => s.stream !== "cache read" && (s.nOn > 0 || s.nOff > 0),
  );
  const head = (
    <>
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
    </>
  );
  // No attribution yet: no disclosure triangle to open onto an empty panel.
  if (streams.length === 0) return <div className="arow">{head}</div>;
  return (
    <details className="adet">
      <summary className="arow">{head}</summary>
      <div className="streams asub">
        {saver.summary && <div className="asum">{saver.summary}</div>}
        {/* Measured, and the part the summary leaves out. Deterministic, so it
            is here whether or not the local model is switched on. */}
        {saver.caveat && <div className="acav">{saver.caveat}</div>}
        {note && <SaverNote note={note} />}
        {saver.turns && <StreamRow s={saver.turns} invert subject="this saver" />}
        {streams.map((s) => (
          <StreamRow key={s.stream} s={s} />
        ))}
      </div>
    </details>
  );
}

export function Proof() {
  const stats = useStore((s) => s.stats);
  const savers = useStore((s) => s.savers);
  const period = useStore((s) => s.period);
  const setTab = useStore((s) => s.setTab);
  const saverNotes = useStore((s) => s.saverNotes);
  const loadSaverNotes = useStore((s) => s.loadSaverNotes);
  const [shareOpen, setShareOpen] = useState(false);

  const rows = savers?.savers ?? [];
  // The local model, once, and only from this screen: it is the only place the
  // per-saver prose is shown, and a load is ~3GB resident.
  useEffect(() => {
    if (rows.length > 0) void loadSaverNotes();
  }, [rows.length, loadSaverNotes]);
  const noteFor = (id: string) => saverNotes.find((n) => n.insightId === `saver:${id}`);
  const view = proofView(stats?.headline ?? null, rows);
  // THE STAMP. Keyed by the identity of the claim, not by mount: the verdict
  // presses on the first time a given claim is earned and never again. Tone plus
  // the value is enough to tell one claim from the next - and deliberately NOT
  // the period, because the claim is not period-scoped, so keying on it would
  // re-press the same verdict when Spend's picker moves. The flag lives outside
  // React because Proof remounts on every tab switch and re-renders every few
  // seconds off the session watcher.
  const claimId =
    view && view.tone !== "waiting" && view.multiplier != null
      ? `${view.tone}:${view.multiplier.toFixed(2)}`
      : null;
  const pressStamp = useStampOnce(claimId);
  // Cache read is excluded from the price-weighted spend the × is built on
  // (docs/measurement.md), so it is not evidence for the claim this screen
  // makes. The payload still carries it for anything that wants all four.
  const streams = (stats?.headline.streams ?? []).filter((s) => s.stream !== "cache read");
  // The denominator gets its own row ABOVE the streams it divides. A saver that
  // buys cheaper turns by needing more of them looks green on every stream
  // below and is a loss overall, so this is the first thing shown, not a
  // footnote under them.
  const turns = stats?.headline.turns ?? null;

  return (
    <>
      <div className="head">
        <div>
          <h1>Proof</h1>
          <div className="sub">
            Did your savers actually save anything? Piggy only says yes when it can show the
            comparison it measured. Every session you have ever run counts toward it - the
            experiment needs {view?.arms[0]?.target ?? 10} sessions on each side, so it is not
            sliced by date.
          </div>
        </div>
        <button className="btn primary" onClick={() => setShareOpen(true)}>
          Share
        </button>
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
            {/* The answer to "why does this always say measuring". Sits between
                the arms and the blocker: the arms show WHERE the count is, this
                says what the count is of, why it is there, and when it ends. */}
            {view.wait && (
              <div className="waitbox">
                <div className="wb-what">{view.wait.what}</div>
                {view.wait.progress && (
                  <div className="wb-row">
                    <b>{view.wait.progress}</b>
                    {view.wait.eta && <span>{view.wait.eta}</span>}
                  </div>
                )}
                {view.wait.because && <div className="wb-why">{view.wait.because}</div>}
              </div>
            )}
            {view.blockers.map((b) => (
              <BlockerCard key={b.title} blocker={b} />
            ))}
          </>
        ) : (
          <>
            <div className="eyebrow">Your Claude plan</div>
            <div className="big measuring">Reading your sessions…</div>
            <div className="sub">
              Every session on disk counts toward the comparison, so the first read takes a
              moment.
            </div>
            <div className="progress hero-progress" role="progressbar" aria-label="Reading sessions">
              <div className="progress-bar" />
            </div>
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
            {turns && <StreamRow s={turns} invert />}
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
        {/* Loading and "you have no savers" are different answers to the same
            question, and the empty state is a call to action - showing it while
            the list is still being read tells the user to fix a problem they may
            not have. */}
        {savers === null && (
          <>
            <div className="load-head">
              <div className="load-txt">
                <div className="desc">Loading savers…</div>
                <div className="progress" role="progressbar" aria-label="Loading savers">
                  <div className="progress-bar" />
                </div>
              </div>
              <span className="chip-spinner" role="status" aria-label="Loading" />
            </div>
            {[0, 1, 2, 3, 4].map((i) => (
              <div className="arow skel" key={i} aria-hidden>
                <div className="aname">
                  <div className="sk sk-line" />
                </div>
                <div className="sk sk-line sk-short" />
                <div className="sk sk-chip" />
              </div>
            ))}
          </>
        )}
        {rows.map((s) => (
          <SaverRowEvidence key={s.id} saver={s} note={noteFor(s.id)} />
        ))}
        {savers !== null && rows.length === 0 && (
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
