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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use piggy_core::attribution::{
    self, Badge as CoreBadge, Headline as CoreHeadline, HeadlineBaseline, SaverAttribution,
    MIN_GROUP,
};
use piggy_core::registry::Entry;
use piggy_core::rotation::{self, RotationOutcome};
use piggy_core::{
    config, discovery, engine, stats::Period, sweep, tagging, Catalog, PiggyState, Pricing, Store,
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

struct AttrBundle {
    headline: CoreHeadline,
    per_saver: std::collections::HashMap<String, SaverAttribution>,
}

/// Invalidate the attribution cache so the next dashboard read recomputes.
/// Called after anything that changes the session data (indexing, rotation
/// tagging, baseline anchoring).
fn bump_attr_version() {
    ATTR_INDEX_VERSION.fetch_add(1, Ordering::Relaxed);
}

/// The per-saver attribution + headline for the current index version, computed
/// once and cached. Best-effort: an unreadable store propagates as `Err` and the
/// caller degrades to an honest "measuring"/"not_enough_data" rather than crash.
fn attribution_bundle() -> anyhow::Result<Arc<AttrBundle>> {
    let version = ATTR_INDEX_VERSION.load(Ordering::Relaxed);
    if let Ok(guard) = ATTR_CACHE.lock() {
        if let Some((v, bundle)) = guard.as_ref() {
            if *v == version {
                return Ok(bundle.clone());
            }
        }
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

// ---------------------------------------------------------------------------
// Period helpers
// ---------------------------------------------------------------------------

fn period_from(s: &str) -> Period {
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Headline {
    /// The measured "lasts N× longer" multiplier, or `null` until measured.
    pub value: Option<f64>,
    /// `"measured" | "estimated" | "not_enough_data"`.
    pub label: String,
    pub n_holdout: u64,
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
///   with a multiplier. Sub-line: "estimated vs your history · holdout
///   measurement in progress".
/// * **not_enough_data** — a partial holdout (1..MIN_GROUP), no baseline, or no
///   computable multiplier: never a faked number. `n_holdout` still carries the
///   holdout sessions gathered so far so the UI can show "N of 10".
///
/// A live holdout always wins the baseline in `piggy-core`, so `baseline ==
/// Holdout` ⇔ at least one holdout session exists; hence `n_holdout` is the true
/// holdout count when holding out, and 0 for the pre-install / none cases.
fn map_headline(hl: &CoreHeadline) -> Headline {
    let n_holdout = if hl.baseline == HeadlineBaseline::Holdout {
        hl.n_baseline as u64
    } else {
        0
    };
    let has_mult = hl.multiplier.is_some();
    let measured = has_mult
        && hl.baseline == HeadlineBaseline::Holdout
        && hl.n_full_on >= MIN_GROUP
        && hl.n_baseline >= MIN_GROUP;
    let estimated = has_mult && !measured && hl.baseline == HeadlineBaseline::PreInstall;
    let label = if measured {
        "measured"
    } else if estimated {
        "estimated"
    } else {
        "not_enough_data"
    };
    Headline {
        value: if label == "not_enough_data" {
            None
        } else {
            hl.multiplier
        },
        label: label.to_string(),
        n_holdout,
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
    pub installable: bool,
    pub behavior_changing: bool,
    pub warning: Option<String>,
    pub risk: Option<String>,
    pub claimed_savings: Option<String>,
    pub license: String,
    pub license_note: Option<String>,
    pub ordering: i64,
    pub badge: Badge,
    /// True when the saver exposes user-tunable options (a Configure control).
    pub configurable: bool,
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

fn saver_row(e: &Entry, state: &PiggyState, attr: Option<&SaverAttribution>) -> SaverRow {
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
        installable: e.installable().is_ok() && e.has_install_steps(),
        behavior_changing: e.behavior_changing,
        warning: e.warning.clone(),
        risk: e.risk.clone(),
        claimed_savings: e.claimed_savings.clone(),
        license: e.license.clone(),
        license_note: e.license_note.clone(),
        ordering: e.ordering,
        badge: attr.map(saver_badge).unwrap_or(Badge {
            kind: "measuring".to_string(),
            delta: None,
            n: 0,
        }),
        configurable: !e.config_options.is_empty(),
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
            }
        }
        None => Badge {
            kind: "measuring".to_string(),
            delta: None,
            n: 0,
        },
    }
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
    let savers = curated_installable(&catalog)
        .iter()
        .map(|e| {
            let attr = bundle.as_ref().and_then(|b| b.per_saver.get(&e.id));
            saver_row(e, &state, attr)
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

pub fn savers_list() -> Result<SaversState, ApiError> {
    build_savers_state().map_err(generic("Couldn't read your savers"))
}

/// Turn a single saver on or off. `on` when not installed installs it (with the
/// engine's own health-check + rollback); `off` uses the fast A/B disable path.
pub fn saver_toggle(id: String, on: bool) -> Result<SaversState, ApiError> {
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
            // Turning a saver on can auto-disable one it conflicts with; tell the user.
            if on {
                if let Ok(after) = PiggyState::load() {
                    result.notice = conflict_notice(&catalog, &before_enabled, &after);
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

/// The master switch. On installs/enables the curated default-on set in
/// `ordering` order; off disables every Piggy-managed saver (kept installed).
pub fn master_toggle(on: bool) -> Result<SaversState, ApiError> {
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
        if let Ok(after) = PiggyState::load() {
            result.notice = conflict_notice(&catalog, &before_enabled, &after);
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
    SweepReportDto {
        sessions_considered: report.sessions_considered,
        est_recoverable_tokens: est_recoverable,
        estimated: true,
        items: report
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
                estimated: true,
                recommend_disable: i.recommend_disable,
                reason: i.reason,
            })
            .collect(),
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
    (|| -> anyhow::Result<SweepReportDto> {
        let store = Store::open(&config::piggy_home())?;
        let mut state = PiggyState::load()?;
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
            sweep::apply(&store, &mut state, item.idx, sweep::DEFAULT_N_SESSIONS)?;
        }
        Ok(dto_from(sweep::scan(&store, sweep::DEFAULT_N_SESSIONS)?))
    })()
    .map_err(generic("Couldn't switch those off"))
}

/// Restore swept items. NOTE: this worktree's `piggy-core` only exposes
/// `sweep::restore_all` (per-item restore is private), so this restores **every**
/// swept item, then re-scans. `ids` is accepted for a future per-item API.
pub fn sweep_restore(_ids: Vec<String>) -> Result<SweepReportDto, ApiError> {
    (|| -> anyhow::Result<SweepReportDto> {
        let mut state = PiggyState::load()?;
        sweep::restore_all(&mut state)?;
        state.save()?;
        let store = Store::open(&config::piggy_home())?;
        Ok(dto_from(sweep::scan(&store, sweep::DEFAULT_N_SESSIONS)?))
    })()
    .map_err(generic("Couldn't restore those"))
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
/// `discovery::discover` serves a cached result, degrades to a stale cache on
/// rate-limit/offline, and never errors — so an `Err` here just means an empty
/// feed while the catalog's listed-only rows keep the tab useful. We carry no
/// `authorClaims` for wild repos: a GitHub result has no vetted savings claim, and
/// Piggy never invents one.
fn discovery_feed(force: bool) -> Vec<DiscoverFeedItem> {
    match discovery::discover(force) {
        Ok(cache) => cache
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
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn discovered(force: bool) -> DiscoverDto {
    let catalog = Catalog::embedded();
    DiscoverDto {
        feed: discovery_feed(force),
        listed_only: listed_only_entries(&catalog),
    }
}

/// The Discover tab feed. Refreshes from GitHub at most once a day (handled inside
/// `piggy-core`); reads the cache otherwise.
pub fn discovered_list() -> DiscoverDto {
    discovered(false)
}

/// Manual "check now" refresh — forces a live GitHub search past the daily cache.
pub fn refresh_discovered() -> DiscoverDto {
    discovered(true)
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
