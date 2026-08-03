import { useEffect, useState } from "react";
import { useStore } from "../store";
import { PiggyMark } from "../components/PiggyMark";
import { formatTokens, commafy } from "../lib/format";
import { useCountUp } from "../lib/motion";
import type { Annotation, Insight, LedgerOverview, LedgerProject, LedgerSource } from "../types";

// The work bucket. Floor rows are identified by the backend's `isFloor`, since
// the floor is now a residual plus any number of named components.
const CONVERSATION = "__conversation";

/** Rows below this are collapsed. A real tree has 24 sources and 14 of them
 *  round to 0.0% — listing them all buries the four that matter. */
const TAIL_SHARE = 0.005;

function pct(fraction: number): string {
  const p = fraction * 100;
  if (p > 0 && p < 0.1) return "<0.1%";
  return `${p.toFixed(1)}%`;
}

/** Overhead is the headline: high means tokens bought startup, not work. */
function overheadTone(overhead: number): "good" | "warn" | "bad" {
  if (overhead >= 0.5) return "bad";
  if (overhead >= 0.2) return "warn";
  return "good";
}

function toneOf(src: LedgerSource): "floor" | "work" | "inject" {
  if (src.isFloor) return "floor";
  return src.kind === CONVERSATION ? "work" : "inject";
}

/**
 * The one-glance answer: a single bar split into what you paid to *start*
 * sessions, what the work actually cost, and what you could configure away.
 * Three numbers in the shape of the argument, above a table that explains it.
 */
function Split({ l }: { l: LedgerOverview }) {
  // Sum every floor row, not just the residual: the floor is decomposed into
  // named components (floor:skill_listing and friends) and they are startup
  // cost too.
  const floor = l.sources.filter((s) => s.isFloor).reduce((n, s) => n + s.tokens, 0);
  const work = l.sources.find((s) => s.kind === CONVERSATION)?.tokens ?? 0;
  const inject = l.sources
    .filter((s) => s.removable && !s.isFloor)
    .reduce((n, s) => n + s.tokens, 0);
  const floorTotal = floor;
  const total = Math.max(l.totalTokens, 1);
  const segs = [
    { tone: "floor", label: "Session floor", tokens: floor, hint: "paid before you type" },
    { tone: "work", label: "Work", tokens: work, hint: "prompts, tools, files" },
    { tone: "inject", label: "Injections", tokens: inject, hint: "configurable" },
  ] as const;

  // THE TICK. The one hero figure on this screen settles like a mechanical
  // counter. Nothing else on the page animates its number.
  const shownOverhead = useCountUp(l.overhead);

  return (
    <div className="lsplit">
      {/* One denominator, stated in full. The old hero said "of your tokens"
          beside a total labelled "cache writes", so the reader could not tell
          which of the two numbers the percentage was a share of. */}
      <div className="lsplit-head">
        <div className="lsplit-claim">
          <span className={`lsplit-big ${overheadTone(l.overhead)}`}>{pct(shownOverhead)}</span>
          <span className="lsplit-of">of cache-write tokens</span>
          <span className="lsplit-cap">were spent before the first message</span>
        </div>
      </div>
      <div className="lsplit-facts">
        <span><b>{formatTokens(floorTotal)}</b> startup</span>
        <span><b>{commafy(l.sessions)}</b> sessions</span>
        <span><b>{formatTokens(l.sessions ? floorTotal / l.sessions : 0)}</b> per session</span>
      </div>

      <div className="lsplit-bar" role="img" aria-label={`${pct(l.overhead)} session floor`}>
        {segs.map((s) => (
          <i
            key={s.tone}
            className={`lseg ${s.tone}`}
            style={{ width: `${(s.tokens / total) * 100}%` }}
            title={`${s.label}: ${commafy(s.tokens)} tokens`}
          />
        ))}
      </div>

      <div className="lsplit-key">
        {segs.map((s) => (
          <div key={s.tone} className="lkey">
            <span className="lkey-top">
              <i className={`ldot ${s.tone}`} />
              <b>{s.label}</b>
            </span>
            <small>
              {formatTokens(s.tokens)} · {pct(s.tokens / total)}
            </small>
            <em>{s.hint}</em>
          </div>
        ))}
      </div>
    </div>
  );
}

/**
 * Prose from the opt-in local model, attached under the finding it explains.
 *
 * Branded "Piggy Insights" rather than by model name, so it reads as part of the
 * product. The separation from the measured text above it therefore rests
 * entirely on the panel: everything else on this screen is arithmetic on observed
 * tokens, and this is a 4B model's reading of it. Hence the inset rule and the
 * quieter type. Which model wrote it, and the fact that it was generated rather
 * than measured, live in the tooltip.
 */
function Note({ note }: { note: Annotation }) {
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

/** Findings, above the tables. Each one is a number the user can act on, so the
 *  action is always visible rather than hidden behind a disclosure. */
function Insights({ items, notes }: { items: Insight[]; notes: Annotation[] }) {
  const [all, setAll] = useState(false);
  if (items.length === 0) return null;
  const shown = all ? items : items.slice(0, 3);
  const noteFor = (id: string) => notes.find((n) => n.insightId === id);
  return (
    <>
      <div className="sect">
        Top opportunities
        <span className="sect-sub">
          {items.length} finding{items.length === 1 ? "" : "s"}, ranked by avoidable spend
        </span>
      </div>
      <div className="linsights">
        {shown.map((i, idx) => {
          const note = noteFor(i.id);
          return (
            <div key={i.id} className={`lins ${i.severity}`}>
              {/* Rank, not alarm. Three red HIGH labels in a column say nothing
                  about relative priority and make a list of opportunities read
                  as a list of failures. */}
              <span className="lins-rank">{String(idx + 1).padStart(2, "0")}</span>
              <div className="lins-body">
                <b>{i.title}</b>
                {/* The two claim types are labelled rather than blended: the
                    detail is arithmetic on observed tokens, the action is
                    advice inferred from it. */}
                <p><span className="lins-tag meas">Measured</span>{i.detail}</p>
                <em><span className="lins-tag rec">Do</span>{i.action}</em>
                {note && <Note note={note} />}
              </div>
              <span className="lins-right">
                <span className="lins-tok" title={`${commafy(i.tokens)} tokens`}>
                  {formatTokens(i.tokens)}
                </span>
              </span>
            </div>
          );
        })}
        {items.length > 3 && (
          <button className="ltail" onClick={() => setAll((v) => !v)}>
            {all ? "Show top 3 only" : `Show ${items.length - 3} more findings`}
          </button>
        )}
      </div>
    </>
  );
}

function SourceRow({ src, max }: { src: LedgerSource; max: number }) {
  return (
    <div className="lrow">
      <span className="lrow-label">{src.label}</span>
      <span className="lrow-bar" aria-hidden>
        <i
          className={`lbar ${toneOf(src)}`}
          style={{ width: `${max > 0 ? (src.tokens / max) * 100 : 0}%` }}
        />
      </span>
      <span className="lrow-tok" title={`${commafy(src.tokens)} tokens`}>
        {formatTokens(src.tokens)}
      </span>
      <span className="lrow-share">{pct(src.share)}</span>
    </div>
  );
}

function ProjectRow({ p }: { p: LedgerProject }) {
  const tone = overheadTone(p.overhead);
  return (
    <div className="lprow">
      <span className="lp-name" title={p.project}>
        {p.name}
      </span>
      <span className="lp-num">{commafy(p.sessions)}</span>
      <span className="lp-num">{p.msgsPerSession.toFixed(1)}</span>
      <span className="lp-num">{formatTokens(p.floorTokens)}</span>
      <span className="lp-num">{formatTokens(p.workTokens)}</span>
      <span className="lp-overcell">
        <i className="lp-track" aria-hidden>
          <i className={`lp-fill ${tone}`} style={{ width: `${p.overhead * 100}%` }} />
        </i>
        <b className={tone}>{pct(p.overhead)}</b>
      </span>
    </div>
  );
}

function Shell({ children }: { children: React.ReactNode }) {
  return (
    <div className="analytics">
      <div className="sect">
        Context ledger
        <span className="sect-sub">where your tokens come from</span>
      </div>
      <div className="foot-note" style={{ marginTop: 0 }}>
        {children}
      </div>
    </div>
  );
}

export function Ledger() {
  const ledger = useStore((s) => s.ledger);
  const period = useStore((s) => s.period);
  const setPeriod = useStore((s) => s.setPeriod);
  const findings = useStore((s) => s.insights);
  const notes = useStore((s) => s.annotations);
  const loadAnnotations = useStore((s) => s.loadAnnotations);
  const [showTail, setShowTail] = useState(false);

  // The Ledger is the only screen that renders the advisor's prose, so it is
  // what asks for it. Driving inference from the store's refresh instead meant a
  // 4B model running on every session-log write regardless of which tab was
  // open. `loadAnnotations` no-ops once the current period has been asked.
  useEffect(() => {
    if (findings.length > 0) void loadAnnotations();
  }, [findings, loadAnnotations]);

  if (!ledger)
    return (
      <div className="piggy-wait">
        <div className="piggy-run" role="img" aria-label="Reading your sessions">
          {[0, 1, 2].map((i) => (
            <i key={i} style={{ animationDelay: `${i * 0.5}s` }}>
              <PiggyMark size={24} />
            </i>
          ))}
        </div>
        Reading your sessions…
      </div>
    );
  if (ledger.empty) {
    return (
      <Shell>
        Nothing indexed for {ledger.periodLabel.toLowerCase()} yet. Once Claude runs, every token in
        your context window shows up here with its cause.
      </Shell>
    );
  }

  const max = ledger.sources[0]?.tokens ?? 0;
  const head = ledger.sources.filter((s) => s.share >= TAIL_SHARE);
  const tail = ledger.sources.filter((s) => s.share < TAIL_SHARE);
  const tailTokens = tail.reduce((n, s) => n + s.tokens, 0);

  // Worst offenders first, but only projects big enough for the ratio to mean
  // something: a 2-session project at 90% overhead is noise, not a finding.
  const worst = [...ledger.projects]
    .filter((p) => p.sessions >= 5 && p.floorTokens + p.workTokens > 0)
    .sort((a, b) => b.floorTokens - a.floorTokens)
    .slice(0, 8);

  return (
    <div className="analytics">
      {/* A title, a sentence, and controls. The old single mono line was the
          heading, the description and the date range at once, and the period
          was prose the user could not change from here. */}
      <div className="head">
        <div>
          <h1>Spend</h1>
          <div className="sub">See where your tokens went and what caused them.</div>
        </div>
      </div>
      <div className="viewbar">
        <div className="views">
          <span className="on">By cause</span>
          <span>Over time</span>
        </div>
        <div className="periods">
          {(["today", "week", "month", "all"] as const).map((p) => (
            <button key={p} className={period === p ? "on" : ""} onClick={() => void setPeriod(p)}>
              {{ today: "Day", week: "Week", month: "Month", all: "All" }[p]}
            </button>
          ))}
        </div>
      </div>

      <Split l={ledger} />

      <Insights items={findings} notes={notes} />

      <div className="sect">
        By source
        <span className="sect-sub">every token charged to what caused it</span>
      </div>
      <div className="lledger">
        {head.map((s) => (
          <SourceRow key={s.kind} src={s} max={max} />
        ))}
        {tail.length > 0 && (
          <>
            {showTail && tail.map((s) => <SourceRow key={s.kind} src={s} max={max} />)}
            <button className="ltail" onClick={() => setShowTail((v) => !v)}>
              {showTail ? "Hide" : "Show"} {tail.length} smaller sources
              <span>{formatTokens(tailTokens)} combined</span>
            </button>
          </>
        )}
      </div>

      {worst.length > 0 && (
        <>
          <div className="sect">
            By project
            <span className="sect-sub">
              overhead is the floor's share; short sessions pay it over and over
            </span>
          </div>
          <div className="lprojects">
            <div className="lprow lp-head">
              <span className="lp-name">project</span>
              <span className="lp-num">sessions</span>
              <span className="lp-num">msg/sess</span>
              <span className="lp-num">floor</span>
              <span className="lp-num">work</span>
              <span className="lp-overcell">overhead</span>
            </div>
            {worst.map((p) => (
              <ProjectRow key={p.project} p={p} />
            ))}
          </div>
        </>
      )}

      <div className="foot-note" style={{ marginTop: 0 }}>
        These are measured tokens, not estimates: every cache write is charged to whatever entered
        the context before it. The session floor is your system prompt, tool definitions and memory,
        paid once per session before you type. Projects that open many short sessions pay it over
        and over, which is what a high overhead means.
      </div>
    </div>
  );
}
