// Snapshot the REAL database into the dev-server fixture.
//
// The dev server (`npm run dev:mock`) runs in a plain browser and cannot call
// Tauri, so it needs data from somewhere. That "somewhere" used to be numbers
// typed by hand, which drift: a hand-written ledger fixture disagreed with the
// real one and hid a bug where the headroom multiplier read 1.59x instead of
// 1.35x for a whole review cycle.
//
// So the fixture is generated, never authored. Everything below comes out of
// `piggy ledger --json` and `piggy insights --json` against ~/.piggy/piggy.db.
// If the numbers on screen look wrong, they are wrong in the product too.
//
//   node scripts/snapshot-dev-data.mjs [--since 2026-07-24]

import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const piggy = join(repo, "target", "release", "piggy");
const out = join(here, "..", "src", "dev-snapshot.json");

const sinceArg = process.argv.indexOf("--since");
// Full ISO timestamp, not a date: `piggy stats --period week` cuts at
// now-minus-7-days to the second, and a date-only cutoff pulled in an extra
// day. The grid then reported 10,177 sessions beside a header saying 7,789.
const since =
  sinceArg > -1 ? process.argv[sinceArg + 1] : new Date(Date.now() - 7 * 864e5).toISOString();

function piggyJson(cmd) {
  try {
    return JSON.parse(execFileSync(piggy, [cmd, "--since", since, "--json"], { maxBuffer: 64 << 20 }));
  } catch (e) {
    console.error(`\n${piggy} ${cmd} failed. Build it first:\n  cargo build --release --bin piggy\n`);
    throw e;
  }
}

const ledger = piggyJson("ledger");
const insights = piggyJson("insights");

// `piggy stats` takes --period, not --since; and the source grid has no CLI
// command at all, so it comes straight out of the DB. Both are real: the point
// of this script is that NOTHING on the dev dashboard is invented.
const stats = JSON.parse(
  execFileSync(piggy, ["stats", "--period", "week", "--json"], { maxBuffer: 64 << 20 }),
).week;
const report = JSON.parse(execFileSync(piggy, ["report", "--json"], { maxBuffer: 64 << 20 }));

const db = join(process.env.HOME, ".piggy", "piggy.db");
// Mirrors `Store::by_source` exactly: INNER join (a session with no model rows
// has no tokens and is not a data point) and a window on `ended_at`, which is
// what every stats query uses. A left join on started_at instead put 10,115
// sessions in the tool tile beside a header reading 7,789.
const sql = `SELECT s.source, s.interface, COUNT(DISTINCT s.session_id),
       COALESCE(SUM(m.input_tokens+m.output_tokens+m.cache_creation_tokens+m.cache_read_tokens),0),
       COALESCE(SUM(m.cost_usd_est),0)
     FROM sessions s JOIN session_models m ON m.session_id = s.session_id
     WHERE COALESCE(s.ended_at,'') >= '${since}'
     GROUP BY s.source, s.interface`;
const grid = execFileSync("sqlite3", ["-separator", "\t", db, sql], { maxBuffer: 64 << 20 })
  .toString()
  .trim()
  .split("\n")
  .filter(Boolean)
  .map((l) => l.split("\t"));

const CELLS = [
  ["claude-code", "gui"],
  ["claude-code", "tui"],
  ["codex", "gui"],
  ["codex", "tui"],
];
const cells = CELLS.map(([source, iface]) => {
  const hit = grid.find((g) => g[0] === source && g[1] === iface);
  return {
    source,
    interface: iface,
    sessions: hit ? Number(hit[2]) : 0,
    totalTokens: hit ? Number(hit[3]) : 0,
    costUsdEst: hit ? Number(hit[4]) : 0,
    toolPresent: true,
  };
});
const unknown = grid.filter((g) => !CELLS.some(([s, i]) => s === g[0] && i === g[1]));

const leaf = (p) => p.split("/").filter(Boolean).pop() ?? p;

const snapshot = {
  // Stamped so a stale fixture is visible rather than silently old.
  generatedAt: new Date().toISOString(),
  since,
  ledger: {
    periodLabel: "Last 7 days",
    totalTokens: ledger.total_tokens,
    removableTokens: ledger.removable_tokens,
    overhead: ledger.overhead,
    headroom: ledger.headroom ?? null,
    removableShare: ledger.removable_cost_share,
    sessions: ledger.projects.reduce((n, p) => n + p.sessions, 0),
    sources: ledger.sources.map((s) => ({
      kind: s.kind,
      label: s.label,
      tokens: s.tokens,
      share: s.share,
      removable: s.removable,
      isFloor: s.is_floor,
      estimated: s.estimated,
    })),
    projects: ledger.projects.map((p) => ({
      project: p.project,
      name: leaf(p.project),
      sessions: p.sessions,
      msgsPerSession: p.msgs_per_session,
      floorTokens: p.floor_tokens,
      workTokens: p.work_tokens,
      overhead: p.overhead,
    })),
    empty: ledger.total_tokens === 0,
  },
  insights,
  stats: {
    periodLabel: "Last 7 days",
    streams: {
      input: stats.input_tokens,
      output: stats.output_tokens,
      cacheWrite: stats.cache_creation_tokens,
      cacheRead: stats.cache_read_tokens,
    },
    totalTokens:
      stats.input_tokens + stats.output_tokens + stats.cache_creation_tokens + stats.cache_read_tokens,
    sessions: stats.sessions,
    costUsdEst: stats.cost_usd_est,
    costEstimated: stats.cost_estimated,
    // False whenever any model is missing from the pricing table — today
    // `claude-opus-5` is, and it is most of the volume.
    fullyPriced: (stats.unpriced_tokens ?? 0) === 0,
    unpricedTokens: stats.unpriced_tokens ?? 0,
    headline: {
      value: report.headline.multiplier,
      label:
        report.headline.multiplier == null
          ? "not_enough_data"
          : report.headline.observational
            ? "estimated"
            : "measured",
      // Mirrors `map_headline`: `nHoldout` is the holdout count, so it is 0 when
      // the baseline is the pre-install history rather than a holdout. It used
      // to carry `nBaseline` regardless, which put thousands of "holdout
      // sessions" on the dev dashboard for a run that had none.
      nHoldout: report.headline.baseline === "holdout" ? report.headline.nBaseline : 0,
      note: report.headline.note ?? null,
      // The experiment behind the number, for the Proof screen.
      nFullOn: report.headline.nFullOn,
      nBaseline: report.headline.nBaseline,
      baselineKind: report.headline.baseline,
      onRandomized: report.headline.onRandomized,
      baselineClean: report.headline.baselineClean,
      minGroup: report.headline.minGroup,
      turns: report.headline.turns
        ? {
            stream: report.headline.turns.stream,
            kind: report.headline.turns.badge,
            nOn: report.headline.turns.nOn,
            nOff: report.headline.turns.nOff,
            medianOn: report.headline.turns.medianOn,
            medianOff: report.headline.turns.medianOff,
            delta:
              report.headline.turns.deltaPct == null ? null : -report.headline.turns.deltaPct / 100,
          }
        : null,
      streams: report.headline.streams.map((s) => ({
        stream: s.stream,
        kind: s.badge,
        nOn: s.nOn,
        nOff: s.nOff,
        medianOn: s.medianOn,
        medianOff: s.medianOff,
        // The CLI reports positive-is-a-saving percent; the GUI's convention is
        // a negative fraction (same as `Badge.delta`). Convert once, here.
        delta: s.deltaPct == null ? null : -s.deltaPct / 100,
      })),
    },
  },
  sources: {
    cells,
    unknownTokens: unknown.reduce((n, g) => n + Number(g[3]), 0),
    unknownSessions: unknown.reduce((n, g) => n + Number(g[2]), 0),
  },
};

writeFileSync(out, JSON.stringify(snapshot, null, 2));
console.log(
  `wrote ${out}\n  since ${since} · ${snapshot.ledger.sessions.toLocaleString()} sessions · ` +
    `${snapshot.ledger.totalTokens.toLocaleString()} cache-write tokens · ` +
    `headroom ${snapshot.ledger.headroom?.toFixed(2) ?? "n/a"}x · ${insights.length} insights\n` +
    `  stats: ${snapshot.stats.totalTokens.toLocaleString()} tokens, ${snapshot.stats.sessions.toLocaleString()} sessions, ` +
    `$${snapshot.stats.costUsdEst.toFixed(2)} (${snapshot.stats.unpricedTokens.toLocaleString()} tokens unpriced)`,
);
