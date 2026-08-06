//! The backend bridge: thin, typed wrappers over `piggy-core` that the Tauri
//! command layer calls. Every function here returns a plain `#[derive(Serialize)]`
//! struct (never a raw `serde_json::Value`) so IPC serialization is unaffected by
//! `piggy-core`'s `arbitrary_precision` serde_json feature.
//!
//! ## M3 wiring
//!
//! The measurement milestone (M3) — holdout deltas, the headline multiplier, the
//! discovered feed, rotation, the session watcher — is live in `piggy-core`.
//! Every seam that used to degrade to an honest fallback now consumes the real
//! API:
//!
//! * per-saver badge  → [`attribution::attribute`] (measured / estimated / measuring),
//! * headline         → [`attribution::headline`] (holdout-backed multiplier),
//! * discovered feed  → [`discovery::discover`] (cached GitHub search, ≤1/day),
//! * preferences      → the `piggy-core` [`PiggyState`] `settings` ledger,
//! * background loop  → [`piggy_core::SessionWatcher`] + [`rotation::tick_now`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use piggy_core::attribution::{
    self, Badge as CoreBadge, Headline as CoreHeadline, HeadlineBaseline,
    MultiplierState as CoreMultiplierState, SaverAttribution, MIN_GROUP,
};
use piggy_core::advice::{self, GenerateOptions, Params};
use piggy_core::registry::Entry;
use piggy_core::rotation::{self, RotationOutcome};
use piggy_core::store::advice_status;
use piggy_core::{
    claudemd, cli_link, config, diff, discovery, engine, probe, stats::Period, sweep, tagging,
    ActionKind, Candidate, Catalog, McpManifest, PiggyState, Pricing, Store,
};

/// A time-derived bootstrap seed for the attribution CIs (production runs use a
/// live seed; the math is otherwise deterministic given it). Mirrors the CLI's
/// `time_seed` so the GUI and `piggy report` agree.
fn time_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1
}

// ---------------------------------------------------------------------------
// Attribution cache
//
// The bootstrap over every indexed session is the daemon's heaviest work, and
// the UI refreshes on every session write. Recomputing it per saver + headline
// on each refresh pegs every core once the session table is large. Instead we
// compute the whole bundle (headline + one attribution per curated saver) once
// per *index version* — bumped whenever indexing or rotation changes the data —
// and hand out a shared snapshot. Repeat refreshes for unchanged data are O(1),
// and a single recompute builds the per-session rate map once and reuses it
// across every saver and the headline (instead of one full scan per call).
// ---------------------------------------------------------------------------

static ATTR_INDEX_VERSION: AtomicU64 = AtomicU64::new(0);
static ATTR_CACHE: Mutex<Option<(u64, Arc<AttrBundle>)>> = Mutex::new(None);
/// Held for the duration of a recompute so concurrent callers queue behind one
/// another instead of each starting their own. `stats_overview` and
/// `savers_list` are issued together on every refresh and both want this bundle,
/// so without it a single refresh ran the whole scan-and-bootstrap twice, in
/// parallel, competing for the same cores.
static ATTR_COMPUTE: Mutex<()> = Mutex::new(());

pub(crate) struct AttrBundle {
    headline: CoreHeadline,
    pub(crate) per_saver: std::collections::HashMap<String, SaverAttribution>,
}

/// Invalidate the attribution cache so the next dashboard read recomputes.
/// Called after anything that changes the session data (indexing, rotation
/// tagging, baseline anchoring) — including the background watcher's incremental
/// re-index, which is the steady-state path once the app is running.
pub fn bump_attr_version() {
    ATTR_INDEX_VERSION.fetch_add(1, Ordering::Relaxed);
}

/// The per-saver attribution + headline for the current index version, computed
/// once and cached. Best-effort: an unreadable store propagates as `Err` and the
/// caller degrades to an honest "measuring"/"not_enough_data" rather than crash.
pub(crate) fn attribution_bundle() -> anyhow::Result<Arc<AttrBundle>> {
    let version = ATTR_INDEX_VERSION.load(Ordering::Relaxed);
    if let Some(bundle) = attr_cached(Some(version)) {
        return Ok(bundle);
    }

    // Stale-while-revalidate. The watcher bumps the version on every session
    // write, so a strict cache made the *typical* refresh - the one right after
    // Claude wrote a line - pay the full rescan while the UI sat on a spinner.
    // A bundle one index version old is worth far more here than a two-second
    // wait: serve it, recompute once in the background, and the next refresh
    // (the watcher fires plenty) picks up the new numbers.
    if let Some(stale) = attr_cached(None) {
        // A revalidation already running holds ATTR_COMPUTE, so a burst of
        // refreshes queues one recompute rather than a thread each. A poisoned
        // lock reads as idle: `attr_recompute` takes it anyway.
        let busy = matches!(
            ATTR_COMPUTE.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        );
        if !busy {
            std::thread::spawn(|| {
                let _ = attr_recompute();
            });
        }
        return Ok(stale);
    }

    // Cold start: nothing to serve, so this one call waits.
    attr_recompute()
}

/// The cached bundle, either for `version` specifically or whatever is there.
fn attr_cached(version: Option<u64>) -> Option<Arc<AttrBundle>> {
    let guard = ATTR_CACHE.lock().ok()?;
    let (v, bundle) = guard.as_ref()?;
    match version {
        Some(want) if want != *v => None,
        _ => Some(bundle.clone()),
    }
}

fn attr_recompute() -> anyhow::Result<Arc<AttrBundle>> {
    // Single-flight. The second caller blocks here rather than racing the first
    // through the same scan, then finds the finished bundle on the re-check
    // below and returns it. `lock()` only fails if a previous holder panicked
    // mid-recompute; that poisons nothing we read, so take the guard anyway and
    // let this call rebuild the cache.
    let _compute = ATTR_COMPUTE.lock().unwrap_or_else(|e| e.into_inner());
    // Re-read the version rather than reusing the one from before the wait: a
    // re-index may have landed while we queued, and computing against a version
    // we already know is stale would cache under it and miss immediately.
    let version = ATTR_INDEX_VERSION.load(Ordering::Relaxed);
    if let Some(bundle) = attr_cached(Some(version)) {
        return Ok(bundle);
    }

    let home = config::piggy_home();
    let store = Store::open(&home)?;
    let pricing = Pricing::load(&home);
    let catalog = Catalog::embedded();
    let seed = time_seed();
    // One full-table scan for the whole bundle.
    let rate_map = store.session_rate_map(&pricing)?;
    let headline = attribution::headline_with_map(&store, &rate_map, seed)?;
    let mut per_saver = std::collections::HashMap::new();
    for e in curated_installable(&catalog) {
        if let Ok(attr) = attribution::attribute_with_map(&store, &rate_map, &e.id, seed) {
            per_saver.insert(e.id.clone(), attr);
        }
    }
    let bundle = Arc::new(AttrBundle {
        headline,
        per_saver,
    });
    if let Ok(mut guard) = ATTR_CACHE.lock() {
        *guard = Some((version, bundle.clone()));
    }
    Ok(bundle)
}

// ---------------------------------------------------------------------------
// State write lock
//
// `PiggyState::save` is atomic per write but has no compare-and-swap: it replaces
// the whole document, so two writers that each loaded before either saved leave
// only the last one's changes. This process has more than one writer - the
// background watcher thread steps the scheduler (`rotation_tick_if_enabled` ->
// `rotation::tick_now`, which loads and saves on its own) while the user is free
// to sweep, flip a saver or change settings - and the sweep pass holds its
// read-modify-write open across two full scans of the session store, which is
// long enough for a tick to land inside it. Losing that tick is not a lost
// preference: it puts Piggy's ledger and Claude's settings.json out of step, and
// every session after it is attributed to a saver set that is not the one
// running. Every mutator below takes this guard for its whole cycle so the
// in-process writers queue instead of clobbering one another, and loads state as
// late as it can so the window a separate `piggy` CLI process could still slip
// into is one write wide.
// ---------------------------------------------------------------------------

static STATE_WRITE: Mutex<()> = Mutex::new(());

/// Take [`STATE_WRITE`] for a whole read-modify-write of `state.json`. A poisoned
/// lock reads as free, the way `attr_recompute` treats its compute guard: the
/// panic belongs to the writer that hit it, and refusing every later write would
/// wedge the app until a restart.
fn state_write() -> std::sync::MutexGuard<'static, ()> {
    STATE_WRITE.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Error payload (plain-language; never raw JSON in the UI)
// ---------------------------------------------------------------------------

/// A user-facing error surfaced as a red inline banner. `detail` is always an
/// English sentence (engine errors already read this way); the UI never shows a
/// raw error object.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub title: String,
    pub detail: String,
    pub rolled_back: bool,
}

impl ApiError {
    pub fn new(title: &str, detail: impl Into<String>, rolled_back: bool) -> Self {
        ApiError {
            title: title.into(),
            detail: detail.into(),
            rolled_back,
        }
    }
}

/// Map a low-level `anyhow` error to a generic, plain-language banner payload.
fn generic(title: &str) -> impl FnOnce(anyhow::Error) -> ApiError + '_ {
    move |e| ApiError::new(title, first_sentence(&e.to_string()), false)
}

/// Trim a chained error message to its leading, most human portion.
fn first_sentence(s: &str) -> String {
    s.split(':').next().unwrap_or(s).trim().to_string()
}

/// Longest per-item failure reason the banner will print.
const REASON_MAX: usize = 160;

/// One per-item failure reason, whole.
///
/// Deliberately not [`first_sentence`]: that cuts at the first colon, and the
/// advice engine's refusals put the part that matters *after* one - "nothing to
/// write for {path}: turn on the local advisor in Settings for a drafted
/// rewrite". Through `generic` the reader gets a bare file path and no reason at
/// all. This keeps the colon and the whole first line, drops a trailing period
/// so it can be joined into the banner's own sentence, and caps the length so
/// one runaway chain cannot push the rest of a bundle's failures off the screen.
fn one_sentence(e: &anyhow::Error) -> String {
    let raw = e.to_string();
    let line = raw.lines().next().unwrap_or("").trim();
    let line = line.strip_suffix('.').unwrap_or(line);
    if line.chars().count() <= REASON_MAX {
        return line.to_string();
    }
    let cut: String = line.chars().take(REASON_MAX).collect();
    format!("{}…", cut.trim_end())
}

/// An engine note as a standalone sentence: capital in, full stop on. The
/// wording stays the engine's.
fn sentence(s: &str) -> String {
    let mut chars = s.chars();
    let mut out = String::with_capacity(s.len() + 1);
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    if !out.is_empty() && !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// The date half of an RFC3339 stamp. A row has no room for a timestamp, and
/// the minute a probe ran is not a fact anybody acts on.
fn day(ts: &str) -> &str {
    ts.split('T').next().unwrap_or(ts)
}

// ---------------------------------------------------------------------------
// Period helpers
// ---------------------------------------------------------------------------

pub(crate) fn period_from(s: &str) -> Period {
    match s {
        "today" => Period::Today,
        "week" => Period::Week,
        "month" => Period::Month,
        _ => Period::All,
    }
}

fn period_key(p: Period) -> &'static str {
    match p {
        Period::Today => "today",
        Period::Week => "week",
        Period::Month => "month",
        Period::All => "all",
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn fmt_md(d: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    format!("{} {}", MONTHS[d.month0() as usize], d.day())
}

/// The date-range label shown on the share card (e.g. `Jul 6 – Jul 12`).
fn date_range_label(p: Period) -> String {
    let today = chrono::Local::now().date_naive();
    match p {
        Period::Today => fmt_md(today),
        Period::Week => format!(
            "{} – {}",
            fmt_md(today - chrono::Duration::days(6)),
            fmt_md(today)
        ),
        Period::Month => format!(
            "{} – {}",
            fmt_md(today - chrono::Duration::days(29)),
            fmt_md(today)
        ),
        Period::All => "All time".to_string(),
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn session_count() -> u64 {
    Store::open(&config::piggy_home())
        .and_then(|s| s.session_count())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// environment (empty-state routing)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    /// Claude Code appears to be present on this machine.
    pub claude_installed: bool,
    /// Codex appears to be present on this machine (`~/.codex` exists).
    pub codex_installed: bool,
    /// At least one session has been indexed.
    pub has_data: bool,
    pub sessions: u64,
}

pub fn environment() -> Environment {
    let claude_installed = config::claude_dir().exists() || config::claude_projects_dir().exists();
    let codex_installed = config::codex_dir().exists();
    let sessions = session_count();
    Environment {
        claude_installed,
        codex_installed,
        has_data: sessions > 0,
        sessions,
    }
}

// ---------------------------------------------------------------------------
// stats_overview
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Streams {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

/// One stream's side of the headline comparison, as the Proof screen draws it:
/// the two medians it is a ratio of, plus the delta the core is willing to show.
///
/// The medians are the load-bearing part. `docs/measurement.md` says to show the
/// measured per-stream percentages *before* the price-weighted `×`, and a
/// percentage with nothing behind it is still a number the user has to take on
/// faith. With both medians the screen can draw the comparison the delta came
/// from, and an arm with no sessions is visibly an empty bar rather than a
/// missing sentence.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlineStream {
    /// `"input" | "output" | "cache write" | "cache read"`.
    pub stream: String,
    /// `"measured" | "estimated" | "measuring"` — this stream's own badge, which
    /// is not always the headline's: a stream can settle before the `×` does.
    pub kind: String,
    pub n_on: u64,
    pub n_off: u64,
    /// Median tokens per assistant turn, savers on / savers off. Zero when that
    /// arm has no sessions, which the UI renders as an empty arm rather than a
    /// zero measurement.
    pub median_on: f64,
    pub median_off: f64,
    /// The change as a fraction, **negative = a saving** — the same sign
    /// convention as `Badge.delta` (see `saver_badge`), because both cross this
    /// boundary in one payload and a UI that flipped one and not the other would
    /// render a saving as a regression.
    ///
    /// **Gated on the badge** (`StreamStat::shown_pct`), so a stream still
    /// `measuring` sends `null` rather than a point estimate the UI might print
    /// anyway. Note that `shown_pct` also fires for an observational
    /// `estimated` figure, which is why `kind` travels alongside: only a
    /// `measured` kind may be labelled measured in the UI.
    pub delta: Option<f64>,
    /// What the row means when `delta` is null, in one sentence
    /// (`StreamStat::note`): still gathering, too small to compare, measured
    /// and flat, or compared and too noisy. `None` when there is a delta, since
    /// the number says it. Without this the UI printed the same "measuring"
    /// chip over all four, and a settled null result read as a stuck bar.
    pub note: Option<String>,
    /// The state `note` is prose for: `delta` | `waiting` | `quiet` |
    /// `no_change` | `inconclusive`. The UI branches on this, never on the
    /// sentence.
    pub reading: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Headline {
    /// The measured "lasts N× longer" multiplier, or `null` until measured.
    pub value: Option<f64>,
    /// `"measured" | "estimated" | "not_enough_data"`.
    pub label: String,
    pub n_holdout: u64,
    /// Why the figure is only `estimated`, in the user's terms. `None` when the
    /// label is not `estimated`.
    ///
    /// The reason is not always "no holdout yet": a holdout can exist and still
    /// not back a measured claim, because the savers were pinned on by hand or
    /// because a pinned saver ran through the holdout itself. The frontends
    /// cannot tell those apart from `label` alone, and hard-coding one of them
    /// ("estimated vs your history, holdout measurement in progress") was simply
    /// false for the other two.
    pub note: Option<String>,
    // --- the experiment behind the number -----------------------------------
    //
    // Everything below is what the Proof screen needs to show the *comparison*
    // rather than assert its result. The core has always computed it; this
    // payload used to drop it on the floor, so a screen titled "Proof" could
    // only ever render the conclusion or the word "measuring".
    /// Sessions on the ON arm: your current saver set, running.
    pub n_full_on: u64,
    /// Sessions on the OFF arm, whichever baseline won (`baseline_kind`).
    /// `n_holdout` is the same count *only* when the baseline is the holdout,
    /// and 0 otherwise, so it cannot stand in for this.
    pub n_baseline: u64,
    /// `"holdout" | "pre_install" | "none"` — what the ON arm is being compared
    /// against, which decides whether a measured claim is reachable at all.
    pub baseline_kind: String,
    /// Whether the ON arm is randomized (every saver on because the scheduler
    /// said so). False once it leans on hand-pinned sessions, which is the
    /// blocker the user can actually undo.
    pub on_randomized: bool,
    /// How much of `n_full_on` is measured-eligible: current saver set, every
    /// saver on because the scheduler chose it, before observational sessions
    /// are pooled in.
    ///
    /// The screen cannot tell the two `on_randomized == false` cases apart
    /// without this, and they want opposite words. Zero is "nothing is
    /// rotating", where waiting fixes nothing and the fix is a button. Non-zero
    /// and under `min_group` is "rotation is running and this arm is at N of
    /// 10", where waiting is exactly the fix - and `n_full_on` cannot say so,
    /// because it is the pooled total and sits in the thousands while the arm
    /// that decides the badge holds five.
    pub n_full_on_randomized: u64,
    /// Whether the holdout was a clean all-off one. False when a pinned saver
    /// rode through it, so the no-savers counterfactual was never observed.
    pub baseline_clean: bool,
    /// `"shown" | "no_data" | "withheld_cost_more"` — why `value` is null, when
    /// it is. `note` names one blocker in one sentence and the randomization gap
    /// outranks this one, so without the field the screen could never say that
    /// the estimate was withheld for costing MORE rather than for still
    /// gathering.
    pub multiplier_state: String,
    /// Sessions needed on **each** arm before a measured claim ([`MIN_GROUP`]),
    /// so the UI can draw honest progress instead of hard-coding 10.
    pub min_group: u64,
    /// Per-stream comparison, in display order. Empty when the store had no
    /// attribution bundle to read.
    pub streams: Vec<HeadlineStream>,
    /// Turns per session, on vs off. Not one of `streams`: it is the
    /// denominator they are divided by. A NEGATIVE delta here means the savers
    /// made the agent take more turns, which every per-turn figure is blind to,
    /// so the UI shows it as a regression however good the streams look.
    pub turns: Option<HeadlineStream>,
    /// What the experiment is still waiting for, when it is waiting on sample
    /// size. `None` once both arms are full — at which point "still gathering"
    /// would be the wrong story and the UI must fall back to `note`.
    pub waiting: Option<Waiting>,
    /// Sessions the ON arm gained from an earlier saver set that differed only
    /// by savers measured as doing nothing. 0 in the normal case, where the
    /// current set stands on its own.
    pub n_carried: u64,
    /// The null savers that made the fold-in legal, for the sentence explaining
    /// it. Empty when `n_carried` is 0.
    pub carried_savers: Vec<String>,
}

/// The wait, in the terms the Proof screen needs to explain itself: which arm,
/// how far along, when the count started, and roughly how much longer.
///
/// `since` on the ON arm is the one users cannot deduce for themselves. The ON
/// arm is scoped to the saver set they run *now*, so installing or removing a
/// single saver silently restarts it at zero — which reads as "Piggy is broken,
/// it has said measuring for weeks" unless the screen says the count restarted
/// and when.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Waiting {
    /// `"on" | "baseline"` — which side is short.
    pub arm: String,
    pub have: u64,
    pub need: u64,
    /// RFC3339 timestamp this arm's count started from, when known.
    pub since: Option<String>,
    /// Days left at the pace observed so far. `None` when there is no pace to
    /// extrapolate from, and the UI must say so rather than guess.
    pub days_left: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsOverview {
    pub period: String,
    pub period_label: String,
    pub streams: Streams,
    pub total_tokens: u64,
    pub sessions: u64,
    pub cost_usd_est: f64,
    pub cost_estimated: bool,
    pub fully_priced: bool,
    pub today_tokens: u64,
    pub headline: Headline,
}

pub fn stats_overview(period_s: String) -> Result<StatsOverview, ApiError> {
    (|| -> anyhow::Result<StatsOverview> {
        let home = config::piggy_home();
        let store = Store::open(&home)?;
        let period = period_from(&period_s);
        let t = store.totals(period)?;
        let today = store.totals(Period::Today)?;
        // Real holdout-backed headline, from the cached attribution bundle.
        // Best-effort: an unreadable or dataless store yields an honest
        // "not_enough_data" rather than an error (the token totals above are the
        // load-bearing part of this call).
        let headline = attribution_bundle()
            .map(|b| map_headline(&b.headline))
            .unwrap_or_else(|_| Headline {
                value: None,
                label: "not_enough_data".to_string(),
                n_holdout: 0,
                note: None,
                n_full_on: 0,
                n_baseline: 0,
                baseline_kind: "none".to_string(),
                on_randomized: false,
                n_full_on_randomized: 0,
                baseline_clean: false,
                multiplier_state: "no_data".to_string(),
                min_group: MIN_GROUP as u64,
                streams: Vec::new(),
                turns: None,
                waiting: None,
                n_carried: 0,
                carried_savers: Vec::new(),
            });
        Ok(StatsOverview {
            period: period_key(period).to_string(),
            period_label: period.label().to_string(),
            streams: Streams {
                input: t.input_tokens,
                output: t.output_tokens,
                cache_write: t.cache_creation_tokens,
                cache_read: t.cache_read_tokens,
            },
            total_tokens: t.total_tokens(),
            sessions: t.sessions,
            cost_usd_est: round2(t.cost_usd_est),
            cost_estimated: true,
            fully_priced: t.fully_priced(),
            today_tokens: today.total_tokens(),
            headline,
        })
    })()
    .map_err(generic("Couldn't read your token history"))
}

/// Map the core [`CoreHeadline`] onto the UI payload, following the honesty rules
/// in `docs/measurement.md`:
///
/// * **measured** — a live holdout baseline that meets the sample bar
///   ([`MIN_GROUP`] per side) with a computable multiplier. The Dashboard/Home
///   sub-line reads "measured against N holdout sessions".
/// * **estimated** — no live holdout, but an observational pre-install baseline
///   with a multiplier, meeting the same [`MIN_GROUP`] sample bar as `measured`.
///   Sub-line: "estimated vs your history · holdout measurement in progress".
///   The bar matters as much here as it does for a holdout: "estimated" labels
///   where the *baseline* came from, not how much data backs it, so without it a
///   single pre-install session against a single full-on one would render a
///   confident-looking multiplier the data cannot support.
/// * **not_enough_data** — a partial baseline (1..MIN_GROUP) of either kind, no
///   baseline, or no computable multiplier: never a faked number. `n_holdout`
///   still carries the holdout sessions gathered so far so the UI can show
///   "N of 10".
///
/// A live holdout always wins the baseline in `piggy-core`, so `baseline ==
/// Holdout` ⇔ at least one holdout session exists; hence `n_holdout` is the true
/// holdout count when holding out, and 0 for the pre-install / none cases.
/// One core `StreamStat` onto the wire shape. Shared by the four token streams
/// and the turns arm so they cannot drift in sign or gating.
fn map_stream(s: &piggy_core::attribution::StreamStat) -> HeadlineStream {
    HeadlineStream {
        stream: s.stream.label().to_string(),
        kind: match s.badge {
            CoreBadge::Measured => "measured",
            CoreBadge::Estimated => "estimated",
            CoreBadge::Measuring => "measuring",
        }
        .to_string(),
        n_on: s.n_on as u64,
        n_off: s.n_off as u64,
        median_on: s.median_on,
        median_off: s.median_off,
        delta: s.shown_pct().map(|p| -p / 100.0),
        note: s.note(),
        reading: s.reading().key().to_string(),
    }
}

fn map_headline(hl: &CoreHeadline) -> Headline {
    let n_holdout = if hl.baseline == HeadlineBaseline::Holdout {
        hl.n_baseline as u64
    } else {
        0
    };
    let has_mult = hl.multiplier.is_some();
    let enough = hl.n_full_on >= MIN_GROUP && hl.n_baseline >= MIN_GROUP;
    // `hl.ceiling` is the authority on whether a measured claim is available: it
    // already accounts for a manually-pinned full-on group and for a holdout that
    // had a saver running through it. This used to re-derive "measured" from the
    // baseline kind alone, which is exactly how a manual-on era reached the
    // dashboard labelled measured while the core had it right. Only the sample
    // bar is ours to add.
    let measured = has_mult
        && enough
        && hl.baseline == HeadlineBaseline::Holdout
        && hl.ceiling == CoreBadge::Measured;
    // Still worth showing, just not as measured: either the baseline is the
    // observational pre-install history, or the holdout is real but the ON side
    // is pinned by hand.
    let estimated = has_mult
        && enough
        && !measured
        && matches!(
            hl.baseline,
            HeadlineBaseline::PreInstall | HeadlineBaseline::Holdout
        );
    let label = if measured {
        "measured"
    } else if estimated {
        "estimated"
    } else {
        "not_enough_data"
    };
    // Say which reason applies, in the user's terms. Order matters: a pinned ON
    // side is the thing the user can actually undo, so lead with it.
    let note = if measured {
        None
    } else if !estimated {
        // No publishable multiplier. Two very different reasons land here and want
        // different words: the sample bar not being met yet, versus the data being
        // in but the estimate withheld as implausible. `multiplier_state` carries
        // which, so the sub-line stops always blaming sample size (it read "10 of 10
        // sessions ... no number faked" while the real blocker was suppression).
        let sample_short = hl.n_full_on < MIN_GROUP || hl.n_baseline < MIN_GROUP;
        Some(if !hl.on_randomized && hl.n_full_on_randomized == 0 {
            // The root blocker, when it applies: nothing is being rotated. A setup
            // with every saver pinned by hand gives Piggy no on/off contrast to
            // measure - and its all-off holdouts get contaminated by the still-on
            // pinned savers - so it sits here forever. Name that and the fix, not a
            // session count that implies it is merely gathering.
            "you turned your savers on by hand, so Piggy never switches them off and has \
             nothing to compare against · hand one back to Piggy in the Savers tab"
                .to_string()
        } else if !hl.on_randomized {
            // Rotation IS running, and this arm is short of the bar - the pooled
            // `n_full_on` just hides it, because the sessions Piggy chose the setup
            // for are a handful inside thousands the user switched on by hand. The
            // line above is wrong here in the way that matters most: it tells
            // someone whose experiment is four sessions from settling that nothing
            // is being switched off, and points them at a Savers tab with nothing
            // pinned in it.
            format!(
                "{} of {} sessions Piggy chose the setup for · the other {} ran a set you \
                 turned on by hand, so they cannot back a randomized comparison",
                hl.n_full_on_randomized,
                MIN_GROUP,
                hl.n_full_on.saturating_sub(hl.n_full_on_randomized),
            )
        } else if sample_short {
            // Name the side that is actually short. Hard-coding "N of 10 holdout
            // sessions" reads as nonsense ("15 of 10") whenever the holdout is fine
            // and the full-on side is the thin one, the normal state right after the
            // saver set changes and Piggy restarts counting on the setup you run now.
            if hl.n_baseline < MIN_GROUP {
                format!(
                    "{} of {} holdout sessions so far · no number faked",
                    hl.n_baseline, MIN_GROUP
                )
            } else {
                format!(
                    "{} of {} sessions on your current saver set so far · no number faked",
                    hl.n_full_on, MIN_GROUP
                )
            }
        } else if hl.multiplier_state == CoreMultiplierState::WithheldCostMore {
            // Enough sessions on both sides, but the savers came out behind an
            // observational baseline. Almost always heavier recent work, not a real
            // regression, and Piggy is not rotating the savers so it cannot prove the
            // sign. Say that, rather than "N of N sessions", which reads as "just
            // gathering data" when the data is already in.
            "your recent sessions cost more per turn than your history · likelier heavier \
             work than a regression, so no number is faked · let Piggy rotate savers to \
             measure the sign"
                .to_string()
        } else {
            // Enough sessions but no comparable spend to divide (a side priced to
            // zero). Rare, and honest about it rather than faking a count.
            "enough sessions, but no comparable spend to measure yet · no number faked"
                .to_string()
        })
    } else if !hl.on_randomized && hl.n_full_on_randomized == 0 {
        // Covers a saver switched on by hand AND one switched off by hand: either
        // way Piggy stopped rotating it, which is the part that matters here.
        Some(
            "estimated: you set some savers by hand, so Piggy is not rotating them and \
             cannot measure them"
                .to_string(),
        )
    } else if hl.n_carried > 0 {
        // The ON arm was topped up from an earlier saver set. That is the reason
        // this figure is estimated rather than measured, and it outranks the
        // generic "vs your history" line below, which would name the wrong cause
        // entirely for a headline whose baseline is a perfectly clean holdout.
        //
        // It also has to come before the hand-set branch: a fold-in is only
        // `on_randomized` when *every* carried session was scheduler-chosen
        // (`carried_all_randomized` in `piggy-core`), so a mixed carry drops the
        // flag on a live arm the scheduler chose every session of. Read below
        // that, the same state rendered "the rest you turned on by hand" about
        // sessions Piggy itself chose - they just ran an earlier saver set - and
        // the carry-forward explanation written for it never appeared at all.
        Some(format!(
            "estimated: counting {} sessions from before you changed savers, since {} measured as no change · same setup, different weeks",
            hl.n_carried,
            hl.carried_savers.join(" and "),
        ))
    } else if !hl.on_randomized {
        // Rotation is running; this arm just has not filled yet. Same correction as
        // the no-multiplier branch above: "Piggy is not rotating them" is false here
        // and hides a count that is nearly at the bar. Nothing was carried (the
        // branch above owns that case), so the rest of the pool really is the
        // sessions the user switched on by hand.
        Some(format!(
            "estimated: {} of {} sessions so far ran a setup Piggy chose · the rest you turned \
             on by hand, so they can back an estimate but not a measurement",
            hl.n_full_on_randomized, MIN_GROUP,
        ))
    } else if !hl.baseline_clean && hl.baseline == HeadlineBaseline::Holdout {
        // Only a real holdout baseline can be "dirtied" by a saver running through
        // it. A pre-install baseline is `baseline_clean == false` by definition (it
        // is not a holdout at all), so without the `== Holdout` guard the
        // observational estimate wrongly claimed a saver ran through a holdout that
        // does not exist. It falls through to the honest history note instead.
        Some(
            "estimated: a saver you turned on yourself kept running through the holdout, \
             so it was not a no-savers comparison"
                .to_string(),
        )
    } else {
        Some("estimated vs your history · holdout measurement in progress".to_string())
    };
    Headline {
        value: if label == "not_enough_data" {
            None
        } else {
            hl.multiplier
        },
        label: label.to_string(),
        n_holdout,
        note,
        n_carried: hl.n_carried as u64,
        carried_savers: hl.carried_savers.clone(),
        waiting: hl.waiting().map(|w| Waiting {
            arm: match w.arm {
                piggy_core::attribution::WaitingArm::On => "on",
                piggy_core::attribution::WaitingArm::Baseline => "baseline",
            }
            .to_string(),
            have: w.have as u64,
            need: w.need as u64,
            since: w.since,
            days_left: w.days_left,
        }),
        n_full_on: hl.n_full_on as u64,
        n_baseline: hl.n_baseline as u64,
        baseline_kind: match hl.baseline {
            HeadlineBaseline::Holdout => "holdout",
            HeadlineBaseline::PreInstall => "pre_install",
            HeadlineBaseline::None => "none",
        }
        .to_string(),
        on_randomized: hl.on_randomized,
        n_full_on_randomized: hl.n_full_on_randomized as u64,
        baseline_clean: hl.baseline_clean,
        multiplier_state: match hl.multiplier_state {
            CoreMultiplierState::Shown => "shown",
            CoreMultiplierState::NoData => "no_data",
            CoreMultiplierState::WithheldCostMore => "withheld_cost_more",
        }
        .to_string(),
        min_group: MIN_GROUP as u64,
        turns: Some(map_stream(&hl.turns)),
        // Through `map_stream` like every other stream on the wire: this was a
        // second hand-rolled copy of it, which is how the two came to disagree
        // about a field.
        streams: hl.streams.iter().map(map_stream).collect(),
    }
}

// ---------------------------------------------------------------------------
// sources_overview (per-tool / per-surface observability)
// ---------------------------------------------------------------------------

/// One `(tool, surface)` cell of the observability grid: Claude Code or Codex,
/// via the desktop app / IDE (gui) or the terminal (tui). Tokens are measured
/// from the tool's own session logs; cost is always an estimate.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceCell {
    /// `"claude-code"` | `"codex"`.
    pub source: String,
    /// `"gui"` | `"tui"`.
    pub interface: String,
    pub sessions: u64,
    pub total_tokens: u64,
    pub cost_usd_est: f64,
    /// True when the tool looks installed on this machine, so the UI can say
    /// "nothing yet" (installed, no sessions in window) vs "not detected".
    pub tool_present: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcesOverview {
    pub period: String,
    /// The four canonical cells (Claude Code / Codex × App / Terminal), always
    /// present and zero-filled, in that fixed order.
    pub cells: Vec<SourceCell>,
    /// Tokens from sessions whose surface couldn't be classified (old logs
    /// without a client marker, exotic clients). Shown honestly, never folded
    /// into a guessed bucket.
    pub unknown_tokens: u64,
    pub unknown_sessions: u64,
}

pub fn sources_overview(period_s: String) -> Result<SourcesOverview, ApiError> {
    (|| -> anyhow::Result<SourcesOverview> {
        let home = config::piggy_home();
        let store = Store::open(&home)?;
        let period = period_from(&period_s);
        let rows = store.by_source(period)?;

        let claude_present =
            config::claude_dir().exists() || config::claude_projects_dir().exists();
        let codex_present = config::codex_dir().exists();

        let mut cells: Vec<SourceCell> = [
            ("claude-code", "gui", claude_present),
            ("claude-code", "tui", claude_present),
            ("codex", "gui", codex_present),
            ("codex", "tui", codex_present),
        ]
        .iter()
        .map(|(source, interface, present)| SourceCell {
            source: source.to_string(),
            interface: interface.to_string(),
            sessions: 0,
            total_tokens: 0,
            cost_usd_est: 0.0,
            tool_present: *present,
        })
        .collect();

        let mut unknown_tokens = 0u64;
        let mut unknown_sessions = 0u64;
        for row in rows {
            match cells
                .iter_mut()
                .find(|c| c.source == row.source && c.interface == row.interface)
            {
                Some(cell) => {
                    cell.sessions = row.totals.sessions;
                    cell.total_tokens = row.totals.total_tokens();
                    cell.cost_usd_est = round2(row.totals.cost_usd_est);
                }
                None => {
                    unknown_tokens += row.totals.total_tokens();
                    unknown_sessions += row.totals.sessions;
                }
            }
        }

        Ok(SourcesOverview {
            period: period_key(period).to_string(),
            cells,
            unknown_tokens,
            unknown_sessions,
        })
    })()
    .map_err(generic("Couldn't read your per-tool history"))
}

// ---------------------------------------------------------------------------
// usage_series (day-over-day analytics)
// ---------------------------------------------------------------------------

/// One UTC calendar day of usage, with the four token streams kept separate so
/// the UI can chart them and derive cache efficiency. Cost is always an estimate.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyPoint {
    /// `YYYY-MM-DD` (UTC).
    pub date: String,
    pub total_tokens: u64,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub cost_usd_est: f64,
    pub sessions: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSeries {
    pub period: String,
    pub period_label: String,
    /// Oldest day first, zero-filled so the day-over-day series is continuous.
    pub points: Vec<DailyPoint>,
}

/// The day-over-day usage series for the window: per-day token streams, cost,
/// and session counts. The token-maximization rollups (cache-hit rate, busiest
/// day, trend) are derived from these points in the UI so they stay testable and
/// the payload stays small.
pub fn usage_series(period_s: String) -> Result<UsageSeries, ApiError> {
    (|| -> anyhow::Result<UsageSeries> {
        let store = Store::open(&config::piggy_home())?;
        let period = period_from(&period_s);
        let points = store
            .daily_series(period)?
            .into_iter()
            .map(|r| DailyPoint {
                date: r.date,
                total_tokens: r.totals.total_tokens(),
                input: r.totals.input_tokens,
                output: r.totals.output_tokens,
                cache_write: r.totals.cache_creation_tokens,
                cache_read: r.totals.cache_read_tokens,
                cost_usd_est: round2(r.totals.cost_usd_est),
                sessions: r.totals.sessions,
            })
            .collect();
        Ok(UsageSeries {
            period: period_key(period).to_string(),
            period_label: period.label().to_string(),
            points,
        })
    })()
    .map_err(generic("Couldn't read your day-over-day usage"))
}

// ---------------------------------------------------------------------------
// savers_list / toggles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Badge {
    /// `"measured" | "estimated" | "measuring" | "claimed"`.
    ///
    /// * `measured`  — a randomized holdout/single-off A/B delta that cleared the
    ///   confidence bar (the only green claim).
    /// * `estimated` — the same delta math against the observational pre-install
    ///   baseline; shown with a number but never conflated with measured.
    /// * `measuring` — below the bar: honest session progress, no point estimate.
    /// * `claimed`   — the author's own number (install card only, never here).
    pub kind: String,
    /// Delta fraction (negative = saving), or `null` while still measuring.
    pub delta: Option<f64>,
    /// Sessions backing the figure (measured/estimated) or counted so far
    /// (measuring).
    pub n: u64,
    /// The two arms behind `n`, sent separately because the sum hides the thing
    /// the chip's progress bar is about: promotion needs `MIN_GROUP` on **both**
    /// sides, so a 14-on / 0-off split is `n = 14` and paints a full bar that can
    /// never settle. The bar fills on the weaker arm.
    pub n_on: u64,
    pub n_off: u64,
    /// Why an *enabled* saver is stuck at `measuring`, in the user's terms (a
    /// required binary missing, rotation off, or pinned on by hand). `None` for a
    /// settled badge, a disabled saver, or the ordinary warm-up - the chip's
    /// progress bar already says "n of 10" there. Mirrors `Headline::note`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaverRow {
    pub id: String,
    pub name: String,
    pub plain_label: Option<String>,
    pub description: String,
    pub install_type: String,
    pub status: String,
    pub default_on: bool,
    pub installed: bool,
    pub enabled: bool,
    /// True when the last toggle was a manual (hand) choice, which pauses this
    /// saver from rotation. The UI shows an "un-pin" action so measurement can
    /// resume - without it, a saver touched in the UI is measured never again.
    pub pinned: bool,
    pub installable: bool,
    pub behavior_changing: bool,
    pub warning: Option<String>,
    pub risk: Option<String>,
    pub claimed_savings: Option<String>,
    pub license: String,
    pub license_note: Option<String>,
    pub ordering: i64,
    pub badge: Badge,
    /// The same four token streams the headline breaks out, for this saver alone
    /// (`badge` is only the output one). Same wire shape and same per-stream
    /// gating, so a stream still `measuring` sends no number here either. Empty
    /// when the saver has no attribution yet.
    pub streams: Vec<HeadlineStream>,
    /// Turns per session, on vs off - the denominator the four streams divide
    /// by. Separate for the same reason as on the headline: a saver that buys
    /// cheaper turns by needing more of them looks green on every stream above.
    pub turns: Option<HeadlineStream>,
    /// The one-line learning across all five arms (`SaverAttribution::summary`),
    /// so the panel leads with the finding instead of making the reader derive
    /// it from five rows of medians. `None` with no attribution yet.
    pub summary: Option<String>,
    /// What the summary does not cover (`SaverAttribution::caveat`): a thin arm,
    /// or an uncomparable turn count under a per-turn saving. `None` when the
    /// comparison hides nothing. Deterministic, so it renders whether or not the
    /// local advisor is switched on.
    pub caveat: Option<String>,
    /// True when the saver exposes user-tunable options (a Configure control).
    pub configurable: bool,
    /// Wrapper-model savers only: the command that starts a Claude session
    /// through this saver (e.g. Headroom's `piggy-claude`). `None` when the
    /// saver applies to every session. The UI renders a copyable launch
    /// instruction when set.
    pub launch_command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaversState {
    pub master_on: bool,
    pub savers: Vec<SaverRow>,
    /// A one-line, plain-language heads-up produced by the last mutation - e.g.
    /// when turning the master switch on auto-disabled a conflicting saver.
    /// `None` on plain reads (`savers_list`), so the UI only flashes it after an
    /// action the user just took.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// Curated savers this build can actually install (real, known steps). These are
/// the rows shown on Home; everything else lands in Discover.
fn curated_installable(catalog: &Catalog) -> Vec<&Entry> {
    catalog
        .ordered()
        .into_iter()
        .filter(|e| {
            e.status.starts_with("curated") && e.installable().is_ok() && e.has_install_steps()
        })
        .collect()
}

/// The curated **default-on** set the master switch manages, in `ordering` order.
fn default_on_ids(catalog: &Catalog) -> Vec<String> {
    curated_installable(catalog)
        .into_iter()
        .filter(|e| e.default_on)
        .map(|e| e.id.clone())
        .collect()
}

fn saver_row(
    e: &Entry,
    state: &PiggyState,
    attr: Option<&SaverAttribution>,
    bin_present: &HashMap<&str, bool>,
) -> SaverRow {
    let st = state.savers.get(&e.id);
    SaverRow {
        id: e.id.clone(),
        name: e.name.clone(),
        plain_label: e.plain_label.clone(),
        description: e.description.clone(),
        install_type: e.install_type.clone(),
        status: e.status.clone(),
        default_on: e.default_on,
        installed: st.is_some(),
        enabled: st.map(|s| s.enabled).unwrap_or(false),
        pinned: st.map(|s| s.is_pinned()).unwrap_or(false),
        installable: e.installable().is_ok() && e.has_install_steps(),
        behavior_changing: e.behavior_changing,
        warning: e.warning.clone(),
        risk: e.risk.clone(),
        claimed_savings: e.claimed_savings.clone(),
        license: e.license.clone(),
        license_note: e.license_note.clone(),
        ordering: e.ordering,
        badge: {
            let mut b = attr.map(saver_badge).unwrap_or(Badge {
                kind: "measuring".to_string(),
                delta: None,
                n: 0,
                n_on: 0,
                n_off: 0,
                note: None,
            });
            if b.kind == "measuring" {
                // First required binary that's now missing (soft-required ones
                // install anyway, so this can happen at runtime).
                let missing_binary = e
                    .required_binaries()
                    .into_iter()
                    .find(|(bin, _)| !bin_present.get(bin).copied().unwrap_or(true));
                b.note = measuring_note(
                    st.map(|s| s.enabled).unwrap_or(false),
                    st.and_then(|s| s.last_toggle_source.as_deref()),
                    state.settings.holdout_enabled,
                    missing_binary,
                );
            }
            b
        },
        streams: attr
            .map(|a| a.streams.iter().map(map_stream).collect())
            .unwrap_or_default(),
        turns: attr.map(|a| map_stream(&a.turns)),
        summary: attr.map(|a| a.summary()),
        caveat: attr.and_then(|a| a.caveat()),
        configurable: !e.config_options.is_empty(),
        launch_command: e.launch_command(),
    }
}

/// The per-saver row badge, taken from the **output** stream (the headline
/// per-saver figure, per `SaverAttribution::output`). Never blends measured and
/// estimated. The delta is emitted in the UI's sign convention (negative =
/// saving), the inverse of `piggy-core`'s `1 - on/off` (positive = saving).
fn saver_badge(a: &SaverAttribution) -> Badge {
    match a.output() {
        Some(s) => {
            let n = (s.n_on + s.n_off) as u64;
            // `shown_pct` is `Some` only for measured/estimated; it is signed with
            // positive = saving, so negate for the UI's negative-is-saving axis.
            let delta = s.shown_pct().map(|p| -p / 100.0);
            let kind = match s.badge {
                CoreBadge::Measured => "measured",
                CoreBadge::Estimated => "estimated",
                CoreBadge::Measuring => "measuring",
            };
            Badge {
                kind: kind.to_string(),
                delta: if matches!(s.badge, CoreBadge::Measuring) {
                    None
                } else {
                    delta
                },
                n,
                n_on: s.n_on as u64,
                n_off: s.n_off as u64,
                note: None,
            }
        }
        None => Badge {
            kind: "measuring".to_string(),
            delta: None,
            n: 0,
            n_on: 0,
            n_off: 0,
            note: None,
        },
    }
}

/// Why an *enabled* saver's badge still reads `measuring`, phrased for the row so
/// the user knows what to change. `None` when the honest answer is just "needs
/// more sessions" - the chip's progress bar already says that - or when the saver
/// is off. Otherwise the blocked states, most-actionable first: a required binary
/// gone (the saver runs but its hooks degrade or no-op, so there is nothing to
/// measure), rotation turned off globally, or this saver pinned on by hand (so it
/// sits out the A/B rotation that measurement needs). `missing_binary` is the
/// absent `(binary, reason)` from the saver's `require_binary` step, if any.
fn measuring_note(
    enabled: bool,
    toggle_source: Option<&str>,
    holdout_enabled: bool,
    missing_binary: Option<(&str, &str)>,
) -> Option<String> {
    if !enabled {
        return None;
    }
    if let Some((bin, reason)) = missing_binary {
        return Some(format!(
            "Needs {bin}, which isn't installed: {reason}. Install {bin} to fix."
        ));
    }
    if !holdout_enabled {
        return Some(
            "Rotation is off, so Piggy can't run a fair test. Turn on \"Rotate savers for fair tests\" in Settings."
                .to_string(),
        );
    }
    if toggle_source == Some(piggy_core::store::source::MANUAL) {
        return Some(
            "You turned this on by hand, so Piggy never switches it off and has nothing to \
             compare it against. It keeps working; Piggy just can't tell you what it saves."
                .to_string(),
        );
    }
    None
}

/// The master switch is a system-level flag, not a rollup of savers: disabling
/// any single saver leaves Piggy ON. Only the master switch writes it. Legacy
/// state (`None`) falls back to "is anything running" so upgrades read sensibly
/// until the switch is next used.
fn master_is_on(state: &PiggyState) -> bool {
    state
        .master_on
        .unwrap_or_else(|| state.savers.values().any(|s| s.enabled))
}

fn build_savers_state() -> anyhow::Result<SaversState> {
    let catalog = Catalog::embedded();
    let state = PiggyState::load()?;
    // Per-saver attribution comes from the cached bundle (shared store/pricing/seed
    // so every row agrees with `piggy report`). A store failure degrades each row
    // to an honest "measuring".
    let bundle = attribution_bundle().ok();
    let entries = curated_installable(&catalog);
    // Presence of each required binary (python3, node, ...), checked once per
    // distinct binary per list refresh - not per row - since each check spawns a
    // process. A saver whose hooks degrade or no-op without its binary says so.
    let mut bin_present: HashMap<&str, bool> = HashMap::new();
    for e in &entries {
        for (bin, _) in e.required_binaries() {
            bin_present
                .entry(bin)
                .or_insert_with(|| engine::binary_on_path(bin));
        }
    }
    let savers = entries
        .iter()
        .map(|e| {
            let attr = bundle.as_ref().and_then(|b| b.per_saver.get(&e.id));
            saver_row(e, &state, attr, &bin_present)
        })
        .collect();
    Ok(SaversState {
        master_on: master_is_on(&state),
        savers,
        notice: None,
    })
}

/// The ids of every saver currently enabled, as a set for before/after diffing.
fn enabled_ids(state: &PiggyState) -> std::collections::HashSet<String> {
    state
        .savers
        .iter()
        .filter(|(_, s)| s.enabled)
        .map(|(id, _)| id.clone())
        .collect()
}

/// A user-facing label for a saver id: its real name, else the id.
fn friendly_name(catalog: &Catalog, id: &str) -> String {
    catalog
        .get(id)
        .map(|e| e.name.clone())
        .unwrap_or_else(|| id.to_string())
}

/// Build the "we turned these off for you" notice after a mutation, given the
/// savers that were enabled before and the state afterward. `auto_off` is the set
/// of savers that were on before and are off now - each was disabled because a
/// saver Piggy just turned on conflicts with it. Returns `None` when nothing was
/// silently turned off. Best-effort names the saver that replaced each one.
fn conflict_notice(
    catalog: &Catalog,
    before: &std::collections::HashSet<String>,
    after_state: &PiggyState,
) -> Option<String> {
    let after = enabled_ids(after_state);
    let mut auto_off: Vec<&String> = before.difference(&after).collect();
    if auto_off.is_empty() {
        return None;
    }
    auto_off.sort();
    let parts: Vec<String> = auto_off
        .iter()
        .map(|id| {
            let name = friendly_name(catalog, id);
            // Find an enabled saver that conflicts with this one, in either direction.
            let replacer = after.iter().find(|other| {
                let declared_here = catalog
                    .get(other)
                    .map(|e| e.conflicts_with.iter().any(|c| c == *id))
                    .unwrap_or(false);
                let declared_there = catalog
                    .get(id)
                    .map(|e| e.conflicts_with.iter().any(|c| c == *other))
                    .unwrap_or(false);
                declared_here || declared_there
            });
            match replacer {
                Some(other) => format!(
                    "{name} turned off - {} does the same job and is now on.",
                    friendly_name(catalog, other)
                ),
                None => format!("{name} turned off - it conflicts with a saver that's now on."),
            }
        })
        .collect();
    Some(parts.join(" "))
}

/// The "how do I actually use it" heads-up for wrapper-model savers: they only
/// apply to sessions started with their launch command, so every turn-on
/// repeats the instruction. `None` for savers without a launch command.
fn launch_notice(catalog: &Catalog, id: &str) -> Option<String> {
    let e = catalog.get(id)?;
    let cmd = e.launch_command()?;
    Some(format!(
        "{} is on. It saves only in sessions you start with {cmd}. Plain claude sessions are untouched.",
        e.name
    ))
}

pub fn savers_list() -> Result<SaversState, ApiError> {
    build_savers_state().map_err(generic("Couldn't read your savers"))
}

/// Turn a single saver on or off. `on` when not installed installs it (with the
/// engine's own health-check + rollback); `off` uses the fast A/B disable path.
pub fn saver_toggle(id: String, on: bool) -> Result<SaversState, ApiError> {
    let _guard = state_write();
    let catalog = Catalog::embedded();
    let before_enabled = PiggyState::load()
        .map(|s| enabled_ids(&s))
        .unwrap_or_default();
    let installed = PiggyState::load()
        .map(|s| s.is_installed(&id))
        .unwrap_or(false);

    let result = if on {
        if installed {
            engine::set_enabled(&catalog, &id, true)
        } else {
            engine::install(&catalog, &id)
        }
    } else if installed {
        engine::set_enabled(&catalog, &id, false)
    } else {
        // Nothing to do.
        return build_savers_state().map_err(generic("Couldn't read your savers"));
    };

    match result {
        Ok(report) if report.rolled_back => Err(ApiError::new(
            "That saver couldn't be turned on",
            report
                .messages
                .first()
                .cloned()
                .unwrap_or_else(|| "It failed its health check.".to_string()),
            true,
        )),
        Ok(_) => {
            let mut result = build_savers_state().map_err(generic("Couldn't read your savers"))?;
            // Turning a saver on can auto-disable a conflicting one, and a
            // wrapper-model saver only works in sessions started through its
            // launch command - the user must hear about both right away.
            if on {
                let mut parts: Vec<String> = Vec::new();
                if let Ok(after) = PiggyState::load() {
                    parts.extend(conflict_notice(&catalog, &before_enabled, &after));
                }
                parts.extend(launch_notice(&catalog, &id));
                if !parts.is_empty() {
                    result.notice = Some(parts.join(" "));
                }
            }
            Ok(result)
        }
        Err(e) => Err(ApiError::new(
            if on {
                "Couldn't turn that saver on"
            } else {
                "Couldn't turn that saver off"
            },
            first_sentence(&e.to_string()),
            false,
        )),
    }
}

/// Hand a hand-pinned saver back to the scheduler so it can be measured.
///
/// A manual toggle records `source == "manual"`, which pauses the saver from
/// rotation forever (see `rotation::controlled_savers`) - so a saver ever touched
/// in the UI is never measured again, and while pinned ON it also contaminates
/// every all-off holdout. Un-pinning re-stamps the source to `rotation` at the
/// saver's *current* on/off state (no flip now), so the next scheduler tick can
/// start the on/off comparison. This is the missing inverse of the manual toggle.
pub fn saver_unpin(id: String) -> Result<SaversState, ApiError> {
    let _guard = state_write();
    let catalog = Catalog::embedded();
    let enabled = PiggyState::load()
        .ok()
        .and_then(|s| s.savers.get(&id).map(|x| x.enabled))
        .unwrap_or(false);
    engine::set_enabled_src(&catalog, &id, enabled, piggy_core::store::source::ROTATION)
        .map_err(generic("Couldn't hand that saver back to Piggy"))?;
    // Un-pin is a deliberate return to rotation, so clear the resting-choice
    // marker: `SaverState::is_pinned` keys on it, and leaving it set would keep the
    // saver paused. (A holdout override preserves it, on purpose.)
    let mut st = PiggyState::load().map_err(generic("Couldn't read your savers"))?;
    if st.savers.get(&id).and_then(|s| s.manual_enabled).is_some() {
        if let Some(s) = st.savers.get_mut(&id) {
            s.manual_enabled = None;
        }
        st.save().map_err(generic("Couldn't save your savers"))?;
    }
    // Rotation reads the new source on its next tick; invalidate the cached
    // attribution bundle so the dashboard reflects the change on refresh.
    bump_attr_version();
    build_savers_state().map_err(generic("Couldn't read your savers"))
}

/// The master switch. On installs/enables the curated default-on set in
/// `ordering` order; off disables every Piggy-managed saver (kept installed).
pub fn master_toggle(on: bool) -> Result<SaversState, ApiError> {
    let _guard = state_write();
    let catalog = Catalog::embedded();
    // What was on before, so we can tell the user which savers a conflict silently
    // turned off (e.g. enabling the default-on Headroom auto-disables rtk).
    let before_enabled = PiggyState::load()
        .map(|s| enabled_ids(&s))
        .unwrap_or_default();

    if on {
        let ids = default_on_ids(&catalog);
        for id in &ids {
            let installed = PiggyState::load()
                .map(|s| s.is_installed(id))
                .unwrap_or(false);
            let res = if installed {
                engine::set_enabled(&catalog, id, true)
            } else {
                engine::install(&catalog, id)
            };
            match res {
                Ok(report) if report.rolled_back => {
                    return Err(ApiError::new(
                        "Couldn't turn everything on",
                        format!(
                            "\u{201c}{}\u{201d} failed its health check, so it was rolled back. The rest are unchanged.",
                            catalog.get(id).map(|e| e.name.as_str()).unwrap_or(id.as_str())
                        ),
                        true,
                    ));
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(ApiError::new(
                        "Couldn't turn everything on",
                        format!(
                            "\u{201c}{}\u{201d} couldn't turn on: {}",
                            catalog
                                .get(id)
                                .map(|e| e.name.as_str())
                                .unwrap_or(id.as_str()),
                            first_sentence(&e.to_string())
                        ),
                        false,
                    ));
                }
            }
        }
    } else {
        // Disable every installed, enabled, Piggy-managed saver.
        let state = PiggyState::load().map_err(generic("Couldn't read your savers"))?;
        let enabled: Vec<String> = state
            .savers
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(id, _)| id.clone())
            .collect();
        for id in enabled {
            if let Err(e) = engine::set_enabled(&catalog, &id, false) {
                return Err(ApiError::new(
                    "Couldn't turn everything off",
                    format!(
                        "\u{201c}{}\u{201d} couldn't turn off: {}",
                        catalog
                            .get(&id)
                            .map(|e| e.name.as_str())
                            .unwrap_or(id.as_str()),
                        first_sentence(&e.to_string())
                    ),
                    false,
                ));
            }
        }
    }

    // Persist the system switch itself. This is the *only* writer of `master_on`;
    // individual saver toggles deliberately leave it untouched, so disabling one
    // saver never turns Piggy off.
    if let Ok(mut state) = PiggyState::load() {
        state.master_on = Some(on);
        state
            .save()
            .map_err(generic("Couldn't save the master switch"))?;
    }

    let mut result = build_savers_state().map_err(generic("Couldn't read your savers"))?;
    // Only surface the "turned off X" heads-up when turning the master on; turning
    // it off intentionally disables everything, so a diff there is just noise.
    if on {
        let mut parts: Vec<String> = Vec::new();
        if let Ok(after) = PiggyState::load() {
            parts.extend(conflict_notice(&catalog, &before_enabled, &after));
            // Wrapper-model savers in the default-on set (Headroom) need their
            // launch instruction whenever the master switch turns them on.
            for id in default_on_ids(&catalog) {
                if after.savers.get(&id).map(|s| s.enabled).unwrap_or(false) {
                    parts.extend(launch_notice(&catalog, &id));
                }
            }
        }
        if !parts.is_empty() {
            result.notice = Some(parts.join(" "));
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// saver configuration (catalog configOptions)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChoiceDto {
    pub value: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionDto {
    pub key: String,
    pub label: String,
    pub description: String,
    pub choices: Vec<ConfigChoiceDto>,
    pub default: String,
    /// The value in effect now: the saver's own config file wins, then the
    /// user's last choice in Piggy, then the default.
    pub current: String,
}

fn config_dtos(resolved: Vec<piggy_core::saver_config::ResolvedOption>) -> Vec<ConfigOptionDto> {
    resolved
        .into_iter()
        .map(|r| ConfigOptionDto {
            key: r.option.key,
            label: r.option.label,
            description: r.option.description,
            choices: r
                .option
                .choices
                .into_iter()
                .map(|c| ConfigChoiceDto {
                    value: c.value,
                    label: c.label,
                    description: c.description,
                })
                .collect(),
            default: r.option.default,
            current: r.current,
        })
        .collect()
}

/// The options a saver exposes, resolved to their current values.
pub fn saver_config_get(id: String) -> Result<Vec<ConfigOptionDto>, ApiError> {
    (|| -> anyhow::Result<Vec<ConfigOptionDto>> {
        let catalog = Catalog::embedded();
        let state = PiggyState::load().unwrap_or_default();
        Ok(config_dtos(piggy_core::saver_config::get_config(
            &catalog, &state, &id,
        )?))
    })()
    .map_err(generic("Couldn't read that saver's options"))
}

/// Apply one option value and return the re-resolved options.
pub fn saver_config_set(
    id: String,
    key: String,
    value: String,
) -> Result<Vec<ConfigOptionDto>, ApiError> {
    let _guard = state_write();
    (|| -> anyhow::Result<Vec<ConfigOptionDto>> {
        let catalog = Catalog::embedded();
        Ok(config_dtos(piggy_core::saver_config::set_config(
            &catalog, &id, &key, &value,
        )?))
    })()
    .map_err(generic("Couldn't change that setting"))
}

// ---------------------------------------------------------------------------
// sweep
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepItemDto {
    pub idx: usize,
    /// Opaque stable handle the UI passes back to apply/restore.
    pub stable_id: String,
    pub kind: String,
    pub id: String,
    pub source: Option<String>,
    pub used: u64,
    /// `"window" | "lifetime" | "n/a"` — how to read `used`.
    pub used_scope: String,
    pub est_tokens: u64,
    /// Whether `est_tokens` is an estimated count. True for every row Piggy can
    /// produce today (the shipped tokenizer divides bytes by 3.5), which is why
    /// the sheet renders a "~" unconditionally: see
    /// [`piggy_core::sweep::SweepItem::tokens_estimated`].
    pub estimated: bool,
    pub recommend_disable: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepReportDto {
    pub sessions_considered: u64,
    pub est_recoverable_tokens: u64,
    pub estimated: bool,
    pub items: Vec<SweepItemDto>,
}

fn stable_id(kind: &str, id: &str, source: Option<&str>) -> String {
    format!("{kind}|{id}|{}", source.unwrap_or(""))
}

fn used_scope(kind: &str) -> &'static str {
    match kind {
        "mcp" => "window",
        "hook" => "n/a",
        _ => "lifetime",
    }
}

fn dto_from(report: sweep::SweepReport) -> SweepReportDto {
    let est_recoverable = report.est_recoverable_tokens();
    let items: Vec<SweepItemDto> = report
        .items
        .into_iter()
        .map(|i| SweepItemDto {
            idx: i.idx,
            stable_id: stable_id(&i.kind, &i.id, i.source.as_deref()),
            used_scope: used_scope(&i.kind).to_string(),
            kind: i.kind,
            id: i.id,
            source: i.source,
            used: i.used,
            est_tokens: i.est_tokens,
            estimated: i.tokens_estimated,
            recommend_disable: i.recommend_disable,
            reason: i.reason,
        })
        .collect();
    SweepReportDto {
        sessions_considered: report.sessions_considered,
        est_recoverable_tokens: est_recoverable,
        // Derived, never hardcoded. The total is only as exact as the rows
        // behind it, and a probed manifest measured every row would make this
        // false: saying "estimated" over an exact figure is the same class of
        // mislabel as the reverse.
        estimated: items.iter().any(|i| i.estimated),
        items,
    }
}

pub fn sweep_report() -> Result<SweepReportDto, ApiError> {
    (|| -> anyhow::Result<SweepReportDto> {
        let store = Store::open(&config::piggy_home())?;
        Ok(dto_from(sweep::scan(&store, sweep::DEFAULT_N_SESSIONS)?))
    })()
    .map_err(generic("Couldn't scan for unused add-ons"))
}

pub fn sweep_apply(ids: Vec<String>) -> Result<SweepReportDto, ApiError> {
    let _guard = state_write();
    (|| -> anyhow::Result<SweepReportDto> {
        let store = Store::open(&config::piggy_home())?;
        let wanted: HashSet<String> = ids.into_iter().collect();
        // Re-scan between each disable: applying by index is only valid against a
        // fresh scan (indices renumber as items drop out), so we resolve each
        // still-wanted item to its current index one at a time.
        loop {
            let report = sweep::scan(&store, sweep::DEFAULT_N_SESSIONS)?;
            let next = report.items.iter().find(|i| {
                i.kind != "hook" && wanted.contains(&stable_id(&i.kind, &i.id, i.source.as_deref()))
            });
            let Some(item) = next else { break };
            // Load per pass, never once around the loop: `sweep::apply` writes the
            // whole document back, so an instance carried across the scans would
            // undo anything written between them - and each scan re-reads
            // `~/.claude.json`, `settings.json` and the skills dir, which is the
            // slowest stretch in this module.
            let mut state = PiggyState::load()?;
            sweep::apply(&store, &mut state, item.idx, sweep::DEFAULT_N_SESSIONS)?;
        }
        Ok(dto_from(sweep::scan(&store, sweep::DEFAULT_N_SESSIONS)?))
    })()
    .map_err(generic("Couldn't switch those off"))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreFailureDto {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepRestoreDto {
    pub report: SweepReportDto,
    /// Items still on the disabled list because putting them back failed; the
    /// reason names the file and missing key.
    pub failures: Vec<RestoreFailureDto>,
}

/// Restore all swept items (`piggy-core` has no per-item restore), then re-scan.
pub fn sweep_restore() -> Result<SweepRestoreDto, ApiError> {
    let _guard = state_write();
    (|| -> anyhow::Result<SweepRestoreDto> {
        let mut state = PiggyState::load()?;
        let outcome = sweep::restore_all(&mut state)?;
        state.save()?;
        let store = Store::open(&config::piggy_home())?;
        Ok(SweepRestoreDto {
            report: dto_from(sweep::scan(&store, sweep::DEFAULT_N_SESSIONS)?),
            failures: outcome
                .failures
                .into_iter()
                .map(|f| RestoreFailureDto {
                    id: f.id,
                    reason: f.reason,
                })
                .collect(),
        })
    })()
    .map_err(generic("Couldn't restore those"))
}

// ---------------------------------------------------------------------------
// advice
//
// The app's job here is to render what the engine computed and get out of the
// way. Every figure crosses this boundary already formatted and already carrying
// the label that says how it was arrived at; nothing below re-derives a number,
// and nothing below maps a basis string to a different word.
// ---------------------------------------------------------------------------

/// What the reader is told when the plan behind a suggestion has moved.
const ADVICE_GONE: &str =
    "This suggestion is no longer current. Piggy re-checked and the numbers behind it have moved.";

/// What a `stale` row says for itself.
const ADVICE_STALE: &str = "Piggy re-checked and the numbers behind this moved, so the plan no \
                            longer describes your setup. It comes back on the next scan if it \
                            still applies.";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceEvidenceDto {
    pub label: String,
    /// Preformatted by the engine: thousands separators, units, and a leading
    /// `~` on an estimate. Rendered verbatim. The app re-deriving an evidence
    /// figure is how a number and its basis label drift apart.
    pub value: String,
    /// One of `piggy_core::advice::basis`, carried across the wire unchanged:
    /// "observed" | "estimated" | "measured manifest" | "measured" |
    /// "estimated (observational)" | "not enough data yet". The app maps it to a
    /// colour and never to a different word.
    pub basis: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceItemDto {
    pub id: String,
    /// "server-disable" | "server-scope" | "claudemd-fix" | "claudemd-trim" |
    /// "saver-mix" (`ActionKind::as_str`).
    pub kind: String,
    /// "Add-ons" | "CLAUDE.md" | "Savers" (`ActionKind::group_label`).
    pub group: String,
    pub target: String,
    /// The claim, in the registry `plainLabel` voice. The engine writes it.
    pub title: String,
    pub evidence: Vec<AdviceEvidenceDto>,
    pub est_tokens_month: i64,
    /// How to read `est_tokens_month`: "saves" for every kind but
    /// `claudemd-trim`, where it is what the file COSTS and a rewrite could at
    /// best recover part of it. Straight off `ActionKind::est_is_burden`, never
    /// guessed, because "saves 140k" over a figure that is a burden is a claim
    /// Piggy has not measured.
    pub figure_kind: String,
    /// 1 toggle, 2 config move, 3 content edit (`advice::RISK_*`).
    pub risk_tier: u8,
    /// "open" | "applied" | "dismissed" | "stale" (`store::advice_status`).
    pub status: String,
    /// True when this row edits file content and there is a draft for
    /// `advice_diff` to answer with.
    pub has_diff: bool,
    pub applyable: bool,
    /// One plain sentence when `applyable` is false.
    pub blocked_reason: Option<String>,
    /// RFC3339, on applied rows only.
    pub applied_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceReportDto {
    /// Every candidate the engine regenerated, biggest figure first, ties on id.
    /// The engine's order. The app never re-sorts it.
    pub items: Vec<AdviceItemDto>,
    /// Applied rows, newest first, read from the table rather than from `items`:
    /// applying is what stops a candidate regenerating.
    pub applied: Vec<AdviceItemDto>,
    /// What the open list is worth a month: the savings, and nothing else.
    pub est_tokens_month: i64,
    /// The other half, kept apart: what the open items whose figure is a burden
    /// cost today. Never added to the savings - the two summed is roughly a 10x
    /// overstatement in the shape a reader is most likely to believe.
    pub est_tokens_month_burden: i64,
    pub generated_at: String,
    /// False in v1. Model ranking is M5.4's, so the app says "ranked by
    /// estimated tokens a month" rather than implying a model chose.
    pub advisor_ranked: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceFailureDto {
    /// The candidate id for an apply failure; the file, server or saver name for
    /// an undo failure (`advice::UndoFailure::item`).
    pub id: String,
    /// One sentence, already plain, in the engine's own words.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceApplyDto {
    pub report: AdviceReportDto,
    /// Ids that applied, in the order asked.
    pub applied: Vec<String>,
    /// One entry per id that did not. A bundle never fails whole because one
    /// member did.
    pub failures: Vec<AdviceFailureDto>,
    /// `Applied::warnings`, flattened: a conflicting saver switched off, and the
    /// like.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceUndoDto {
    pub report: AdviceReportDto,
    pub restored: usize,
    pub failures: Vec<AdviceFailureDto>,
    /// `Undone::message`, in the engine's words.
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffLineDto {
    /// "ctx" | "add" | "del".
    pub op: String,
    pub text: String,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunkDto {
    pub header: String,
    pub lines: Vec<DiffLineDto>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceDiffDto {
    pub id: String,
    /// `~`-abbreviated, so a screenshot of the sheet does not carry the account
    /// name.
    pub display_path: String,
    pub hunks: Vec<DiffHunkDto>,
    pub added: usize,
    pub removed: usize,
    /// Real byte counts of the two versions: observed.
    pub before_bytes: i64,
    pub after_bytes: i64,
    /// bytes / 3.5, through `claudemd::est_tokens`. Estimated, and labelled so
    /// wherever it is printed.
    pub before_est_tokens: i64,
    pub after_est_tokens: i64,
    pub truncated: bool,
}

/// One candidate as the app sees it.
fn advice_item(c: &Candidate) -> AdviceItemDto {
    AdviceItemDto {
        id: c.id.clone(),
        kind: c.kind.as_str().to_string(),
        group: c.kind.group_label().to_string(),
        target: c.target.clone(),
        title: c.title.clone(),
        evidence: c
            .evidence
            .iter()
            .map(|e| AdviceEvidenceDto {
                label: e.label.clone(),
                // Both verbatim. An evidence value is already formatted and an
                // evidence basis is already chosen; touching either here is the
                // one defect this whole surface exists to avoid.
                value: e.value.clone(),
                basis: e.basis.clone(),
            })
            .collect(),
        est_tokens_month: c.est_tokens_month,
        figure_kind: if c.kind.est_is_burden() {
            "burden"
        } else {
            "saves"
        }
        .to_string(),
        risk_tier: c.risk_tier,
        status: c.status.clone(),
        has_diff: c.kind.edits_content() && c.new_content.is_some(),
        applyable: c.status == advice_status::OPEN && !c.blocked(),
        blocked_reason: advice_blocked_reason(c),
        applied_at: None,
    }
}

/// Why this cannot be applied right now, in one sentence with no colon in it.
fn advice_blocked_reason(c: &Candidate) -> Option<String> {
    if c.status == advice_status::STALE {
        return Some(ADVICE_STALE.to_string());
    }
    if c.blocked() {
        // The engine's own words for what is missing.
        return c.prerequisites.first().map(|p| sentence(p.note()));
    }
    None
}

/// Regenerate every candidate.
///
/// `advice::generate` reconciles the advice table as a side effect (new rows
/// open, vanished open rows stale, spent dismissals retired), which is why there
/// is no separate refresh command: asking for the report IS the refresh.
fn advice_regenerate(
    store: &mut Store,
    catalog: &Catalog,
    pricing: &Pricing,
) -> anyhow::Result<Vec<Candidate>> {
    let state = PiggyState::load()?;
    let opts = GenerateOptions::new(catalog, pricing, &state);
    advice::generate(store, &opts)
}

fn advice_report_dto(store: &Store, candidates: &[Candidate]) -> anyhow::Result<AdviceReportDto> {
    let mut rows = store.advice_by_status(advice_status::APPLIED)?;
    // Newest first: what you just did is what you are most likely to want back.
    rows.sort_by(|a, b| b.applied_at.cmp(&a.applied_at));
    let mut applied = Vec::new();
    for row in &rows {
        // A row whose payload will not parse is a row Piggy cannot describe, and
        // an undescribed entry in an Undo list is worse than no entry.
        let Ok(candidate) = Candidate::from_row(row) else {
            continue;
        };
        let mut dto = advice_item(&candidate);
        dto.applied_at = row.applied_at.clone();
        applied.push(dto);
    }

    // Totals over the OPEN items only, split by the engine's own definition of
    // which figure is a saving and which is a burden.
    let open: Vec<Candidate> = candidates
        .iter()
        .filter(|c| c.status == advice_status::OPEN)
        .cloned()
        .collect();
    Ok(AdviceReportDto {
        items: candidates.iter().map(advice_item).collect(),
        applied,
        est_tokens_month: advice::total_savings(&open),
        est_tokens_month_burden: advice::total_burden(&open),
        generated_at: chrono::Utc::now().to_rfc3339(),
        advisor_ranked: false,
    })
}

pub fn advice_report() -> Result<AdviceReportDto, ApiError> {
    (|| -> anyhow::Result<AdviceReportDto> {
        let home = config::piggy_home();
        let mut store = Store::open(&home)?;
        let catalog = Catalog::embedded();
        let pricing = Pricing::load(&home);
        let candidates = advice_regenerate(&mut store, &catalog, &pricing)?;
        advice_report_dto(&store, &candidates)
    })()
    .map_err(generic("Couldn't work out what to suggest"))
}

/// The proposed edit to one CLAUDE.md, as structured diff rows.
///
/// Regenerates rather than reading the stored row: `Candidate::new_content` is
/// never serialized (CLAUDE.md contents stay out of the database), so the draft
/// exists only in the list this call computes. An id that is no longer in that
/// list is a plan whose evidence has moved, and the id hash is what proves it.
pub fn advice_diff(id: String) -> Result<AdviceDiffDto, ApiError> {
    let title = "Couldn't show the changes";
    let home = config::piggy_home();
    let candidates = (|| -> anyhow::Result<Vec<Candidate>> {
        let mut store = Store::open(&home)?;
        let catalog = Catalog::embedded();
        let pricing = Pricing::load(&home);
        advice_regenerate(&mut store, &catalog, &pricing)
    })()
    .map_err(generic(title))?;

    let candidate = candidates
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::new(title, ADVICE_GONE, false))?;
    let Params::Claudemd { path } = &candidate.params else {
        return Err(ApiError::new(
            title,
            "That suggestion changes a setting rather than a file, so there is nothing to show.",
            false,
        ));
    };
    let Some(after) = candidate.new_content.as_deref() else {
        // A drafting kind with no draft yet. The engine already knows what is
        // missing; say that rather than inventing a reason.
        let note = candidate
            .prerequisites
            .first()
            .map(|p| p.note())
            .unwrap_or("there is no drafted replacement for this file");
        return Err(ApiError::new("No draft yet", sentence(note), false));
    };
    let before = std::fs::read_to_string(path)
        .map_err(|e| ApiError::new(title, format!("Piggy could not read {path}. {e}"), false))?;

    let d = diff::unified(&before, after);
    let before_bytes = before.len() as i64;
    let after_bytes = after.len() as i64;
    Ok(AdviceDiffDto {
        id,
        display_path: crate::commands::tildify(std::path::Path::new(path)),
        hunks: d
            .hunks
            .into_iter()
            .map(|h| DiffHunkDto {
                header: h.header,
                lines: h
                    .lines
                    .into_iter()
                    .map(|l| DiffLineDto {
                        op: l.op.as_str().to_string(),
                        text: l.text,
                        old_no: l.old_no,
                        new_no: l.new_no,
                    })
                    .collect(),
            })
            .collect(),
        added: d.added,
        removed: d.removed,
        before_bytes,
        after_bytes,
        before_est_tokens: claudemd::est_tokens(before_bytes),
        after_est_tokens: claudemd::est_tokens(after_bytes),
        truncated: d.truncated,
    })
}

/// Apply a bundle, reporting per item.
pub fn advice_apply(ids: Vec<String>) -> Result<AdviceApplyDto, ApiError> {
    let _guard = state_write();
    (|| -> anyhow::Result<AdviceApplyDto> {
        let home = config::piggy_home();
        let mut store = Store::open(&home)?;
        let catalog = Catalog::embedded();
        let pricing = Pricing::load(&home);
        // One generate for the whole bundle, and matching by id afterwards. The
        // candidate id is a hash over the kind, the target, the fingerprint and
        // every evidence row, so "the id is still here" is a proof that the plan
        // and every number behind it are unchanged. Regenerating between items
        // would buy nothing on top of that: each kind's apply re-resolves its own
        // target and refuses if it moved.
        let candidates = advice_regenerate(&mut store, &catalog, &pricing)?;

        let mut applied: Vec<String> = Vec::new();
        let mut failures: Vec<AdviceFailureDto> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut kinds: Vec<ActionKind> = Vec::new();
        for id in &ids {
            let Some(candidate) = candidates.iter().find(|c| &c.id == id) else {
                failures.push(AdviceFailureDto {
                    id: id.clone(),
                    reason: ADVICE_GONE.to_string(),
                });
                continue;
            };
            // Load state per item, never once around the loop. A server-disable
            // drives `sweep::apply`, which writes the whole document back, and a
            // saver-mix has `engine::set_enabled` save its own copy: an instance
            // carried across a mixed bundle would write back over whatever the
            // item before it had just saved. Re-reading also means each item sees
            // the snapshot and scope-move records the previous one left behind,
            // which is the behaviour Undo depends on rather than a cost.
            let mut state = match PiggyState::load() {
                Ok(state) => state,
                Err(e) => {
                    failures.push(AdviceFailureDto {
                        id: id.clone(),
                        reason: one_sentence(&e),
                    });
                    continue;
                }
            };
            match advice::apply(&mut store, &mut state, &catalog, candidate) {
                Ok(done) => {
                    applied.push(done.id);
                    warnings.extend(done.warnings);
                    kinds.push(done.kind);
                }
                // The engine's refusal names the candidate and says what moved.
                // Straight through: paraphrasing it here would lose the one
                // detail that tells the reader what to do next.
                Err(e) => failures.push(AdviceFailureDto {
                    id: id.clone(),
                    reason: one_sentence(&e),
                }),
            }
        }

        // Both of these change what attribution reads.
        if kinds
            .iter()
            .any(|k| matches!(k, ActionKind::SaverMix | ActionKind::ServerDisable))
        {
            bump_attr_version();
        }

        // A second generate, so the sheet needs no follow-up read.
        let after = advice_regenerate(&mut store, &catalog, &pricing)?;
        Ok(AdviceApplyDto {
            report: advice_report_dto(&store, &after)?,
            applied,
            failures,
            warnings,
        })
    })()
    .map_err(generic("Couldn't apply that"))
}

/// Reverse one applied row.
pub fn advice_undo(id: String) -> Result<AdviceUndoDto, ApiError> {
    let _guard = state_write();
    let title = "Couldn't put that back";
    let home = config::piggy_home();
    let catalog = Catalog::embedded();
    let pricing = Pricing::load(&home);
    let mut store = Store::open(&home).map_err(generic(title))?;
    let mut state = PiggyState::load().map_err(generic(title))?;

    // Undo can refuse: a later Piggy edit to the same file is still applied, and
    // putting this one back would write over it. That refusal names the
    // suggestion to undo first, which is the whole answer, so it reaches the
    // banner intact rather than through `generic`, which cuts at the first colon.
    let done = advice::undo(&mut store, &mut state, &catalog, &id)
        .map_err(|e| ApiError::new(title, one_sentence(&e), false))?;

    if matches!(done.kind, ActionKind::SaverMix | ActionKind::ServerDisable) {
        bump_attr_version();
    }
    let after = advice_regenerate(&mut store, &catalog, &pricing).map_err(generic(title))?;
    Ok(AdviceUndoDto {
        report: advice_report_dto(&store, &after).map_err(generic(title))?,
        restored: done.restored,
        // Per item, never collapsed into a count: an undo that put three of four
        // files back has to say which one it did not.
        failures: done
            .failures
            .into_iter()
            .map(|f| AdviceFailureDto {
                id: f.item,
                reason: f.reason,
            })
            .collect(),
        message: done.message,
    })
}

/// "Not for me": suppress this target until its evidence roughly doubles.
pub fn advice_dismiss(id: String) -> Result<AdviceReportDto, ApiError> {
    let title = "Couldn't set that aside";
    let home = config::piggy_home();
    let catalog = Catalog::embedded();
    let pricing = Pricing::load(&home);
    let mut store = Store::open(&home).map_err(generic(title))?;

    // `None` for the note: there is no UI for typing a reason, and a note Piggy
    // invented would become the baseline the reopen rule measures against.
    // Dismiss also refuses an applied row, since `dismissed` carries no
    // restore_ref and moving one there would destroy the only handle Undo has;
    // the sheet does not offer it, and this is the second door.
    let existed = advice::dismiss(&mut store, &id, None)
        .map_err(|e| ApiError::new(title, one_sentence(&e), false))?;
    if !existed {
        return Err(ApiError::new(
            title,
            "Piggy no longer has that suggestion. Reopen this panel and try again.",
            false,
        ));
    }
    let after = advice_regenerate(&mut store, &catalog, &pricing).map_err(generic(title))?;
    advice_report_dto(&store, &after).map_err(generic(title))
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeServerDto {
    pub key: String,
    /// The `mcp_manifests.scope` value: the project path, or the user-scope
    /// marker. Identity, not display - it is what `probe_measure` matches on.
    pub scope: String,
    /// "Every project" at user scope, else the `~`-abbreviated project path.
    pub scope_label: String,
    /// "stdio" | "remote" (`Transport::label`).
    pub transport: String,
    /// "measured" | "stale" | "failed" | "never" | "deferred"
    /// (`MeasurementStatus::tag`).
    pub measurement: String,
    /// Present only on a `measured` row. A stale row's stored numbers describe a
    /// command that is not what runs today, so they are not sent at all: there
    /// is no label under which printing them would be true.
    pub tool_count: Option<i64>,
    pub schema_bytes: Option<i64>,
    pub schema_tokens: Option<i64>,
    pub tokenizer: Option<String>,
    /// True when `schema_tokens` came from `probe::TOKENIZER_BYTES_ESTIMATE`.
    /// The schema BYTES are measured either way; the token count is only as good
    /// as the tokenizer that produced it, and today that is a division by 3.5.
    pub tokens_estimated: bool,
    /// Date only. A row has no room for a timestamp.
    pub measured_at: Option<String>,
    /// Already redacted by `probe.rs`; never re-wrap it.
    pub error: Option<String>,
    /// False for http/sse. No button on those.
    pub probeable: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReportDto {
    pub servers: Vec<ProbeServerDto>,
    pub measured: usize,
    pub deferred: usize,
}

fn probe_dto(servers: &[probe::ConfiguredServer], manifests: &[McpManifest]) -> ProbeReportDto {
    let mut out = Vec::with_capacity(servers.len());
    let mut measured_count = 0usize;
    let mut deferred_count = 0usize;
    for server in servers {
        let status = probe::status(manifests, server);
        match status {
            probe::MeasurementStatus::Measured(_) => measured_count += 1,
            probe::MeasurementStatus::Deferred => deferred_count += 1,
            _ => {}
        }
        // Figures come from the status, not from the row's own `ok` flag. A row
        // can be `ok` and still describe a configuration that no longer exists:
        // a changed command, args or env makes it Stale, and quoting a previous
        // configuration's tool count under this configuration is how a surface
        // tells a lie without anyone writing one.
        let measured = match &status {
            probe::MeasurementStatus::Measured(row) => Some(row),
            _ => None,
        };
        let row = status.manifest();
        out.push(ProbeServerDto {
            key: server.key.clone(),
            scope: server.scope().to_string(),
            scope_label: match &server.project {
                None => "Every project".to_string(),
                Some(p) => crate::commands::tildify(std::path::Path::new(p)),
            },
            transport: server.transport.label().to_string(),
            measurement: status.tag().to_string(),
            tool_count: measured.map(|m| m.tool_count),
            schema_bytes: measured.map(|m| m.schema_bytes),
            schema_tokens: measured.map(|m| m.schema_tokens),
            tokenizer: measured.map(|m| m.tokenizer.clone()),
            tokens_estimated: measured.is_some_and(|m| m.tokenizer == probe::TOKENIZER_BYTES_ESTIMATE),
            measured_at: row.map(|m| day(&m.measured_at).to_string()),
            error: row.and_then(|m| m.error.clone()),
            probeable: server.transport == probe::Transport::Stdio,
        });
    }
    ProbeReportDto {
        servers: out,
        measured: measured_count,
        deferred: deferred_count,
    }
}

/// The listing. Reads the configs and the stored measurements; launches nothing.
pub fn probe_report() -> Result<ProbeReportDto, ApiError> {
    (|| -> anyhow::Result<ProbeReportDto> {
        let store = Store::open(&config::piggy_home())?;
        let servers = probe::configured_servers()?;
        let manifests = store.mcp_manifests()?;
        Ok(probe_dto(&servers, &manifests))
    })()
    .map_err(generic("Couldn't read your MCP servers"))
}

/// Start one configured server, read its tool list, stop it.
///
/// Keyed on both the server name and its scope, because `mcp_manifests` is, and
/// the same server can exist at user scope and under a project with different
/// arguments. One server per call and no "measure all" in the app: the timeout
/// is ten seconds per server, and a dozen of them is minutes on one blocking
/// thread with no progress to show for it. `piggy probe --all --yes` is the bulk
/// path.
pub fn probe_measure(server_key: String, scope: String) -> Result<ProbeReportDto, ApiError> {
    (|| -> anyhow::Result<ProbeReportDto> {
        let mut store = Store::open(&config::piggy_home())?;
        let servers = probe::configured_servers()?;
        let target = servers
            .iter()
            .find(|s| s.key == server_key && s.scope() == scope)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Piggy no longer sees a server called '{server_key}' there. \
                     Your Claude config changed since this list was read."
                )
            })?;
        probe::probe(&mut store, &target, &probe::ProbeOptions::default())?;
        let manifests = store.mcp_manifests()?;
        Ok(probe_dto(&servers, &manifests))
    })()
    .map_err(generic("Couldn't measure that server"))
}

// ---------------------------------------------------------------------------
// discover
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub claimed_savings: Option<String>,
    pub license: String,
    pub license_note: Option<String>,
    pub exclusion_reason: Option<String>,
    /// Plain-language "why it's not available yet" when there is no exclusion.
    pub note: String,
    pub repo_url: Option<String>,
    pub risk: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverFeedItem {
    pub name: String,
    pub description: String,
    pub stars: Option<u64>,
    pub author_claims: Option<String>,
    pub repo_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverDto {
    /// Live GitHub-discovery results (from `discovery::discover`, cached ≤1/day).
    /// Empty when offline/rate-limited with no cache yet — never an error.
    pub feed: Vec<DiscoverFeedItem>,
    pub listed_only: Vec<DiscoverEntry>,
}

fn plain_status(e: &Entry) -> String {
    if e.status.contains("v1_1") {
        "Coming in a later Piggy update — it needs a per-project or license step we haven't built yet.".to_string()
    } else if e.status.contains("deferred") || e.status.contains("v2") {
        "Planned for a future version of Piggy.".to_string()
    } else if e.status == "listed_only" {
        "Listed for transparency — not installable.".to_string()
    } else {
        "Not available to turn on yet.".to_string()
    }
}

/// The catalog-derived "listed for transparency" rows: everything not curated +
/// installable, with the plain-language reason it isn't available. Richer than the
/// discovery module's own `listed_only` (license notes, exclusion reasons), so we
/// build these from the catalog directly.
fn listed_only_entries(catalog: &Catalog) -> Vec<DiscoverEntry> {
    catalog
        .ordered()
        .into_iter()
        .filter(|e| {
            !(e.status.starts_with("curated") && e.installable().is_ok() && e.has_install_steps())
        })
        .map(|e| DiscoverEntry {
            id: e.id.clone(),
            name: e.name.clone(),
            description: e.description.clone(),
            claimed_savings: e.claimed_savings.clone(),
            license: e.license.clone(),
            license_note: e.license_note.clone(),
            exclusion_reason: e.exclusion_reason.clone(),
            note: plain_status(e),
            repo_url: e
                .source
                .repo
                .as_ref()
                .map(|r| format!("https://github.com/{r}")),
            risk: e.risk.clone(),
        })
        .collect()
}

/// The live discovery feed (GitHub search), mapped to the UI item. Best-effort:
/// We carry no `authorClaims` for wild repos: a GitHub result has no vetted
/// savings claim, and Piggy never invents one.
fn feed_items(cache: discovery::DiscoveryCache) -> Vec<DiscoverFeedItem> {
    cache
        .repos
        .into_iter()
        .filter(|r| !r.listed_only)
        .map(|r| DiscoverFeedItem {
            name: r.full_name,
            description: r.description.unwrap_or_default(),
            stars: Some(r.stars),
            author_claims: None,
            repo_url: if r.url.is_empty() { None } else { Some(r.url) },
        })
        .collect()
}

/// The Discover section's feed, cache only. Savers mounts this on render, and
/// a primary tab must not phone GitHub unprompted; the live search runs behind
/// the explicit refresh alone.
pub fn discovered_list() -> DiscoverDto {
    let catalog = Catalog::embedded();
    DiscoverDto {
        feed: feed_items(discovery::discover_cached()),
        listed_only: listed_only_entries(&catalog),
    }
}

/// Manual "check now" refresh: the one path that performs a live GitHub search
/// past the daily cache. `discovery::discover` degrades to a stale cache on
/// rate-limit/offline and never errors, so an `Err` here just means an empty
/// feed while the catalog's listed-only rows keep the section useful.
pub fn refresh_discovered() -> DiscoverDto {
    let catalog = Catalog::embedded();
    DiscoverDto {
        feed: match discovery::discover(true) {
            Ok(cache) => feed_items(cache),
            Err(_) => Vec::new(),
        },
        listed_only: listed_only_entries(&catalog),
    }
}

// ---------------------------------------------------------------------------
// share card
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareCardData {
    pub period: String,
    pub week_label: String,
    /// Measured tokens banked vs holdout, or `null` until measured.
    pub tokens_saved: Option<u64>,
    /// Measured "lasts N× longer" multiplier, or `null`.
    pub multiplier: Option<f64>,
    /// `"measured" | "estimated" | "not_enough_data"`.
    pub headline_label: String,
    pub n_holdout: u64,
    /// True only when the numbers are measured — the Share button is gated on it.
    pub shareable: bool,
}

pub fn share_card_data(period_s: String) -> Result<ShareCardData, ApiError> {
    let ov = stats_overview(period_s.clone())?;
    let period = period_from(&period_s);
    // Shareable once there is a holdout-measured OR history-estimated headline;
    // never while still "measuring" (nothing to prove yet).
    let shareable = ov.headline.label == "measured" || ov.headline.label == "estimated";
    // "Tokens banked" is derived from the headline multiplier applied to the
    // period's plan-metered spend (input + output + cache-write; cache reads are
    // excluded from spend weighting, per measurement.md). If your plan lasts M×
    // longer you ran at 1/M the rate, so the counterfactual is M× your actual and
    // the banked amount is actual × (M − 1). This is an estimate even when the
    // headline is "measured" (holdout-backed) — the card's proof line says so.
    let tokens_saved = match ov.headline.value {
        Some(m) if shareable && m > 1.0 => {
            let plan_metered = ov.streams.input + ov.streams.output + ov.streams.cache_write;
            let banked = (plan_metered as f64 * (m - 1.0)).round();
            if banked >= 1.0 {
                Some(banked as u64)
            } else {
                None
            }
        }
        _ => None,
    };
    Ok(ShareCardData {
        period: ov.period,
        week_label: date_range_label(period),
        tokens_saved,
        multiplier: ov.headline.value,
        headline_label: ov.headline.label,
        n_holdout: ov.headline.n_holdout,
        shareable,
    })
}

/// Decode base64 (RFC 4648, standard alphabet), ignoring padding/whitespace.
fn b64_decode(s: &str) -> Result<Vec<u8>, ApiError> {
    fn v(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let val = v(c).ok_or_else(|| {
            ApiError::new(
                "Couldn't save the image",
                "The image data was malformed.",
                false,
            )
        })?;
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

fn desktop_path(file: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Desktop").join(file)
}

/// Decode the PNG and write it to `~/Desktop/piggy-savings.png`, returning the
/// path (the caller reveals it in Finder via the opener plugin).
pub fn save_share_card(png_base64: String) -> Result<PathBuf, ApiError> {
    let bytes = b64_decode(&png_base64)?;
    let path = desktop_path("piggy-savings.png");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, &bytes).map_err(|e| {
        ApiError::new(
            "Couldn't save the image",
            first_sentence(&e.to_string()),
            false,
        )
    })?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// settings (app preferences)
// ---------------------------------------------------------------------------

/// The settings slice the GUI edits. These now live in the `piggy-core`
/// [`PiggyState`] `settings` ledger (the same knobs rotation and attribution
/// read), not a separate file. `rotation_enabled` maps to the core
/// `holdout_enabled` — the master switch for Piggy's A/B rotation: off means no
/// holdout sessions are scheduled (badges fall back to `estimated`), and the
/// background loop skips its rotation step entirely. Launch-at-login is owned by
/// the autostart plugin and merged in at the command layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppPrefs {
    pub holdout_fraction: f64,
    pub rotation_enabled: bool,
}

impl Default for AppPrefs {
    fn default() -> Self {
        AppPrefs {
            holdout_fraction: 0.10,
            rotation_enabled: true,
        }
    }
}

// ---------------------------------------------------------------------------
// The `piggy` command-line tool
// ---------------------------------------------------------------------------

/// Where the bundled `piggy` CLI lives: next to this executable.
///
/// Tauri copies the `binaries/piggy-<triple>` sidecar into
/// `Piggy.app/Contents/MacOS/piggy`, alongside `piggy-app` itself, and does the
/// same next to the dev binary under `target/`. Resolving from
/// [`std::env::current_exe`] therefore works in both, and keeps following the
/// app if the user moves it.
pub fn cli_sidecar_path() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the Piggy executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Piggy executable has no parent directory"))?;
    Ok(dir.join("piggy"))
}

/// Whether the user has the `piggy` CLI linked onto their `PATH`.
///
/// The link's presence *is* the setting: there is no separate stored flag to
/// drift out of sync with the filesystem.
pub fn cli_tool_enabled() -> bool {
    cli_link::exists()
}

/// Turn the `piggy` command-line tool on or off.
///
/// On: symlink `<piggy_home>/bin/piggy` at the bundled sidecar and put that
/// directory on `PATH`. Off: remove the link, and the managed `PATH` line too
/// unless a saver still needs it.
pub fn set_cli_tool(enabled: bool) -> Result<(), ApiError> {
    (|| -> anyhow::Result<()> {
        if enabled {
            let sidecar = cli_sidecar_path()?;
            cli_link::install(&sidecar)?;
        } else {
            cli_link::uninstall()?;
        }
        Ok(())
    })()
    .map_err(generic(if enabled {
        "Couldn't install the piggy command"
    } else {
        "Couldn't remove the piggy command"
    }))
}

/// Re-point an already-installed CLI link at this build's sidecar.
///
/// Called on every launch so the link self-heals after the user moves or
/// replaces Piggy.app. Deliberately does nothing when the user has not opted in,
/// so launching Piggy never touches their shell profile uninvited.
pub fn refresh_cli_link() {
    if !cli_link::exists() {
        return;
    }
    let Ok(sidecar) = cli_sidecar_path() else {
        return;
    };
    // Best-effort: a broken link is surfaced by the Settings toggle and doctor,
    // and must never block startup.
    let _ = cli_link::install(&sidecar);
}

/// The pre-M3 preferences file. If present, its values are folded into the core
/// state once and the file is removed (silent one-shot migration).
fn legacy_prefs_path() -> PathBuf {
    config::piggy_home().join("app-settings.json")
}

/// Fold a legacy `app-settings.json` into the core state's `settings`, then delete
/// it. No-op when the file is absent. Best-effort: a parse/read failure just drops
/// the stale file so it can't shadow the real state.
fn migrate_legacy_prefs() {
    let path = legacy_prefs_path();
    if !path.exists() {
        return;
    }
    if let Some(old) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<AppPrefs>(&b).ok())
    {
        if let Ok(mut state) = PiggyState::load() {
            state.settings.holdout_fraction = old.holdout_fraction.clamp(0.0, 0.5);
            state.settings.holdout_enabled = old.rotation_enabled;
            let _ = state.save();
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// Read the GUI-editable settings straight out of the core state ledger.
pub fn load_prefs() -> AppPrefs {
    migrate_legacy_prefs();
    let state = PiggyState::load().unwrap_or_default();
    AppPrefs {
        holdout_fraction: state.settings.holdout_fraction,
        rotation_enabled: state.settings.holdout_enabled,
    }
}

/// Persist the GUI settings into the core state ledger (clamping the holdout
/// fraction) and anchor the pre-install baseline so rotation/attribution have a
/// cutoff to reason from.
pub fn save_prefs(prefs: &AppPrefs) -> Result<(), ApiError> {
    let _guard = state_write();
    migrate_legacy_prefs();
    (|| -> anyhow::Result<()> {
        let mut state = PiggyState::load()?;
        state.settings.holdout_fraction = prefs.holdout_fraction.clamp(0.0, 0.5);
        state.settings.holdout_enabled = prefs.rotation_enabled;
        state.ensure_created_at();
        state.save()?;
        Ok(())
    })()
    .map_err(generic("Couldn't save your settings"))
}

// ---------------------------------------------------------------------------
// restore defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreDto {
    pub byte_restored: bool,
    pub savers_removed: usize,
    pub swept_restored: usize,
    pub files_removed: usize,
    pub messages: Vec<String>,
}

pub fn restore_defaults() -> Result<RestoreDto, ApiError> {
    let _guard = state_write();
    engine::restore_defaults()
        .map(|r| RestoreDto {
            byte_restored: r.byte_restored,
            savers_removed: r.savers_removed,
            swept_restored: r.swept_restored,
            files_removed: r.files_removed,
            messages: r.messages,
        })
        .map_err(generic("Couldn't restore your settings"))
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorDto {
    pub ok: bool,
    pub checks: Vec<DoctorCheck>,
}

pub fn doctor() -> DoctorDto {
    let mut checks = Vec::new();
    let home = config::piggy_home();
    let projects = config::claude_projects_dir();

    let projects_ok = std::fs::read_dir(&projects).is_ok();
    checks.push(DoctorCheck {
        label: "Claude Code history".to_string(),
        ok: projects_ok,
        detail: if projects_ok {
            "Piggy can read your sessions.".to_string()
        } else {
            "Couldn't find Claude Code's history folder.".to_string()
        },
    });

    // Codex is optional: found = we're reading it; missing = informational,
    // never a failure (Piggy works fine on a Claude-only machine).
    let codex_dirs = config::codex_sessions_dirs();
    checks.push(DoctorCheck {
        label: "Codex history".to_string(),
        ok: true,
        detail: if !codex_dirs.is_empty() {
            "Piggy can read your Codex sessions too.".to_string()
        } else if config::codex_dir().exists() {
            "Codex is installed but has no session history yet.".to_string()
        } else {
            "Codex isn't installed - nothing to measure there.".to_string()
        },
    });

    let settings = config::claude_settings_path();
    if settings.exists() {
        let parses = std::fs::read_to_string(&settings)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .is_some();
        checks.push(DoctorCheck {
            label: "Claude's settings".to_string(),
            ok: parses,
            detail: if parses {
                "Backed up and readable.".to_string()
            } else {
                "Present but Piggy couldn't read it.".to_string()
            },
        });
    } else {
        checks.push(DoctorCheck {
            label: "Claude's settings".to_string(),
            ok: true,
            detail: "No settings file yet — nothing to back up.".to_string(),
        });
    }

    match Store::open(&home).and_then(|s| s.write_test().map(|_| s)) {
        Ok(store) => {
            checks.push(DoctorCheck {
                label: "Piggy's database".to_string(),
                ok: true,
                detail: "Writable and healthy.".to_string(),
            });
            let pricing = Pricing::load(&home);
            match store.pricing_coverage() {
                Ok((matched, total)) if total > 0 => {
                    let pct = 100.0 * matched as f64 / total as f64;
                    checks.push(DoctorCheck {
                        label: "Cost estimates".to_string(),
                        ok: pct >= 99.0,
                        detail: format!(
                            "{pct:.0}% of tokens matched a known price ({} models).",
                            pricing.model_count()
                        ),
                    });
                }
                _ => checks.push(DoctorCheck {
                    label: "Cost estimates".to_string(),
                    ok: true,
                    detail: format!("Pricing table loaded ({} models).", pricing.model_count()),
                }),
            }
        }
        Err(e) => checks.push(DoctorCheck {
            label: "Piggy's database".to_string(),
            ok: false,
            detail: first_sentence(&e.to_string()),
        }),
    }

    DoctorDto {
        ok: checks.iter().all(|c| c.ok),
        checks,
    }
}

// ---------------------------------------------------------------------------
// reindex
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReindexDto {
    pub ran: bool,
    pub sessions: u64,
    pub updated: u64,
    pub scanned: u64,
}

pub fn reindex() -> Result<ReindexDto, ApiError> {
    (|| -> anyhow::Result<ReindexDto> {
        let home = config::piggy_home();
        // Every session-log root on this machine: Claude Code projects plus
        // Codex sessions/archived_sessions, whichever exist.
        let roots = piggy_core::default_roots();
        if roots.is_empty() {
            return Ok(ReindexDto {
                ran: false,
                sessions: 0,
                updated: 0,
                scanned: 0,
            });
        }
        let pricing = Pricing::load(&home);
        let mut store = Store::open(&home)?;
        let rep = piggy_core::run_index_roots(&mut store, &pricing, &roots, false)?;
        // New/changed sessions invalidate the attribution cache.
        if rep.updated > 0 {
            bump_attr_version();
        }
        Ok(ReindexDto {
            ran: true,
            sessions: rep.sessions,
            updated: rep.updated,
            scanned: rep.scanned,
        })
    })()
    .map_err(generic("Couldn't read your latest sessions"))
}

// ---------------------------------------------------------------------------
// background: baseline anchor + rotation (driven by the watcher loop in lib.rs)
// ---------------------------------------------------------------------------

/// Anchor the measurement baseline once at startup: stamp Piggy's install time (so
/// every session already on disk becomes the observational pre-install baseline)
/// and backfill the `pre_install` tags. Best-effort — a failure here just means
/// attribution has less to compare against, never a crash.
pub fn anchor_baseline() {
    let _guard = state_write();
    let Ok(mut state) = PiggyState::load() else {
        return;
    };
    if state.ensure_created_at() {
        let _ = state.save();
    }
    if let Ok(mut store) = Store::open(&config::piggy_home()) {
        let catalog = Catalog::embedded();
        let _ = tagging::tag_pre_install_baseline(&mut store, &state, &catalog);
        // Baseline tags change the OFF groups the attribution reads.
        bump_attr_version();
    }
}

/// Run one rotation scheduler step, gated on the rotation/holdout master switch.
///
/// Returns `true` only when an assignment was actually **applied** (the projects
/// dir was idle) — the watcher loop uses that to emit a stats refresh and to
/// avoid re-ticking until the next session runs. When rotation is disabled, or a
/// session is live, or nothing is installed, this is a no-op returning `false`.
/// `rotation::tick_now` self-gates on the 10-minute idle window, so calling it is
/// always safe; it never perturbs a running session.
pub fn rotation_tick_if_enabled() -> bool {
    // `rotation::tick_now` runs its own load/modify/save, so the tick queues
    // behind whatever the user is doing rather than landing inside it.
    let _guard = state_write();
    let Ok(state) = PiggyState::load() else {
        return false;
    };
    if !state.settings.holdout_enabled {
        return false;
    }
    let catalog = Catalog::embedded();
    let projects = config::claude_projects_dir();
    let Ok(mut store) = Store::open(&config::piggy_home()) else {
        return false;
    };
    let applied = matches!(
        rotation::tick_now(&catalog, &mut store, &projects),
        Ok(RotationOutcome::Applied { .. })
    );
    if applied {
        // A new holdout/full-on assignment retags a session: invalidate the cache.
        bump_attr_version();
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::measuring_note;
    use piggy_core::store::source;

    // -----------------------------------------------------------------------
    // The advice and probe boundary
    //
    // Everything here guards the one defect this milestone treats as a product
    // failure rather than a bug: a figure shown under a label that is not the
    // one the engine computed it with.
    // -----------------------------------------------------------------------

    mod wire {
        use super::super::*;
        use piggy_core::advice::{basis, EvidenceRow, Params, RISK_CONTENT_EDIT, RISK_TOGGLE};

        fn candidate(kind: ActionKind, evidence: Vec<EvidenceRow>) -> Candidate {
            Candidate {
                id: "abc123".into(),
                kind,
                target: "~/.claude/CLAUDE.md".into(),
                title: "Trim this file".into(),
                evidence,
                est_tokens_month: 140_000,
                risk_tier: if kind.edits_content() {
                    RISK_CONTENT_EDIT
                } else {
                    RISK_TOGGLE
                },
                prerequisites: Vec::new(),
                fingerprint: "deadbeef".into(),
                params: Params::Claudemd {
                    path: "/tmp/CLAUDE.md".into(),
                },
                new_content: None,
                status: advice_status::OPEN.into(),
            }
        }

        /// THE boundary guard. The engine hands every figure across with the
        /// label that says how it was arrived at; if this wrapper ever
        /// "helpfully" shortens `estimated (observational)` to something
        /// confident, an A/B on a non-randomized baseline starts reading as a
        /// measurement and the product's whole claim is gone.
        #[test]
        fn an_evidence_basis_reaches_the_wire_verbatim() {
            let all = [
                basis::OBSERVED,
                basis::ESTIMATED,
                basis::MEASURED_MANIFEST,
                basis::MEASURED,
                basis::ESTIMATED_AB,
                basis::MEASURING,
            ];
            let rows: Vec<EvidenceRow> = all
                .iter()
                .map(|b| EvidenceRow {
                    label: "Tokens a month it saves".into(),
                    value: "~12,345".into(),
                    basis: (*b).to_string(),
                })
                .collect();
            let dto = advice_item(&candidate(ActionKind::ServerDisable, rows));
            let out: Vec<&str> = dto.evidence.iter().map(|e| e.basis.as_str()).collect();
            assert_eq!(out, all);
            // And the value with it: an app that re-derives a figure is how the
            // number and its label drift apart.
            assert!(dto.evidence.iter().all(|e| e.value == "~12,345"));
        }

        /// A trim's figure is what the file COSTS. A card that prints it as a
        /// saving claims a rewrite gives all of it back, which nobody has
        /// measured and which is by far the biggest number on the list.
        #[test]
        fn a_trims_figure_is_marked_burden_and_every_other_kind_saves() {
            let burden = advice_item(&candidate(ActionKind::ClaudemdTrim, vec![]));
            assert_eq!(burden.figure_kind, "burden");
            for kind in [
                ActionKind::ServerDisable,
                ActionKind::ServerScope,
                ActionKind::ClaudemdFix,
                ActionKind::SaverMix,
            ] {
                assert_eq!(
                    advice_item(&candidate(kind, vec![])).figure_kind,
                    "saves",
                    "{kind:?}"
                );
            }
        }

        /// A stale row is a plan computed against something that has since
        /// moved. It explains itself and it is never applyable.
        #[test]
        fn a_stale_candidate_explains_itself_and_cannot_be_applied() {
            let mut c = candidate(ActionKind::ServerDisable, vec![]);
            c.status = advice_status::STALE.into();
            let dto = advice_item(&c);
            assert!(!dto.applyable);
            assert!(dto.blocked_reason.is_some());
            assert!(!dto.blocked_reason.unwrap().contains(':'));
        }

        /// The failure banner reads `<name> · <reason>`, so the reason cannot
        /// carry a colon-truncated fragment. `first_sentence` cuts at the first
        /// colon, and the engine puts the actionable half after one: piped
        /// through `generic`, "nothing to write for /a/b: turn on the local
        /// advisor" reaches the reader as a bare file path.
        #[test]
        fn a_per_item_failure_reason_keeps_its_colon_and_its_whole_sentence() {
            let e = anyhow::anyhow!(
                "nothing to write for /a/b: turn on the local advisor in Settings for a drafted rewrite"
            );
            let reason = one_sentence(&e);
            assert!(reason.starts_with("nothing to write for /a/b:"), "{reason}");
            assert!(reason.ends_with("drafted rewrite"), "{reason}");
            // The old behaviour, for contrast.
            assert_eq!(first_sentence(&e.to_string()), "nothing to write for /a/b");
        }

        #[test]
        fn a_very_long_failure_reason_is_capped_on_a_character_boundary() {
            let e = anyhow::anyhow!("{}", "é".repeat(400));
            let reason = one_sentence(&e);
            assert_eq!(reason.chars().count(), REASON_MAX + 1);
        }
    }

    mod probe_wire {
        use super::super::*;

        fn server(key: &str, project: Option<&str>) -> probe::ConfiguredServer {
            probe::ConfiguredServer {
                key: key.into(),
                project: project.map(str::to_string),
                transport: probe::Transport::Stdio,
                config: serde_json::json!({ "command": "node", "args": ["server.mjs"] }),
            }
        }

        fn manifest(s: &probe::ConfiguredServer, config_hash: &str, tokenizer: &str) -> McpManifest {
            McpManifest {
                server_key: s.key.clone(),
                scope: s.scope().to_string(),
                config_hash: config_hash.to_string(),
                tool_count: 21,
                schema_bytes: 43_190,
                schema_tokens: 12_340,
                tokenizer: tokenizer.to_string(),
                measured_at: "2026-08-01T09:14:02Z".into(),
                ok: true,
                error: None,
            }
        }

        /// The bytes are real and the token count is a division by 3.5. Reading
        /// one label off the other printed every probed row as an exact count it
        /// never was.
        #[test]
        fn a_bytes_over_35_token_count_is_flagged_estimated_though_the_bytes_are_measured() {
            let s = server("github", None);
            let m = manifest(&s, &s.config_hash(), probe::TOKENIZER_BYTES_ESTIMATE);
            let dto = probe_dto(&[s], &[m]);
            let row = &dto.servers[0];
            assert_eq!(row.measurement, "measured");
            assert_eq!(row.schema_bytes, Some(43_190));
            assert!(row.tokens_estimated);
            assert_eq!(dto.measured, 1);
        }

        #[test]
        fn a_real_tokenizer_leaves_the_token_count_unhedged() {
            let s = server("github", None);
            let m = manifest(&s, &s.config_hash(), "qwen3-4b");
            let dto = probe_dto(&[s], &[m]);
            assert!(!dto.servers[0].tokens_estimated);
        }

        /// A stale row's stored numbers describe a command that is not what runs
        /// today. There is no label under which printing them is true, so they
        /// do not cross the wire at all.
        #[test]
        fn a_stale_manifest_sends_no_figures() {
            let s = server("github", None);
            let m = manifest(&s, "a-hash-from-a-different-command", probe::TOKENIZER_BYTES_ESTIMATE);
            let dto = probe_dto(&[s], &[m]);
            let row = &dto.servers[0];
            assert_eq!(row.measurement, "stale");
            assert_eq!(row.tool_count, None);
            assert_eq!(row.schema_bytes, None);
            assert_eq!(row.schema_tokens, None);
            assert_eq!(row.tokenizer, None);
            assert!(!row.tokens_estimated);
        }

        #[test]
        fn a_user_scope_server_is_labelled_for_every_project_and_a_remote_has_no_button() {
            let user = server("github", None);
            let mut remote = server("linear", None);
            remote.transport = probe::Transport::Remote;
            let dto = probe_dto(&[user, remote], &[]);
            assert_eq!(dto.servers[0].scope_label, "Every project");
            assert_eq!(dto.servers[0].measurement, "never");
            assert!(dto.servers[0].probeable);
            assert_eq!(dto.servers[1].measurement, "deferred");
            assert!(!dto.servers[1].probeable);
            assert_eq!(dto.deferred, 1);
        }
    }

    mod sweep_wire {
        use super::super::*;

        fn item(cost_basis: &str, tokens_estimated: bool) -> sweep::SweepItem {
            sweep::SweepItem {
                idx: 1,
                kind: "mcp".into(),
                id: "github".into(),
                source: None,
                used: 0,
                used_windowed: true,
                est_tokens: 12_340,
                cost_basis: cost_basis.into(),
                tokens_estimated,
                recommend_disable: true,
                scope_to: None,
                reason: "no tool calls in the look-back window".into(),
            }
        }

        /// The report used to hardcode `estimated: true`, which called a probed
        /// manifest a guess. The flag now follows the rows.
        #[test]
        fn a_sweep_report_of_exact_rows_is_not_called_estimated() {
            let exact = dto_from(sweep::SweepReport {
                sessions_considered: 50,
                items: vec![item(sweep::COST_BASIS_MEASURED, false)],
            });
            assert!(!exact.estimated);
            assert!(!exact.items[0].estimated);

            let hedged = dto_from(sweep::SweepReport {
                sessions_considered: 50,
                items: vec![
                    item(sweep::COST_BASIS_MEASURED, false),
                    item(sweep::COST_BASIS_ESTIMATE, true),
                ],
            });
            assert!(hedged.estimated, "one estimated row hedges the total");
        }
    }

    #[test]
    fn boost_is_wired_to_discover_not_home() {
        use super::{curated_installable, listed_only_entries};
        use piggy_core::registry::Catalog;
        let c = Catalog::embedded();

        // Home (the master-switch set) shows curated+installable savers only.
        let home: Vec<&str> = curated_installable(&c).iter().map(|e| e.id.as_str()).collect();
        assert!(home.contains(&"rtk"), "rtk should be on Home");
        assert!(!home.contains(&"boost"), "boost must NOT be a Home toggle");

        // Discover carries boost as listed-only, with its verified exclusion reason.
        let listed = listed_only_entries(&c);
        let boost = listed
            .iter()
            .find(|d| d.id == "boost")
            .expect("boost surfaces in Discover");
        let reason = boost.exclusion_reason.as_deref().unwrap_or("");
        assert!(
            reason.contains("telemetry") && reason.contains("permissionDecision"),
            "boost reason names both blockers"
        );
        assert_eq!(
            boost.repo_url.as_deref(),
            Some("https://github.com/jfrog/boost"),
            "Discover links the real repo"
        );
    }

    #[test]
    fn measuring_note_names_the_blocker() {
        let py = Some(("python3", "hooks no-op without it"));
        let node = Some(("node", "degrades to skill-only without it"));
        // Off saver: nothing to explain.
        assert_eq!(measuring_note(false, None, true, None), None);
        // A missing binary is the root cause and outranks rotation/pin - and the
        // message names the binary and its reason, so node works the same way.
        let m = measuring_note(true, Some(source::MANUAL), false, py).unwrap();
        assert!(m.contains("python3") && m.contains("no-op"));
        let n = measuring_note(true, Some(source::MANUAL), true, node).unwrap();
        assert!(n.contains("node") && n.contains("skill-only"));
        // Rotation off wins over a manual pin.
        assert!(measuring_note(true, Some(source::MANUAL), false, None)
            .unwrap()
            .contains("Rotation is off"));
        // Rotating but hand-pinned: excluded from the A/B.
        assert!(measuring_note(true, Some(source::MANUAL), true, None)
            .unwrap()
            .contains("turned this on by hand"));
        // Rotating, not pinned, binaries present: ordinary warm-up, chip covers it.
        assert_eq!(
            measuring_note(true, Some(source::ROTATION), true, None),
            None
        );
    }

    #[test]
    fn headline_note_names_cost_more_suppression_not_sample_size() {
        use super::map_headline;
        use piggy_core::attribution::{
            Badge, Headline as CoreHeadline, HeadlineBaseline, MultiplierState,
        };

        // Enough sessions on BOTH sides and the on-side IS randomized, but the
        // multiplier was withheld because the savers came out behind an
        // observational baseline. The old note read "10 of 10 sessions ... no number
        // faked", implying we were still gathering.
        let cost_more = CoreHeadline {
            baseline: HeadlineBaseline::PreInstall,
            n_full_on: 10,
            n_baseline: 7515,
            ceiling: Badge::Estimated,
            on_randomized: true,
            n_full_on_randomized: 10,
            baseline_clean: false,
            multiplier: None,
            multiplier_state: MultiplierState::WithheldCostMore,
            streams: vec![],
            turns: piggy_core::attribution::StreamStat {
                stream: piggy_core::attribution::Stream::Turns,
                n_on: 0,
                n_off: 0,
                median_on: 0.0,
                median_off: 0.0,
                delta: None,
                ci: None,
                badge: Badge::Measuring,
            },
            // Both arms are full here, so `waiting()` is None regardless and the
            // pace never gets read. Present so the struct is complete.
            on_pace: None,
            baseline_pace: None,
            // No fold-in here: this fixture is about cost-more suppression, and a
            // carried arm would take the note branch above it.
            n_carried: 0,
            carried_savers: Vec::new(),
        };
        let h = map_headline(&cost_more);
        assert_eq!(h.label, "not_enough_data");
        let note = h.note.expect("a reason");
        assert!(
            note.contains("cost more") && !note.contains(" of 10"),
            "cost-more suppression must be named, never reported as a session count, got {note:?}"
        );

        // A genuinely short full-on side (still randomized) is an honest count.
        let warming = CoreHeadline {
            n_full_on: 4,
            multiplier_state: MultiplierState::NoData,
            ..cost_more.clone()
        };
        assert!(
            map_headline(&warming)
                .note
                .unwrap()
                .contains("4 of 10 sessions on your current saver set"),
            "a short full-on side stays a count, not the suppression note"
        );

        // The root blocker: NOTHING is randomized (savers pinned by hand, so the
        // scheduler never chose a single session). This must beat both the count and
        // the cost-more message - it is the actual cause, and the only one the user
        // can act on - and point to the fix.
        let pinned = CoreHeadline {
            on_randomized: false,
            n_full_on_randomized: 0,
            ..cost_more.clone()
        };
        let note = map_headline(&pinned).note.expect("a reason");
        assert!(
            note.contains("by hand") && note.contains("Savers tab") && !note.contains(" of 10"),
            "a hand-pinned setup must be named as the blocker with the hand-back fix, got {note:?}"
        );

        // Rotation running, ON arm short of the bar: the same `on_randomized: false`
        // flag, and the opposite advice. `n_full_on` is the POOLED count and sits in
        // the thousands, so the sample-size branch below cannot see this arm at all -
        // which is how a screen four sessions from settling came to say "Piggy never
        // switches them off" and point at a Savers tab with nothing pinned in it.
        let rotating = CoreHeadline {
            on_randomized: false,
            n_full_on: 9792,
            n_full_on_randomized: 5,
            ..cost_more.clone()
        };
        let note = map_headline(&rotating).note.expect("a reason");
        assert!(
            note.contains("5 of 10") && !note.contains("Savers tab"),
            "a filling randomized arm must show its own count, not the pin fix, got {note:?}"
        );
    }

    /// A carried-forward ON arm whose older sessions were not *all* scheduler-
    /// chosen comes back with `on_randomized: false` even though the live arm is
    /// rotation's own. The hand-set note used to win that state and tell the user
    /// "the rest you turned on by hand" about sessions Piggy chose itself, while
    /// the carry-forward sentence written for it never rendered.
    #[test]
    fn carried_arm_gets_the_carry_note_not_the_hand_set_one() {
        use super::map_headline;
        use piggy_core::attribution::{
            Badge, Headline as CoreHeadline, HeadlineBaseline, MultiplierState,
        };

        // 5 live sessions the scheduler chose, topped up with 8 from before the
        // saver set changed. Enough on both sides for a number, capped to
        // `estimated` by the fold-in.
        let carried = CoreHeadline {
            baseline: HeadlineBaseline::Holdout,
            n_full_on: 13,
            n_baseline: 12,
            ceiling: Badge::Estimated,
            on_randomized: false,
            n_full_on_randomized: 5,
            baseline_clean: true,
            multiplier: Some(1.4),
            multiplier_state: MultiplierState::Shown,
            streams: vec![],
            turns: piggy_core::attribution::StreamStat {
                stream: piggy_core::attribution::Stream::Turns,
                n_on: 0,
                n_off: 0,
                median_on: 0.0,
                median_off: 0.0,
                delta: None,
                ci: None,
                badge: Badge::Measuring,
            },
            on_pace: None,
            baseline_pace: None,
            n_carried: 8,
            carried_savers: vec!["rtk".to_string()],
        };
        let h = map_headline(&carried);
        assert_eq!(h.label, "estimated");
        let note = h.note.expect("a reason");
        assert!(
            note.contains("counting 8 sessions from before you changed savers")
                && !note.contains("by hand"),
            "a carried arm must name the fold-in, never blame the user, got {note:?}"
        );

        // Same flag, nothing carried: the hand-set count is the right reason again.
        let hand_set = CoreHeadline {
            n_full_on: 9792,
            n_carried: 0,
            carried_savers: Vec::new(),
            ..carried.clone()
        };
        let note = map_headline(&hand_set).note.expect("a reason");
        assert!(
            note.contains("5 of 10") && note.contains("by hand"),
            "an uncarried arm short of the bar keeps its own count, got {note:?}"
        );
    }

    // The pure-fn test above covers the message; this covers the wiring - that
    // `saver_row` reads the pin/holdout/binary state off a real catalog entry and
    // attaches the note to the row the UI renders.
    #[test]
    fn saver_row_attaches_pin_note() {
        use super::{curated_installable, saver_row};
        use piggy_core::state::SaverState;
        use piggy_core::{Catalog, PiggyState};
        use std::collections::HashMap;

        let catalog = Catalog::embedded();
        let entries = curated_installable(&catalog);
        let e = entries.first().copied().expect("a curated saver to pin");

        // Pin it on by hand, rotation enabled: it runs but sits out the A/B.
        let saver: SaverState = serde_json::from_value(serde_json::json!({
            "id": e.id,
            "version": "1.0.0",
            "installed_at": "2026-01-01",
            "enabled": true,
            "last_toggle_source": source::MANUAL,
        }))
        .unwrap();
        let mut state = PiggyState::default();
        state.settings.holdout_enabled = true;
        state.savers.insert(e.id.clone(), saver);

        // Its required binaries present, so the note reaches the pin branch (a
        // missing binary would outrank it).
        let mut bin_present: HashMap<&str, bool> = HashMap::new();
        for (bin, _) in e.required_binaries() {
            bin_present.insert(bin, true);
        }

        // No attribution yet: the badge is "measuring", the state the note explains.
        let row = saver_row(e, &state, None, &bin_present);
        assert_eq!(row.badge.kind, "measuring");
        assert!(
            row.badge
                .note
                .as_deref()
                .unwrap_or("")
                .contains("turned this on by hand"),
            "expected the pin note on the row, got {:?}",
            row.badge.note
        );
    }
}

// ---------------------------------------------------------------------------
// Context ledger
// ---------------------------------------------------------------------------

/// One row of "where did my context tokens come from".
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerSourceDto {
    /// Stable key (`__floor`, `__conversation`, or an attachment type).
    pub kind: String,
    /// Display name.
    pub label: String,
    pub tokens: u64,
    /// Share of all cache-write tokens, 0.0–1.0.
    pub share: f64,
    /// Whether this is an injection the user can configure away, as opposed to
    /// the session floor or the work itself. Drives the "removable" styling.
    pub removable: bool,
    /// Whether this row is part of the session floor — the residual OR a named
    /// component of it. The hero bar sums these; reading only the residual
    /// understated the floor by every component it had been decomposed into.
    pub is_floor: bool,
    /// Whether the figure is bounded-by-content rather than a measured write.
    pub estimated: bool,
}

/// One project's split between opening sessions and doing work.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerProjectDto {
    pub project: String,
    /// Trailing path component, for a readable label.
    pub name: String,
    pub sessions: u64,
    pub msgs_per_session: f64,
    pub floor_tokens: u64,
    pub work_tokens: u64,
    /// Floor share of this project's tokens, 0.0–1.0.
    pub overhead: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerOverview {
    pub period: String,
    pub period_label: String,
    pub total_tokens: u64,
    pub removable_tokens: u64,
    /// Share of cache writes that bought session startup rather than work.
    pub overhead: f64,
    /// How much further the same plan goes with the configurable context
    /// removed. **Available headroom, not achieved savings** — the dashboard
    /// must word it as "could", never as a saving already banked.
    pub headroom: Option<f64>,
    /// The removable share behind `headroom`, as a fraction of TOTAL COST
    /// (0.0-1.0). Not the share of cache writes — that number is bigger and
    /// quoting it next to a cost-weighted multiplier makes the two disagree.
    pub removable_share: f64,
    pub sessions: u64,
    pub sources: Vec<LedgerSourceDto>,
    pub projects: Vec<LedgerProjectDto>,
    /// True when nothing is indexed for this window, so the UI shows the empty
    /// state instead of a table of zeroes.
    pub empty: bool,
}

pub fn ledger_overview(period_s: String) -> Result<LedgerOverview, ApiError> {
    (|| -> anyhow::Result<LedgerOverview> {
        let home = config::piggy_home();
        let store = Store::open(&home)?;
        let period = period_from(&period_s);
        let pricing = Pricing::load(&home);
        // `day_cutoff`, NOT `cutoff`: calendar days are what every chart on the
        // Ledger screen draws, and this overview shares its period selector with
        // the task table. Windowed on the rolling instant it would reach up to a
        // day further back and quote a project a different total per tab.
        let l = store.ledger(period.day_cutoff().as_deref(), &pricing)?;
        let total = l.total_tokens();
        let sources = l
            .rows
            .iter()
            .map(|r| LedgerSourceDto {
                kind: r.kind.clone(),
                label: r.label(),
                tokens: r.tokens,
                share: l.share(r),
                removable: r.removable(),
                is_floor: r.is_floor(),
                estimated: r.estimated(),
            })
            .collect();
        let projects = l
            .projects
            .iter()
            .map(|p| LedgerProjectDto {
                project: p.project.clone(),
                name: p
                    .project
                    .rsplit('/')
                    .find(|s| !s.is_empty())
                    .unwrap_or(&p.project)
                    .to_string(),
                sessions: p.sessions,
                msgs_per_session: p.msgs_per_session(),
                floor_tokens: p.floor_tokens,
                work_tokens: p.work_tokens,
                overhead: p.overhead(),
            })
            .collect();
        Ok(LedgerOverview {
            period: period_key(period).to_string(),
            period_label: period.label().to_string(),
            total_tokens: total,
            removable_tokens: l.removable_tokens(),
            overhead: l.overhead(),
            headroom: l.headroom(),
            removable_share: l.removable_cost_share(),
            sessions: l.projects.iter().map(|p| p.sessions).sum(),
            sources,
            projects,
            empty: total == 0,
        })
    })()
    // A read cannot have rolled anything back, and the raw chain carries the
    // SQLite message plus the absolute db path into the banner. `generic` is the
    // contract every other read command in this module uses.
    .map_err(generic("Couldn't build the context ledger"))
}

/// One row of the task table: a project's spend, its outcome, and its history.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRowDto {
    pub project: String,
    /// Trailing path component, for a readable label.
    pub name: String,
    pub sessions: u64,
    pub floor_tokens: u64,
    pub work_tokens: u64,
    pub total_tokens: u64,
    /// Share of the window's cache-write tokens, 0.0–1.0.
    pub share: f64,
    /// User prompts recorded. `0` means the logs carry no task boundary, which
    /// the UI must render as missing rather than as zero work.
    pub tasks: u64,
    pub turns: u64,
    /// `null` when no tasks were recorded, so the column shows "no data"
    /// instead of a fabricated average.
    pub turns_per_task: Option<f64>,
    pub tool_errors: u64,
    pub failed_tasks: u64,
    /// Share of tasks that hit at least one tool error, or `null` when none
    /// were recorded.
    pub failure_rate: Option<f64>,
    /// Cache-write tokens per day, oldest first. Drawn as-is: the sparkline is
    /// this series or it is absent, never inferred from anything else.
    ///
    /// It sums to `total_tokens` for every window except all-time, where it
    /// covers the most recent 120 days only. The UI has to label that rather
    /// than let the two figures read as one measurement.
    pub daily: Vec<u64>,
    /// Change against the prior equal-length window, as a fraction. `null` when
    /// there is no prior window or it held nothing.
    pub delta: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskTable {
    pub period: String,
    pub period_label: String,
    pub rows: Vec<TaskRowDto>,
    /// True when the window recorded no task boundaries at all — the logs
    /// predate `promptId`. The UI explains that rather than showing empty
    /// outcome columns.
    pub tasks_unrecorded: bool,
    pub empty: bool,
}

/// The task table: per-project spend joined to the outcome signal.
///
/// Reads the ledger for the floor/work split (so this table and the By cause
/// view cannot disagree) and the task rows for turns, failures and history.
pub fn task_table(period_s: String) -> Result<TaskTable, ApiError> {
    (|| -> anyhow::Result<TaskTable> {
        let home = config::piggy_home();
        let store = Store::open(&home)?;
        let period = period_from(&period_s);
        let pricing = Pricing::load(&home);
        // `day_cutoff`, NOT `cutoff`: the task table windows on calendar days,
        // and the rolling instant reaches up to a day further back. Paired with
        // it the row would show a total the sparkline beside it never draws.
        let ledger = store.ledger(period.day_cutoff().as_deref(), &pricing)?;
        let tasks = store.task_table(period)?;

        // Keyed by project path: both reads now cover the same calendar days,
        // so a row present in one is expected in the other.
        let by_project: std::collections::HashMap<&str, &piggy_core::TaskRow> =
            tasks.iter().map(|t| (t.project.as_str(), t)).collect();
        let total: u64 = ledger.projects.iter().map(|p| p.floor_tokens + p.work_tokens).sum();

        let rows: Vec<TaskRowDto> = ledger
            .projects
            .iter()
            .map(|p| {
                let t = by_project.get(p.project.as_str());
                let row_total = p.floor_tokens + p.work_tokens;
                TaskRowDto {
                    project: p.project.clone(),
                    name: p
                        .project
                        .rsplit('/')
                        .find(|s| !s.is_empty())
                        .unwrap_or(&p.project)
                        .to_string(),
                    sessions: p.sessions,
                    floor_tokens: p.floor_tokens,
                    work_tokens: p.work_tokens,
                    total_tokens: row_total,
                    share: if total == 0 {
                        0.0
                    } else {
                        row_total as f64 / total as f64
                    },
                    tasks: t.map(|t| t.tasks).unwrap_or(0),
                    turns: t.map(|t| t.turns).unwrap_or(0),
                    turns_per_task: t.and_then(|t| t.turns_per_task()),
                    tool_errors: t.map(|t| t.tool_errors).unwrap_or(0),
                    failed_tasks: t.map(|t| t.failed_tasks).unwrap_or(0),
                    failure_rate: t.and_then(|t| t.failure_rate()),
                    daily: t.map(|t| t.daily.clone()).unwrap_or_default(),
                    delta: t.and_then(|t| t.delta()),
                }
            })
            .collect();

        Ok(TaskTable {
            period: period_key(period).to_string(),
            period_label: period.label().to_string(),
            tasks_unrecorded: !rows.is_empty() && rows.iter().all(|r| r.tasks == 0),
            empty: rows.is_empty(),
            rows,
        })
    })()
    // Read-only, like the overview above: no rollback to report, no raw chain.
    .map_err(generic("Couldn't build the task table"))
}

/// One ledger finding for the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InsightDto {
    pub id: String,
    /// `high` | `notable` | `info`.
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub tokens: u64,
    pub action: String,
}

pub fn ledger_insights(period_s: String) -> Result<Vec<InsightDto>, ApiError> {
    (|| -> anyhow::Result<Vec<InsightDto>> {
        let home = config::piggy_home();
        let store = Store::open(&home)?;
        let period = period_from(&period_s);
        // `day_cutoff`, NOT `cutoff`: these findings are quoted beside the same
        // tables and charts, which are all cut on calendar days. A rolling
        // instant would let a headline count spend no tab on the screen shows.
        Ok(store
            .insights(period.day_cutoff().as_deref(), &Pricing::load(&home))?
            .into_iter()
            .map(|i| InsightDto {
                id: i.id,
                severity: i.severity.as_str().to_string(),
                title: i.title,
                detail: i.detail,
                tokens: i.tokens,
                action: i.action,
            })
            .collect())
    })()
    // Read-only, like the overview above: no rollback to report, no raw chain.
    .map_err(generic("Couldn't derive insights"))
}
