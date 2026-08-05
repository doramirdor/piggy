import { useEffect, useState } from "react";
import { useStore } from "../store";
import { PiggyMark } from "../components/PiggyMark";
import { UsageChart } from "../components/UsageChart";
import { Sparkline } from "../components/Sparkline";
import { formatTokens, commafy, pctMagnitude } from "../lib/format";
import { analyzeUsage, shortDay } from "../lib/usage";
import { useCountUp } from "../lib/motion";
import type {
  Annotation,
  Insight,
  LedgerOverview,
  LedgerProject,
  LedgerSource,
  TaskRow,
} from "../types";

// The work bucket. Floor rows are identified by the backend's `isFloor`, since
// the floor is now a residual plus any number of named components.
const CONVERSATION = "__conversation";

/** Rows below this are collapsed. A real tree has 24 sources and 14 of them
 *  round to 0.0%, and listing them all buries the four that matter. */
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
 *
 * Leads with the PER-SESSION figure, not the share. "89.1% of cache-write
 * tokens" was the headline for a while and it answered nothing a reader could
 * act on: they do not know how big cache writes are relative to their bill, so
 * the percentage could equally have meant "most of your money" or "a rounding
 * error", and the screen never said which. A per-session token count is a thing
 * a human can hold (it is the same number the context window is measured in),
 * and the share follows as supporting detail, with its denominator spelled out
 * in the same sentence rather than in a label two lines away.
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
    {
      tone: "floor",
      label: "Starting sessions",
      tokens: floor,
      hint: "loaded before your first message",
    },
    { tone: "work", label: "Your work", tokens: work, hint: "your prompts, tool output, files" },
    {
      tone: "inject",
      label: "Re-sent mid-session",
      tokens: inject,
      hint: "hooks and reminders, configurable",
    },
  ] as const;

  const perSession = l.sessions ? floorTotal / l.sessions : 0;
  const removablePerSession = l.sessions ? l.removableTokens / l.sessions : 0;
  // THE TICK. The one hero figure on this screen settles like a mechanical
  // counter. Nothing else on the page animates its number.
  const shownPerSession = useCountUp(perSession);
  // The concentration line, when there is one. A 7,000-session week whose floor
  // is 80% one benchmark harness is a fact about that harness, not about the
  // user's day job, and a hero that omits it invites them to go trimming skills
  // they actually use. Only stated when one project really does dominate.
  const topProject = [...l.projects].sort((a, b) => b.floorTokens - a.floorTokens)[0];
  const concentrated =
    topProject && floorTotal > 0 && topProject.floorTokens / floorTotal >= 0.25
      ? topProject
      : null;

  return (
    <div className="lsplit">
      {/* The claim, in the order a reader can use it: how much per session, then
          what that adds up to and what share of what, then the part they can
          actually remove. Every number states its own denominator. */}
      <div className="lsplit-head">
        <div className="lsplit-claim">
          <span className={`lsplit-big ${overheadTone(l.overhead)}`}>
            {formatTokens(shownPerSession)}
          </span>
          <span className="lsplit-of">tokens per session, before you type</span>
          <span className="lsplit-cap">
            Every session starts with your system prompt, tool definitions, memory and skill
            listings already in the context window. Across {commafy(l.sessions)} sessions that is{" "}
            {formatTokens(floorTotal)} tokens, or {pct(l.overhead)} of every token written into the
            cache {l.periodLabel.toLowerCase()}, spent before any work happened.
          </span>
        </div>
      </div>
      {/* The lever. Without this the screen is an autopsy: a big number, no
          verb. `headroom` is available headroom, never savings banked, and the
          wording has to keep saying "would" (see `Ledger::headroom`). */}
      {removablePerSession > 0 && (
        <div className="lsplit-lever">
          <b>{formatTokens(removablePerSession)} of it is yours to remove:</b> skill listings, hooks
          and agent listings that load whether or not a session uses them.
          {l.headroom != null && (
            <>
              {" "}
              That is {pct(l.removableShare)} of your bill; without it the same plan would go{" "}
              {l.headroom.toFixed(2)}× further.
            </>
          )}
        </div>
      )}
      {concentrated && (
        <div className="lsplit-lever quiet">
          {/* "Concentrated", not "mostly": this fires from 25% up, and "mostly"
              would be plain wrong at the bottom of that range. */}
          Concentrated in one project: <b>{concentrated.name}</b> is{" "}
          {pct(concentrated.floorTokens / floorTotal)} of it, over {commafy(concentrated.sessions)}{" "}
          sessions averaging {concentrated.msgsPerSession.toFixed(1)} messages each. Short sessions
          pay the full startup cost for almost no work.
        </div>
      )}

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

/**
 * The other half of the same question: not what caused the tokens, but when
 * they were spent. Day-over-day totals, the four streams stacked, and cache
 * reuse, the one lever that changes the shape of the chart.
 */
function OverTime() {
  const series = useStore((s) => s.series);
  const stats = useStore((s) => s.stats);
  const a = analyzeUsage(series);
  const label = (series?.periodLabel ?? stats?.periodLabel ?? "").toLowerCase();

  if (!series || a.activeDays === 0) {
    return (
      <div className="analytics">
        <div className="sect">
          Day over day
          <span className="sect-sub">usage and token maximization</span>
        </div>
        <div className="foot-note" style={{ marginTop: 0 }}>
          No usage in this window yet. Once Claude runs, your day-over-day tokens and cache reuse
          show up here.
        </div>
      </div>
    );
  }

  const trend = a.trendPct;
  const trendUp = trend != null && trend > 0;
  const cachePct = a.cacheHitRate != null ? Math.round(a.cacheHitRate * 100) : null;

  return (
    <div className="analytics">
      <div className="sect">
        Day over day
        <span className="sect-sub">usage and token maximization · {label}</span>
      </div>

      <div className="kpis">
        <div className="kpi">
          <small>Tokens</small>
          <strong>{formatTokens(a.totalTokens)}</strong>
          <p>across {a.activeDays} active day{a.activeDays === 1 ? "" : "s"}</p>
        </div>
        <div className="kpi">
          <small>Daily average</small>
          <strong>{formatTokens(a.dailyAvg)}</strong>
          <p>
            {trend != null ? (
              <span className={`trend ${trendUp ? "up" : "down"}`}>
                {trendUp ? "▲" : "▼"} {pctMagnitude(trend)} vs prior day
              </span>
            ) : (
              "per active day"
            )}
          </p>
        </div>
        <div className="kpi">
          <small>Busiest day</small>
          <strong>{a.busiest ? formatTokens(a.busiest.totalTokens) : "-"}</strong>
          <p>{a.busiest ? shortDay(a.busiest.date) : "no data"}</p>
        </div>
        <div className="kpi">
          <small>Cache reuse</small>
          <strong className={cachePct != null && cachePct >= 40 ? "green" : ""}>
            {cachePct != null ? `${cachePct}%` : "-"}
          </strong>
          <p>context served from cache</p>
        </div>
      </div>

      <div className="uchart-card">
        <UsageChart series={series} />
      </div>
      <div className="foot-note" style={{ marginTop: 0 }}>
        Tokens are measured from your session logs; cost and cache reuse are computed from them.
        Cache reuse is the main token-maximization lever - the higher it is, the less context Claude
        re-reads each turn.
      </div>
    </div>
  );
}

// Sortable numeric columns of the task table, each paired with the value it
// ranks by. `null` sorts last in every case: "not recorded" is not a small
// number, and letting it sort as 0 would park unmeasured projects at one end of
// the table as though that were a finding.
const TASK_SORTS = {
  total: (r: TaskRow) => r.totalTokens,
  sessions: (r: TaskRow) => r.sessions,
  tasks: (r: TaskRow) => (r.tasks === 0 ? null : r.tasks),
  turns: (r: TaskRow) => r.turnsPerTask,
  fail: (r: TaskRow) => r.failureRate,
  delta: (r: TaskRow) => r.delta,
} as const;

type TaskSort = keyof typeof TASK_SORTS;

/** Longest trend series the backend draws (`MAX_ALL_DAYS` in `tasks.rs`). Only
 *  all-time can reach it, and reaching it is what makes the series cover less
 *  than the total beside it. */
const SPARK_MAX_DAYS = 120;

/** What a task cell shows when the figure was never recorded. One constant so
 *  the footnote below quotes the same character the columns print. An en dash,
 *  the typographic placeholder for an empty cell, and deliberately not an em
 *  dash: those are out everywhere in this product. */
const NO_VALUE = "–";

/** Two states, not a three-tone ramp.
 *
 *  Colour has to be earned here: `--amber` and `--red` both resolve to the same
 *  rust in this palette, so a warn/bad scale renders as one colour pretending to
 *  be two. Red marks the band worth acting on and everything below it stays
 *  neutral, and the count under the figure ("18 of 31") carries the meaning in
 *  text either way, so nothing rests on the colour alone. */
function failTone(rate: number): "" | "bad" {
  return rate >= 0.4 ? "bad" : "";
}

function TaskRowCells({ r }: { r: TaskRow }) {
  // Zero tasks means the log carried no `promptId`, not that nothing was asked.
  // Every derived column has to say "not recorded" rather than imply an outcome.
  const unrecorded = r.tasks === 0;
  const delta = r.delta;
  return (
    <div className="trow">
      <span className="t-name" title={r.project}>
        {r.name}
      </span>
      <span className="t-num">{commafy(r.sessions)}</span>
      <span className={`t-num${unrecorded ? " t-none" : ""}`}>
        {unrecorded ? NO_VALUE : commafy(r.tasks)}
      </span>
      {/* The value and its denominator in one cell: a share with no basis beside
          it is the exact shape this product refuses. */}
      <span className="t-totcell" title={`${commafy(r.totalTokens)} tokens`}>
        <b>{formatTokens(r.totalTokens)}</b>
        <i>{pct(r.share)} of window</i>
      </span>
      <span className={`t-num${unrecorded ? " t-none" : ""}`}>
        {r.turnsPerTask == null ? NO_VALUE : r.turnsPerTask.toFixed(1)}
      </span>
      <span className={`t-failcell${unrecorded ? " t-none" : ""}`}>
        {r.failureRate == null ? (
          NO_VALUE
        ) : (
          <>
            <b className={failTone(r.failureRate)}>{pct(r.failureRate)}</b>
            <i>
              {commafy(r.failedTasks)} of {commafy(r.tasks)}
            </i>
          </>
        )}
      </span>
      {/* No prior window, or an empty one. Either way there is no comparison to
          report, and inventing one from a single window is not a delta. */}
      {/* Neutral by choice. Spending more than last week is not a failure and
          spending less is not a proven saving, so the sign carries the
          direction and no colour claims a verdict. */}
      <span className={`t-num${delta == null ? " t-none" : ""}`}>
        {delta == null ? NO_VALUE : `${delta > 0 ? "+" : "−"}${pctMagnitude(delta)}`}
      </span>
      <span className="t-spark">
        <Sparkline points={r.daily} label={r.name} />
      </span>
    </div>
  );
}

/**
 * Which of my tasks was expensive, and did it work.
 *
 * The token columns come from the same ledger the By cause view reads, so the
 * two cannot disagree. The task columns come from `promptId` boundaries and the
 * `is_error` flag on tool results: the only outcome signal the session logs
 * carry, and the reason this view is not just the project table again.
 */
function Tasks() {
  const table = useStore((s) => s.tasks);
  const period = useStore((s) => s.period);
  const loadTasks = useStore((s) => s.loadTasks);
  const [sort, setSort] = useState<TaskSort>("total");

  // On demand: this view is one of three and may never be opened, and the
  // store's refresh runs on the session watcher's debounce.
  //
  // `period` is a dependency because a period change discards the table. It is
  // windowed AND compared against the window before it, so it cannot survive
  // one. Without it the effect never re-fires and the view sits on its loading
  // state forever. `loadTasks` itself is a no-op once the current period has
  // been fetched, so re-running this is cheap.
  useEffect(() => {
    void loadTasks();
  }, [loadTasks, period]);

  if (!table) {
    return (
      <div className="analytics">
        <div className="sect">
          Tasks
          <span className="sect-sub">what each project cost, and how often it failed</span>
        </div>
        <div className="foot-note" style={{ marginTop: 0 }}>
          Reading your tasks…
        </div>
      </div>
    );
  }

  if (table.empty) {
    return (
      <div className="analytics">
        <div className="sect">
          Tasks
          <span className="sect-sub">what each project cost, and how often it failed</span>
        </div>
        <div className="foot-note" style={{ marginTop: 0 }}>
          Nothing indexed for {table.periodLabel.toLowerCase()} yet. Once Claude runs, every project
          shows up here with what it spent and how often its tools failed.
        </div>
      </div>
    );
  }

  // An all-time total covers every session on record, while the trend series
  // stops at the backend's chart clamp (`MAX_ALL_DAYS`). Every other period
  // matches exactly, and so does all-time once history is shorter than the
  // clamp, so the note is only shown when the two really do differ.
  //
  // Measured across every row, not the first one: a project the ledger knows
  // but the task rows do not carries an empty series, and sorted to the top it
  // would hide the note for the whole table.
  const sparkDays = table.rows.reduce((n, r) => Math.max(n, r.daily.length), 0);
  const sparkBounded = table.period === "all" && sparkDays >= SPARK_MAX_DAYS;

  const key = TASK_SORTS[sort];
  const rows = [...table.rows].sort((a, b) => {
    const av = key(a);
    const bv = key(b);
    // Unmeasured last, whichever column is active.
    if (av == null && bv == null) return b.totalTokens - a.totalTokens;
    if (av == null) return 1;
    if (bv == null) return -1;
    return bv - av || a.name.localeCompare(b.name);
  });

  const th = (id: TaskSort, text: string, title?: string) => (
    <button
      className={`t-sort${sort === id ? " on" : ""}`}
      onClick={() => setSort(id)}
      title={title ?? `Sort by ${text.toLowerCase()}`}
      aria-pressed={sort === id}
    >
      {text}
    </button>
  );

  return (
    <div className="analytics">
      <div className="sect">
        Tasks
        <span className="sect-sub">
          {commafy(table.rows.length)} project{table.rows.length === 1 ? "" : "s"} ·{" "}
          {table.periodLabel.toLowerCase()}
        </span>
      </div>

      {table.tasksUnrecorded && (
        <div className="foot-note" style={{ marginTop: 0, marginBottom: 12 }}>
          No task boundaries were recorded in this window. These session logs predate the prompt
          identifier Piggy groups tasks by, so the task, turn and failure columns read “{NO_VALUE}”
          rather than zero. Token columns are unaffected and exact.
        </div>
      )}

      {/* Scrolls rather than compressing: ten columns of tabular figures stop
          being readable long before they stop fitting. */}
      <div className="ttable-wrap">
        <div className="ttable">
          <div className="trow t-head">
            <span className="t-name">Project</span>
            {th("sessions", "Sessions")}
            {th("tasks", "Tasks", "Sort by tasks recorded")}
            {th("total", "Total")}
            {th("turns", "Turns / task")}
            {th("fail", "Tool failures")}
            {th("delta", "Δ prior")}
            <span
              className="t-spark"
              title={
                sparkBounded
                  ? `Cache-write tokens per day, most recent ${sparkDays} days`
                  : "Cache-write tokens per day"
              }
            >
              Trend
            </span>
          </div>
          {rows.map((r) => (
            <TaskRowCells key={r.project} r={r} />
          ))}
        </div>
      </div>

      <div className="foot-note">
        Totals are cache-write tokens, the same ones By cause splits into startup and work. That
        split has its home there rather than a second copy here.
        <b> Δ prior</b> compares this window against the equal-length window before it, so it is
        arithmetic on observed tokens rather than a saver measurement. The Proof tab is where a
        saver has to earn its claim. <b>Tool failures</b> counts tasks with at least one
        <code> tool_result</code> flagged as an error; an unflagged failure is not counted, so the
        rate is a floor rather than an estimate. Task, turn and failure figures cover only the
        sessions whose logs recorded a prompt identifier, which is why a project can show fewer
        tasks than sessions. Divide them against each other and the answer means nothing.
        {sparkBounded && (
          <>
            {" "}
            <b>Trend</b> draws the most recent {sparkDays} days rather than every day on record, so
            over all time it covers less history than the total beside it.
          </>
        )}
      </div>
    </div>
  );
}

/** The default view: the split, the findings, and every token charged to a cause. */
function ByCause({
  ledger,
  findings,
  notes,
}: {
  ledger: LedgerOverview;
  findings: Insight[];
  notes: Annotation[];
}) {
  const [showTail, setShowTail] = useState(false);

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
    <>
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
    </>
  );
}

export function Ledger() {
  const ledger = useStore((s) => s.ledger);
  const period = useStore((s) => s.period);
  const setPeriod = useStore((s) => s.setPeriod);
  const findings = useStore((s) => s.insights);
  const notes = useStore((s) => s.annotations);
  const loadAnnotations = useStore((s) => s.loadAnnotations);
  const busy = useStore((s) => s.periodBusy);
  const [view, setView] = useState<"cause" | "time" | "tasks">("cause");

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
  const stale = busy ? " busy" : "";

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
      {/* The bar stays put through an empty period and through a slice swap:
          the controls that got you into a window are the ones that get you out
          of it. */}
      <div className="viewbar">
        <div className="views">
          <button className={view === "cause" ? "on" : ""} onClick={() => setView("cause")}>
            By cause
          </button>
          <button className={view === "time" ? "on" : ""} onClick={() => setView("time")}>
            Over time
          </button>
          <button className={view === "tasks" ? "on" : ""} onClick={() => setView("tasks")}>
            Tasks
          </button>
        </div>
        <div className="periods">
          {busy && <span className="chip-spinner slice-spin" role="status" aria-label="Loading" />}
          {(["today", "week", "month", "all"] as const).map((p) => (
            <button key={p} className={period === p ? "on" : ""} onClick={() => void setPeriod(p)}>
              {{ today: "Day", week: "Week", month: "Month", all: "All" }[p]}
            </button>
          ))}
        </div>
      </div>

      {/* The old slice stays on screen and dims while the new one loads, so the
          page reads as the same page changing rather than a blank re-entry. */}
      <div className={`slice${stale}`} aria-busy={busy}>
        {view === "tasks" ? (
          <Tasks />
        ) : ledger.empty ? (
          <div className="foot-note" style={{ marginTop: 0 }}>
            Nothing indexed for {ledger.periodLabel.toLowerCase()} yet. Once Claude runs, every
            token in your context window shows up here with its cause.
          </div>
        ) : view === "cause" ? (
          <ByCause ledger={ledger} findings={findings} notes={notes} />
        ) : (
          <OverTime />
        )}
      </div>
    </div>
  );
}
