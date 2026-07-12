//! The backend bridge: thin, typed wrappers over `piggy-core` that the Tauri
//! command layer calls. Every function here returns a plain `#[derive(Serialize)]`
//! struct (never a raw `serde_json::Value`) so IPC serialization is unaffected by
//! `piggy-core`'s `arbitrary_precision` serde_json feature.
//!
//! ## M3 fallbacks
//!
//! The measurement milestone (M3) — holdout deltas, the headline multiplier, the
//! discovered feed — does not exist in this worktree's `piggy-core`. Every place
//! that would consume an M3 value degrades to an honest, typed fallback and is
//! tagged with a `// M3-WIRE:` comment so wiring it up later is one grep away:
//!
//! * per-saver badge  → `{ kind: "measuring", n: <sessions counted> }`
//! * headline         → `{ label: "not_enough_data" }`
//! * discovered feed  → catalog `listed_only`/deferred entries only, empty feed.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use piggy_core::registry::Entry;
use piggy_core::{config, engine, stats::Period, sweep, Catalog, PiggyState, Pricing, Store};

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
    /// At least one session has been indexed.
    pub has_data: bool,
    pub sessions: u64,
}

pub fn environment() -> Environment {
    let claude_installed = config::claude_dir().exists() || config::claude_projects_dir().exists();
    let sessions = session_count();
    Environment {
        claude_installed,
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
            // M3-WIRE: the "lasts N× longer" headline needs the M3 holdout engine,
            // which is absent here. Degrade to an honest "not_enough_data" so the
            // UI never fabricates a savings multiplier.
            headline: Headline {
                value: None,
                label: "not_enough_data".to_string(),
                n_holdout: 0,
            },
        })
    })()
    .map_err(generic("Couldn't read your token history"))
}

// ---------------------------------------------------------------------------
// savers_list / toggles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Badge {
    /// `"measured" | "measuring" | "claimed"`.
    pub kind: String,
    /// Measured delta fraction (negative = saving), or `null`.
    pub delta: Option<f64>,
    /// Sessions counted so far.
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaversState {
    pub master_on: bool,
    pub savers: Vec<SaverRow>,
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

fn saver_row(e: &Entry, state: &PiggyState, sessions: u64) -> SaverRow {
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
        // M3-WIRE: no measured per-saver delta exists yet. Report an honest
        // "measuring" badge carrying how many sessions Piggy has counted so far.
        badge: Badge {
            kind: "measuring".to_string(),
            delta: None,
            n: sessions,
        },
    }
}

fn master_is_on(catalog: &Catalog, state: &PiggyState) -> bool {
    let ids = default_on_ids(catalog);
    !ids.is_empty()
        && ids
            .iter()
            .all(|id| state.savers.get(id).map(|s| s.enabled).unwrap_or(false))
}

fn build_savers_state() -> anyhow::Result<SaversState> {
    let catalog = Catalog::embedded();
    let state = PiggyState::load()?;
    let sessions = session_count();
    let savers = curated_installable(&catalog)
        .iter()
        .map(|e| saver_row(e, &state, sessions))
        .collect();
    Ok(SaversState {
        master_on: master_is_on(&catalog, &state),
        savers,
    })
}

pub fn savers_list() -> Result<SaversState, ApiError> {
    build_savers_state().map_err(generic("Couldn't read your savers"))
}

/// Turn a single saver on or off. `on` when not installed installs it (with the
/// engine's own health-check + rollback); `off` uses the fast A/B disable path.
pub fn saver_toggle(id: String, on: bool) -> Result<SaversState, ApiError> {
    let catalog = Catalog::embedded();
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
        Ok(_) => build_savers_state().map_err(generic("Couldn't read your savers")),
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
                    first_sentence(&e.to_string()),
                    false,
                ));
            }
        }
    }

    build_savers_state().map_err(generic("Couldn't read your savers"))
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
    /// M3-WIRE: live discovery results. Empty until the M3 discovery module lands.
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

pub fn discovered_list() -> DiscoverDto {
    let catalog = Catalog::embedded();
    let listed_only = catalog
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
        .collect();
    // M3-WIRE: no live discovery feed in this worktree — return only the catalog's
    // listed/deferred entries so the tab is honest and nothing here is installable.
    DiscoverDto {
        feed: Vec::new(),
        listed_only,
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
    // M3-WIRE: without the holdout engine the savings/multiplier are unknown, so
    // the card is honestly "still measuring" and Share stays disabled.
    let shareable = ov.headline.label == "measured";
    Ok(ShareCardData {
        period: ov.period,
        week_label: date_range_label(period),
        tokens_saved: None,
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

/// The persisted slice of settings (holdout fraction + rotation). Launch-at-login
/// is owned by the autostart plugin and merged in at the command layer.
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

fn prefs_path() -> PathBuf {
    config::piggy_home().join("app-settings.json")
}

// M3-WIRE: holdoutFraction and rotationEnabled are persisted preferences only.
// They do not yet feed a measurement/rotation engine (that arrives with M3); the
// values are stored faithfully so the M3 glue can read them straight out.
pub fn load_prefs() -> AppPrefs {
    std::fs::read(prefs_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_prefs(prefs: &AppPrefs) -> Result<(), ApiError> {
    let clamped = AppPrefs {
        holdout_fraction: prefs.holdout_fraction.clamp(0.0, 0.5),
        rotation_enabled: prefs.rotation_enabled,
    };
    let path = prefs_path();
    (|| -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&clamped)?;
        std::fs::write(&path, json)?;
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
        let projects = config::claude_projects_dir();
        if !projects.exists() {
            return Ok(ReindexDto {
                ran: false,
                sessions: 0,
                updated: 0,
                scanned: 0,
            });
        }
        let pricing = Pricing::load(&home);
        let mut store = Store::open(&home)?;
        let rep = piggy_core::run_index(&mut store, &pricing, &projects, false)?;
        Ok(ReindexDto {
            ran: true,
            sessions: rep.sessions,
            updated: rep.updated,
            scanned: rep.scanned,
        })
    })()
    .map_err(generic("Couldn't read your latest sessions"))
}
