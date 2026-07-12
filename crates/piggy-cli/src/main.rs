//! `piggy` — measure Claude Code token usage and manage token-saving add-ons.
//!
//! Subcommands:
//!   * `index`  — scan `~/.claude/projects/**/*.jsonl` into the local DB.
//!   * `stats`  — human tables (or `--json`) of token usage and estimated cost.
//!   * `doctor` — environment / data-health checks.
//!   * `parse`  — dump one file's parsed aggregate as JSON (the jq cross-check).
//!   * `list`   — the saver catalog with on/off state and claimed savings.
//!   * `install` / `remove` — turn a saver on (install) or fully off (uninstall).
//!   * `on` / `off` — fast toggle without uninstalling (the A/B path).
//!   * `sweep`  — find unused add-ons that cost tokens; `--apply N` disables one.
//!   * `restore-defaults` — undo everything Piggy changed.
//!   * `backups` — list the settings.json backups Piggy has taken.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use piggy_core::{
    attribution::{self, Badge, HeadlineBaseline},
    config, discovery, engine, parse_file, run_index,
    stats::Totals,
    sweep, Catalog, Period, PiggyState, Pricing, SessionWatcher, Store,
};

#[derive(Parser)]
#[command(
    name = "piggy",
    about = "Measure Claude Code token usage from session logs."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan session logs into the local database (incremental by default).
    Index {
        /// Re-parse every file, ignoring the incremental cache.
        #[arg(long)]
        full: bool,
    },
    /// Show token usage and estimated cost.
    Stats {
        /// Time window. Omit to show a summary of all four windows.
        #[arg(long, value_enum)]
        period: Option<PeriodArg>,
        /// Break the chosen window down by project or model.
        #[arg(long, value_enum)]
        by: Option<ByArg>,
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Check environment and data health.
    Doctor,
    /// Parse a single .jsonl file and print its aggregate (JSON with --json).
    Parse {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List every saver and its on/off state.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Turn a saver on (download/enable + health check).
    Install {
        /// Saver id (e.g. `rtk`, `caveman`, `ponytail`).
        saver: String,
    },
    /// Turn a saver fully off and remove it (reversible; restores settings.json).
    Remove { saver: String },
    /// Fast-enable an already-installed saver (no re-download).
    On { saver: String },
    /// Fast-disable a saver without uninstalling it (the A/B path).
    Off { saver: String },
    /// Find unused add-ons that cost tokens; `--apply N` disables item N.
    Sweep {
        /// Disable the item with this index from the scan (reversible).
        #[arg(long, value_name = "N")]
        apply: Option<usize>,
        /// Look back over this many recent sessions for usage (default 50).
        #[arg(long, value_name = "N")]
        sessions: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Undo everything Piggy changed and restore settings.json to pre-Piggy.
    RestoreDefaults,
    /// List the settings.json backups Piggy has taken.
    Backups,
    /// Measured savings: per-saver attribution table + honest headline.
    Report {
        #[arg(long)]
        json: bool,
    },
    /// View or change the holdout fraction (the share of sessions run all-off).
    Holdout {
        /// Set the holdout fraction (0.0–0.5), e.g. `--fraction 0.1`.
        #[arg(long, value_name = "N")]
        fraction: Option<f64>,
        /// Turn the live holdout on (badges become measured once data arrives).
        #[arg(long, conflicts_with = "off")]
        on: bool,
        /// Turn the live holdout off (badges fall back to observational).
        #[arg(long)]
        off: bool,
    },
    /// Show token-savers discovered on GitHub (cached; `--refresh` forces a pull).
    Discover {
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Watch the projects dir and index + tag new sessions live (foreground).
    Watch {
        /// Process a single batch of events and exit (default: loop forever).
        #[arg(long)]
        once: bool,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum PeriodArg {
    Today,
    Week,
    Month,
    All,
}

impl From<PeriodArg> for Period {
    fn from(p: PeriodArg) -> Self {
        match p {
            PeriodArg::Today => Period::Today,
            PeriodArg::Week => Period::Week,
            PeriodArg::Month => Period::Month,
            PeriodArg::All => Period::All,
        }
    }
}

#[derive(Copy, Clone, ValueEnum)]
enum ByArg {
    Project,
    Model,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Index { full } => cmd_index(full),
        Cmd::Stats { period, by, json } => cmd_stats(period, by, json),
        Cmd::Doctor => {
            let ok = cmd_doctor()?;
            if !ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Parse { file, json } => cmd_parse(&file, json),
        Cmd::List { json } => cmd_list(json),
        Cmd::Install { saver } => cmd_install(&saver),
        Cmd::Remove { saver } => cmd_remove(&saver),
        Cmd::On { saver } => cmd_toggle(&saver, true),
        Cmd::Off { saver } => cmd_toggle(&saver, false),
        Cmd::Sweep {
            apply,
            sessions,
            json,
        } => cmd_sweep(apply, sessions, json),
        Cmd::RestoreDefaults => cmd_restore_defaults(),
        Cmd::Backups => cmd_backups(),
        Cmd::Report { json } => cmd_report(json),
        Cmd::Holdout { fraction, on, off } => cmd_holdout(fraction, on, off),
        Cmd::Discover { refresh, json } => cmd_discover(refresh, json),
        Cmd::Watch { once } => cmd_watch(once),
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// Per-saver "Measured" column labels for `piggy list`, computed from the same
/// attribution engine `piggy report` uses so the two commands never disagree.
/// Best-effort: a saver with no session data (or an unreadable store) simply
/// keeps the honest "not enough data yet" default.
fn measured_labels() -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let home = config::piggy_home();
    let Ok(store) = Store::open(&home) else {
        return out;
    };
    let pricing = Pricing::load(&home);
    let seed = time_seed();
    let Ok(ids) = store.tagged_saver_ids() else {
        return out;
    };
    for id in ids {
        if let Ok(a) = attribution::attribute(&store, &pricing, &id, seed) {
            if let Some(o) = a.output() {
                // Only surface a non-default label once there's a real figure or
                // an explicit measuring count; the output stream is the headline
                // per-saver number, matching `piggy report`.
                out.insert(id.clone(), stream_result(o));
            }
        }
    }
    out
}

fn cmd_list(json: bool) -> Result<()> {
    let catalog = Catalog::embedded();
    let state = PiggyState::load()?;

    if json {
        let arr: Vec<serde_json::Value> = catalog
            .ordered()
            .iter()
            .map(|e| {
                let st = state.savers.get(&e.id);
                serde_json::json!({
                    "id": e.id,
                    "name": e.name,
                    "plainLabel": e.plain_label,
                    "status": e.status,
                    "installType": e.install_type,
                    "installed": st.is_some(),
                    "enabled": st.map(|s| s.enabled).unwrap_or(false),
                    "installable": e.installable().is_ok() && e.has_install_steps(),
                    "behaviorChanging": e.behavior_changing,
                    "risk": e.risk,
                    "claimedSavings": e.claimed_savings,
                    "warning": e.warning,
                    "license": e.license,
                    "licenseNote": e.license_note,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    // Real per-saver measurement, so this column agrees with `piggy report`.
    let measured = measured_labels();
    let headers = ["", "Saver", "What it does", "State", "Measured", "Claimed"];
    let rows: Vec<Vec<String>> = catalog
        .ordered()
        .iter()
        .map(|e| {
            let st = state.savers.get(&e.id);
            let state_label = match st {
                Some(s) if s.enabled => "on",
                Some(_) => "off (installed)",
                None if !e.has_install_steps() || e.installable().is_err() => "unavailable",
                None => "available",
            };
            let dot = if e.behavior_changing { "!" } else { " " };
            vec![
                dot.to_string(),
                format!("{} ({})", e.name, e.id),
                e.plain_label
                    .clone()
                    .unwrap_or_else(|| e.description.clone()),
                state_label.to_string(),
                measured
                    .get(&e.id)
                    .cloned()
                    .unwrap_or_else(|| "not enough data yet".to_string()),
                e.claimed_savings.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    println!("Savers ( ! = changes how Claude behaves )");
    render_table(&headers, &rows);
    println!();
    println!(
        "measured = Piggy's own holdout measurement (arrives once you've run enough sessions)."
    );
    println!("claimed  = the author's number; treat as marketing until measured.");

    // License labels (the catalog promises Piggy shows these before install).
    let mut header_printed = false;
    for e in catalog.ordered() {
        if let Some(note) = &e.license_note {
            if !header_printed {
                println!();
                println!("license notes (shown before you turn one on):");
                header_printed = true;
            }
            println!("  {} ({}): {}", e.name, e.license, note);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// install / remove / on / off
// ---------------------------------------------------------------------------

fn cmd_install(saver: &str) -> Result<()> {
    let catalog = Catalog::embedded();
    // Show the license (and any source-available caveat) before turning it on.
    if let Some(entry) = catalog.get(saver) {
        let license = if entry.license.is_empty() {
            "(unspecified)"
        } else {
            entry.license.as_str()
        };
        println!("License: {license}");
        if let Some(note) = &entry.license_note {
            println!("  {note}");
        }
    }
    let report = engine::install(&catalog, saver)?;
    print_action(&report);
    if report.rolled_back {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_remove(saver: &str) -> Result<()> {
    let catalog = Catalog::embedded();
    let report = engine::uninstall(&catalog, saver)?;
    print_action(&report);
    Ok(())
}

fn cmd_toggle(saver: &str, on: bool) -> Result<()> {
    let catalog = Catalog::embedded();
    let report = engine::set_enabled(&catalog, saver, on)?;
    print_action(&report);
    Ok(())
}

fn print_action(report: &engine::ActionReport) {
    for m in &report.messages {
        println!("{m}");
    }
    if let Some(h) = &report.health {
        for (desc, passed, detail) in &h.checks {
            let mark = if *passed { "ok" } else { "FAIL" };
            println!("  [{mark}] {desc} — {detail}");
        }
    }
    for w in &report.warnings {
        println!("  note: {w}");
    }
}

// ---------------------------------------------------------------------------
// sweep
// ---------------------------------------------------------------------------

fn cmd_sweep(apply: Option<usize>, sessions: Option<usize>, json: bool) -> Result<()> {
    let home = config::piggy_home();
    let store = Store::open(&home)?;
    let n = sessions.unwrap_or(sweep::DEFAULT_N_SESSIONS);

    if let Some(idx) = apply {
        let mut state = PiggyState::load()?;
        let id = sweep::apply(&store, &mut state, idx, n)?;
        println!("disabled #{idx} ({id}). Reverse it any time with `piggy restore-defaults`.");
        return Ok(());
    }

    let report = sweep::scan(&store, n)?;
    if json {
        let arr: Vec<serde_json::Value> = report
            .items
            .iter()
            .map(|i| {
                serde_json::json!({
                    "idx": i.idx,
                    "kind": i.kind,
                    "id": i.id,
                    "source": i.source,
                    "used": i.used,
                    // Whether `used` is a windowed session count (MCP) or a
                    // lifetime total (plugin/skill) or not measurable (hook).
                    "usedScope": match i.kind.as_str() {
                        "mcp" => "window",
                        "hook" => "n/a",
                        _ => "lifetime",
                    },
                    "estTokens": i.est_tokens,
                    "estimated": true,
                    "recommendDisable": i.recommend_disable,
                    "reason": i.reason,
                })
            })
            .collect();
        let out = serde_json::json!({
            "sessionsConsidered": report.sessions_considered,
            "estRecoverableTokens": report.est_recoverable_tokens(),
            "estimated": true,
            "items": arr,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!(
        "Sweep — usage over the last {} session(s)",
        report.sessions_considered
    );
    if report.items.is_empty() {
        println!("  found no plugins, MCP servers, or skills to check.");
        return Ok(());
    }
    let headers = ["#", "Kind", "Add-on", "Used", "Est. tokens", "Suggestion"];
    let rows: Vec<Vec<String>> = report
        .items
        .iter()
        .map(|i| {
            vec![
                i.idx.to_string(),
                i.kind.clone(),
                i.id.clone(),
                commafy(i.used),
                format!("~{}", commafy(i.est_tokens)),
                if i.recommend_disable {
                    format!("turn off — {}", i.reason)
                } else {
                    "keep".to_string()
                },
            ]
        })
        .collect();
    render_table(&headers, &rows);
    println!();
    let rec = report.recommended().count();
    if rec > 0 {
        println!(
            "{rec} unused add-on(s), ~{} tokens/session (estimated). Turn one off: `piggy sweep --apply <#>`.",
            commafy(report.est_recoverable_tokens())
        );
    } else {
        println!("everything here is in use — nothing to sweep.");
    }
    println!("token costs are estimates (config-size heuristic), not measured.");
    println!(
        "MCP usage is over the last {} session(s); plugin/skill usage is a lifetime total (Claude Code keeps no per-session count for those); hooks are informational.",
        report.sessions_considered
    );
    if report.recommended().any(|i| i.kind == "mcp") {
        println!(
            "note: a project you use only occasionally can fall outside a {}-session window — if an MCP server you rely on was flagged, re-run with a wider `--sessions <N>`.",
            report.sessions_considered
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// restore-defaults / backups
// ---------------------------------------------------------------------------

fn cmd_restore_defaults() -> Result<()> {
    let report = engine::restore_defaults()?;
    for m in &report.messages {
        println!("{m}");
    }
    if report.byte_restored {
        println!("your Claude settings are back exactly as they were before Piggy.");
    }
    Ok(())
}

fn cmd_backups() -> Result<()> {
    let dir = config::backups_dir();
    let pre = dir.join("pre-piggy.json");
    if pre.exists() {
        let size = std::fs::metadata(&pre).map(|m| m.len()).unwrap_or(0);
        println!(
            "Restore Defaults target: {} ({} bytes)",
            pre.display(),
            commafy(size)
        );
    } else {
        println!("no pre-Piggy backup yet (Piggy hasn't written settings.json).");
    }
    let mut entries: Vec<(PathBuf, u64)> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("settings-") && n.ends_with(".json"))
                    .unwrap_or(false)
            })
            .map(|p| {
                let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                (p, sz)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    entries.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    println!();
    println!("Timestamped backups ({} kept):", entries.len());
    if entries.is_empty() {
        println!("  (none yet)");
    }
    for (p, sz) in entries.iter().take(20) {
        println!(
            "  {}  ({} bytes)",
            p.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            commafy(*sz)
        );
    }
    if entries.len() > 20 {
        println!("  … and {} more", entries.len() - 20);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// index
// ---------------------------------------------------------------------------

fn cmd_index(full: bool) -> Result<()> {
    let home = config::piggy_home();
    let projects = config::claude_projects_dir();
    if !projects.exists() {
        eprintln!(
            "error: Claude projects directory not found at {}",
            projects.display()
        );
        std::process::exit(1);
    }
    let pricing = Pricing::load(&home);
    let mut store = Store::open(&home)?;

    let start = Instant::now();
    let rep = run_index(&mut store, &pricing, &projects, full)?;
    let secs = start.elapsed().as_secs_f64();

    // Anchor the pre-install baseline the first time we index, then backfill the
    // `pre_install` tag onto every session that predates Piggy.
    let mut state = PiggyState::load()?;
    if state.ensure_created_at() {
        state.save()?;
    }
    let catalog = Catalog::embedded();
    let tagged = piggy_core::tagging::tag_pre_install_baseline(&mut store, &state, &catalog)?;

    println!("indexed {} in {:.2}s", projects.display(), secs);
    if tagged > 0 {
        println!(
            "  tagged {} pre-Piggy session(s) as the measurement baseline",
            commafy(tagged as u64)
        );
    }
    println!(
        "  files: {} scanned, {} updated, {} skipped{}",
        commafy(rep.scanned),
        commafy(rep.updated),
        commafy(rep.skipped),
        if rep.unreadable > 0 {
            format!(", {} unreadable", commafy(rep.unreadable))
        } else {
            String::new()
        }
    );
    println!("  sessions: {}", commafy(rep.sessions));
    if rep.parse_errors > 0 {
        println!(
            "  parse errors (skipped lines): {}",
            commafy(rep.parse_errors)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

fn cmd_stats(period: Option<PeriodArg>, by: Option<ByArg>, json: bool) -> Result<()> {
    let home = config::piggy_home();
    let store = Store::open(&home)?;

    if let Some(by) = by {
        let period: Period = period.map(Into::into).unwrap_or(Period::All);
        let rows = match by {
            ByArg::Project => store.by_project(period)?,
            ByArg::Model => store.by_model(period)?,
        };
        if json {
            print_groups_json(period, by, &rows)?;
        } else {
            print_groups_table(period, by, &rows);
        }
        return Ok(());
    }

    // No --by: a single window, or a summary of all four.
    let periods: Vec<Period> = match period {
        Some(p) => vec![p.into()],
        None => vec![Period::Today, Period::Week, Period::Month, Period::All],
    };
    let mut labelled = Vec::new();
    for p in &periods {
        labelled.push((*p, store.totals(*p)?));
    }
    if json {
        print_totals_json(&labelled)?;
    } else {
        print_totals_table(&labelled);
    }
    Ok(())
}

fn print_totals_table(rows: &[(Period, Totals)]) {
    let headers = [
        "Period",
        "Sessions",
        "Input",
        "Output",
        "Cache write",
        "Cache read",
        "Est. cost",
    ];
    let mut any_partial = false;
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|(p, t)| {
            any_partial |= !t.fully_priced() && t.total_tokens() > 0;
            vec![
                p.label().to_string(),
                commafy(t.sessions),
                commafy(t.input_tokens),
                commafy(t.output_tokens),
                commafy(t.cache_creation_tokens),
                commafy(t.cache_read_tokens),
                cost_cell(t),
            ]
        })
        .collect();
    println!("Token usage (cost estimated)");
    render_table(&headers, &table);
    print_cost_footnote(any_partial);
}

fn print_groups_table(period: Period, by: ByArg, rows: &[piggy_core::GroupRow]) {
    let first = match by {
        ByArg::Project => "Project",
        ByArg::Model => "Model",
    };
    let headers = [
        first,
        "Sessions",
        "Input",
        "Output",
        "Cache write",
        "Cache read",
        "Est. cost",
    ];
    let mut any_partial = false;
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|g| {
            let t = &g.totals;
            any_partial |= !t.fully_priced() && t.total_tokens() > 0;
            vec![
                g.key.clone(),
                commafy(t.sessions),
                commafy(t.input_tokens),
                commafy(t.output_tokens),
                commafy(t.cache_creation_tokens),
                commafy(t.cache_read_tokens),
                cost_cell(t),
            ]
        })
        .collect();
    println!(
        "{} — by {} (cost estimated)",
        period.label(),
        first.to_lowercase()
    );
    if table.is_empty() {
        println!("  (no data — run `piggy index`)");
        return;
    }
    render_table(&headers, &table);
    print_cost_footnote(any_partial);
}

fn cost_cell(t: &Totals) -> String {
    if t.total_tokens() == 0 {
        "-".to_string()
    } else if t.fully_priced() {
        format!("${:.2}", t.cost_usd_est)
    } else if t.cost_usd_est > 0.0 {
        format!("${:.2}*", t.cost_usd_est)
    } else {
        "n/a*".to_string()
    }
}

fn print_cost_footnote(any_partial: bool) {
    println!();
    println!("costs are estimated (not billed amounts).");
    if any_partial {
        println!(
            "* some tokens use a model with no known price and are excluded from the estimate."
        );
    }
}

fn totals_json(t: &Totals) -> serde_json::Value {
    serde_json::json!({
        "sessions": t.sessions,
        "input_tokens": t.input_tokens,
        "output_tokens": t.output_tokens,
        "cache_creation_tokens": t.cache_creation_tokens,
        "cache_creation_1h_tokens": t.cache_creation_1h_tokens,
        "cache_read_tokens": t.cache_read_tokens,
        "cost_usd_est": round2(t.cost_usd_est),
        "cost_estimated": true,
        "unpriced_tokens": t.unpriced_tokens,
    })
}

fn print_totals_json(rows: &[(Period, Totals)]) -> Result<()> {
    let obj: serde_json::Map<String, serde_json::Value> = rows
        .iter()
        .map(|(p, t)| (period_key(*p).to_string(), totals_json(t)))
        .collect();
    println!("{}", serde_json::to_string_pretty(&obj)?);
    Ok(())
}

fn print_groups_json(period: Period, by: ByArg, rows: &[piggy_core::GroupRow]) -> Result<()> {
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|g| {
            let mut v = totals_json(&g.totals);
            v.as_object_mut()
                .unwrap()
                .insert("key".to_string(), serde_json::Value::String(g.key.clone()));
            v
        })
        .collect();
    let out = serde_json::json!({
        "period": period_key(period),
        "by": match by { ByArg::Project => "project", ByArg::Model => "model" },
        "rows": arr,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn period_key(p: Period) -> &'static str {
    match p {
        Period::Today => "today",
        Period::Week => "week",
        Period::Month => "month",
        Period::All => "all",
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

fn cmd_doctor() -> Result<bool> {
    let mut ok = true;
    let home = config::piggy_home();
    let projects = config::claude_projects_dir();

    // 1. Claude projects directory exists and is readable.
    match std::fs::read_dir(&projects) {
        Ok(_) => println!("✅ Claude projects dir readable: {}", projects.display()),
        Err(e) => {
            println!(
                "⚠️  Claude projects dir not readable: {} ({e})",
                projects.display()
            );
            ok = false;
        }
    }

    // 2. settings.json parses (read-only).
    let settings = config::claude_settings_path();
    if settings.exists() {
        match std::fs::read_to_string(&settings)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        {
            Some(_) => println!("✅ settings.json parses: {}", settings.display()),
            None => println!(
                "⚠️  settings.json present but does not parse: {}",
                settings.display()
            ),
        }
    } else {
        println!(
            "✅ settings.json absent (nothing to check): {}",
            settings.display()
        );
    }

    // 3. Database writable.
    match Store::open(&home).and_then(|s| s.write_test().map(|_| s)) {
        Ok(store) => {
            println!("✅ database writable: {}", home.join("piggy.db").display());

            // 4. Pricing coverage.
            let pricing = Pricing::load(&home);
            match store.pricing_coverage() {
                Ok((matched, total)) if total > 0 => {
                    let pct = 100.0 * matched as f64 / total as f64;
                    let mark = if pct >= 99.0 { "✅" } else { "⚠️ " };
                    println!(
                        "{mark} pricing coverage: {:.1}% of tokens matched to a known price ({} models in table)",
                        pct,
                        pricing.model_count()
                    );
                }
                Ok(_) => println!(
                    "✅ pricing table loaded ({} models); no indexed tokens yet — run `piggy index`",
                    pricing.model_count()
                ),
                Err(e) => {
                    println!("⚠️  could not compute pricing coverage: {e}");
                }
            }

            // 5. Parse errors across indexed sessions.
            match store.total_parse_errors() {
                Ok(0) => println!("✅ no parse errors recorded"),
                Ok(n) => println!(
                    "⚠️  {} malformed line(s) skipped across indexed sessions",
                    commafy(n)
                ),
                Err(e) => println!("⚠️  could not read parse-error count: {e}"),
            }
        }
        Err(e) => {
            println!("⚠️  database not writable at {}: {e}", home.display());
            ok = false;
        }
    }

    // 6. Health of active savers (spec: health checks also run on `piggy doctor`).
    let catalog = Catalog::embedded();
    match PiggyState::load() {
        Ok(state) => {
            let enabled: Vec<&String> = state
                .savers
                .iter()
                .filter(|(_, s)| s.enabled)
                .map(|(id, _)| id)
                .collect();
            if enabled.is_empty() {
                println!("✅ no active savers to health-check");
            }
            for id in enabled {
                match engine::health_check(&catalog, id) {
                    Ok(h) if h.ok() => println!("✅ saver '{id}' healthy"),
                    Ok(h) => {
                        ok = false;
                        let failed: Vec<String> = h
                            .checks
                            .iter()
                            .filter(|(_, passed, _)| !passed)
                            .map(|(desc, _, detail)| format!("{desc} ({detail})"))
                            .collect();
                        println!("⚠️  saver '{id}' unhealthy: {}", failed.join("; "));
                    }
                    Err(e) => {
                        ok = false;
                        println!("⚠️  saver '{id}' health check errored: {e}");
                    }
                }
            }
        }
        Err(e) => println!("⚠️  could not read Piggy state for saver health checks: {e}"),
    }

    println!();
    println!(
        "{}",
        if ok {
            "doctor: OK"
        } else {
            "doctor: problems found"
        }
    );
    Ok(ok)
}

// ---------------------------------------------------------------------------
// parse (utility / verification)
// ---------------------------------------------------------------------------

fn cmd_parse(file: &Path, json: bool) -> Result<()> {
    let parse = parse_file(file)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&parse)?);
    } else {
        println!("session: {}", parse.session_id);
        println!("project: {}", parse.project_path.as_deref().unwrap_or("-"));
        println!("branch:  {}", parse.git_branch.as_deref().unwrap_or("-"));
        println!(
            "span:    {} .. {}",
            parse.first_ts.as_deref().unwrap_or("-"),
            parse.last_ts.as_deref().unwrap_or("-")
        );
        println!(
            "messages: {} assistant, {} user, {} tool-results, {} parse errors",
            parse.n_assistant_msgs, parse.n_user_msgs, parse.n_tool_results, parse.parse_errors
        );
        for (model, t) in &parse.models {
            println!(
                "  {model}: in={} out={} cache_write={} (1h={}) cache_read={}",
                commafy(t.input_tokens),
                commafy(t.output_tokens),
                commafy(t.cache_creation_tokens),
                commafy(t.cache_creation_1h_tokens),
                commafy(t.cache_read_tokens),
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// report (measured attribution)
// ---------------------------------------------------------------------------

/// A time-derived bootstrap seed for production runs (tests pass a fixed one).
fn time_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1
}

fn cmd_report(json: bool) -> Result<()> {
    let home = config::piggy_home();
    let pricing = Pricing::load(&home);
    let store = Store::open(&home)?;
    let seed = time_seed();

    let hl = attribution::headline(&store, &pricing, seed)?;
    let saver_ids = store.tagged_saver_ids()?;
    let mut attribs = Vec::new();
    for id in &saver_ids {
        attribs.push(attribution::attribute(&store, &pricing, id, seed)?);
    }

    if json {
        let out = serde_json::json!({
            "headline": headline_json(&hl),
            "savers": attribs.iter().map(saver_json).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    // ---- Headline block --------------------------------------------------
    // The banner must not claim "holdout-based" when there is no live holdout.
    match hl.baseline {
        HeadlineBaseline::Holdout => println!("Piggy report — measured savings (holdout-based)"),
        HeadlineBaseline::PreInstall => println!(
            "Piggy report — estimated savings (observational pre-install baseline, no live holdout yet)"
        ),
        HeadlineBaseline::None => println!("Piggy report — not enough data yet"),
    }
    println!();
    let baseline_label = match hl.baseline {
        HeadlineBaseline::Holdout => "holdout",
        HeadlineBaseline::PreInstall => "pre-install history",
        HeadlineBaseline::None => "—",
    };
    if hl.baseline == HeadlineBaseline::None {
        println!("Headline: not enough data yet — need holdout or pre-install sessions.");
    } else {
        println!(
            "Headline (full-on {} vs {} {} sessions):",
            hl.n_full_on, baseline_label, hl.n_baseline
        );
        let per_turn_label = match hl.baseline {
            HeadlineBaseline::Holdout => "  measured per-turn savings:",
            _ => "  estimated per-turn savings (observational baseline):",
        };
        println!("{per_turn_label}");
        for s in &hl.streams {
            // Cache read is the cheap stream; keep it but de-emphasise below output.
            // Per docs UI copy: show the backing session count on the number line.
            println!("    {:<12}{}", s.stream.label(), stream_result_with_n(s));
        }
        match hl.multiplier {
            Some(m) if hl.n_full_on > 0 => {
                println!();
                println!("  Your plan lasts {m:.1}× longer  (estimated — price-weighted, cache reads excluded)");
            }
            _ => {}
        }
        if hl.baseline == HeadlineBaseline::PreInstall {
            println!(
                "  note: baseline is pre-install history (observational — no live holdout yet)."
            );
        }
    }

    // ---- Per-saver attribution table -------------------------------------
    println!();
    if attribs.is_empty() {
        println!("No per-saver data yet. Run sessions with savers rotating on and off.");
        return Ok(());
    }
    println!("Per-saver attribution (measured, per-turn rates)");
    let headers = ["Saver", "Stream", "Result", "90% CI", "On", "Off"];
    let mut rows: Vec<Vec<String>> = Vec::new();
    for a in &attribs {
        for (i, s) in a.streams.iter().enumerate() {
            rows.push(vec![
                if i == 0 {
                    a.saver_id.clone()
                } else {
                    String::new()
                },
                s.stream.label().to_string(),
                stream_result(s),
                ci_cell(s),
                s.n_on.to_string(),
                s.n_off.to_string(),
            ]);
        }
    }
    render_table(&headers, &rows);
    println!();
    println!(
        "measured = bootstrap CI excludes zero (positive width, family-corrected across the 4 \
         streams), ≥{} randomized sessions per side; interval shown is 90%.",
        attribution::MIN_GROUP
    );
    println!(
        "estimated = same math against the observational pre-install baseline (no live holdout yet)."
    );
    println!("the × multiplier is estimated (uses price weights).");
    // Flag any pre-install (observational) OFF sessions. These never count
    // toward a measured badge — they are only a fallback for an `estimated`
    // figure when randomized OFF data is short.
    for a in &attribs {
        if let Some(n) = a.off_by_source.get("pre_install") {
            if *n > 0 {
                println!(
                    "  {}: {} pre-install (observational) OFF sessions — never used for a measured badge.",
                    a.saver_id, n
                );
            }
        }
    }
    Ok(())
}

/// A measured/estimated/measuring result cell for one stream.
fn stream_result(s: &piggy_core::StreamStat) -> String {
    let word = match s.badge {
        Badge::Measured => "measured",
        Badge::Estimated => "estimated",
        Badge::Measuring => return format!("not enough data yet (+{})", s.n_on.min(s.n_off)),
    };
    let pct = s.delta.unwrap_or(0.0) * 100.0;
    if pct >= 0.0 {
        format!("{word} {:.0}% less", pct)
    } else {
        format!("{word} {:.0}% more", -pct)
    }
}

/// Like [`stream_result`] but appends the backing session count, per the docs'
/// UI copy rule (`measured 22% · 41 sessions`). Used on the headline lines,
/// where the per-side On/Off columns of the saver table aren't present.
fn stream_result_with_n(s: &piggy_core::StreamStat) -> String {
    let base = stream_result(s);
    if s.badge.shows_number() {
        format!("{base} · {} sessions", s.n_on + s.n_off)
    } else {
        base
    }
}

/// The confidence-interval cell (only meaningful once a number is shown).
fn ci_cell(s: &piggy_core::StreamStat) -> String {
    match (s.badge.shows_number(), s.ci) {
        (true, Some((lo, hi))) => format!("[{:.0}%, {:.0}%]", lo * 100.0, hi * 100.0),
        _ => "-".to_string(),
    }
}

fn headline_json(hl: &piggy_core::Headline) -> serde_json::Value {
    serde_json::json!({
        "baseline": match hl.baseline {
            HeadlineBaseline::Holdout => "holdout",
            HeadlineBaseline::PreInstall => "pre_install",
            HeadlineBaseline::None => "none",
        },
        "observational": hl.baseline == HeadlineBaseline::PreInstall,
        "nFullOn": hl.n_full_on,
        "nBaseline": hl.n_baseline,
        "multiplier": hl.multiplier,
        "multiplierEstimated": true,
        "streams": hl.streams.iter().map(stream_json).collect::<Vec<_>>(),
    })
}

fn saver_json(a: &piggy_core::SaverAttribution) -> serde_json::Value {
    serde_json::json!({
        "saver": a.saver_id,
        "nOn": a.n_on,
        "nOff": a.n_off,
        "offBySource": a.off_by_source,
        "streams": a.streams.iter().map(stream_json).collect::<Vec<_>>(),
    })
}

fn stream_json(s: &piggy_core::StreamStat) -> serde_json::Value {
    serde_json::json!({
        "stream": s.stream.label(),
        "badge": match s.badge {
            Badge::Measured => "measured",
            Badge::Estimated => "estimated",
            Badge::Measuring => "measuring",
        },
        "measured": s.badge == Badge::Measured,
        "estimated": s.badge == Badge::Estimated,
        // Point figure shown for both measured and estimated; null while measuring.
        "deltaPct": s.shown_pct(),
        "ci": s.ci.map(|(lo, hi)| [lo * 100.0, hi * 100.0]),
        "nOn": s.n_on,
        "nOff": s.n_off,
    })
}

// ---------------------------------------------------------------------------
// holdout
// ---------------------------------------------------------------------------

fn cmd_holdout(fraction: Option<f64>, on: bool, off: bool) -> Result<()> {
    let mut state = PiggyState::load()?;
    let mut changed = false;
    if let Some(f) = fraction {
        if !(0.0..=0.5).contains(&f) {
            bail!("holdout fraction must be between 0.0 and 0.5 (got {f})");
        }
        state.settings.holdout_fraction = f;
        changed = true;
    }
    if on {
        state.settings.holdout_enabled = true;
        changed = true;
    }
    if off {
        state.settings.holdout_enabled = false;
        changed = true;
    }
    if changed {
        state.ensure_created_at();
        state.save()?;
    }
    println!(
        "Holdout: {} · fraction {:.0}%",
        if state.settings.holdout_enabled {
            "on"
        } else {
            "off"
        },
        state.settings.holdout_fraction * 100.0
    );
    println!(
        "Piggy occasionally runs a session with savers off to measure honestly. When off, badges say 'estimated'."
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// discover
// ---------------------------------------------------------------------------

fn cmd_discover(refresh: bool, json: bool) -> Result<()> {
    let cache = discovery::discover(refresh)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&cache)?);
        return Ok(());
    }
    println!(
        "Discovered token-savers (refreshed {}{})",
        cache.refreshed_at,
        if cache.stale {
            " · stale, GitHub unavailable"
        } else {
            ""
        }
    );
    if cache.repos.is_empty() {
        println!("  (nothing found — try `piggy discover --refresh`)");
        return Ok(());
    }
    let headers = ["Stars", "Repo", "What it is"];
    let rows: Vec<Vec<String>> = cache
        .repos
        .iter()
        .map(|r| {
            let what = if r.listed_only {
                "listed only — not installable".to_string()
            } else {
                r.description.clone().unwrap_or_default()
            };
            vec![
                if r.listed_only {
                    "—".to_string()
                } else {
                    commafy(r.stars)
                },
                r.full_name.clone(),
                truncate(&what, 60),
            ]
        })
        .collect();
    render_table(&headers, &rows);
    // Exclusion reasons for listed-only tools.
    let listed: Vec<_> = cache.repos.iter().filter(|r| r.listed_only).collect();
    if !listed.is_empty() {
        println!();
        println!("Listed for transparency, never installed by Piggy:");
        for r in listed {
            println!(
                "  {} — {}",
                r.full_name,
                r.exclusion_reason.as_deref().unwrap_or("(no reason given)")
            );
        }
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

fn cmd_watch(once: bool) -> Result<()> {
    let home = config::piggy_home();
    let projects = config::claude_projects_dir();
    let pricing = Pricing::load(&home);

    // Anchor the pre-install baseline so live sessions are attributed correctly.
    let mut state = PiggyState::load()?;
    if state.ensure_created_at() {
        state.save()?;
    }

    let mut watcher = SessionWatcher::new(projects.clone(), &home)?;
    println!("watching {} (Ctrl-C to stop)…", projects.display());
    loop {
        let events = watcher.tick(Duration::from_secs(2), &pricing)?;
        for e in &events {
            println!(
                "  {}  session {}{}",
                e.path.display(),
                e.session_id,
                if e.newly_tagged { "  [tagged]" } else { "" }
            );
        }
        if once {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// formatting helpers
// ---------------------------------------------------------------------------

/// Insert thousands separators into a non-negative integer.
fn commafy(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Render a table: first column left-aligned, the rest right-aligned, two
/// spaces between columns.
fn render_table(headers: &[&str], rows: &[Vec<String>]) {
    let ncol = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncol) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let render_row = |cells: &[String]| -> String {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate().take(ncol) {
            if i == 0 {
                line.push_str(&format!("{:<w$}", cell, w = widths[i]));
            } else {
                line.push_str("  ");
                line.push_str(&format!("{:>w$}", cell, w = widths[i]));
            }
        }
        line
    };

    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    println!("{}", render_row(&header_cells));
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", render_row(&sep));
    for row in rows {
        println!("{}", render_row(row));
    }
}
