//! `piggy` - measure Claude Code token usage and manage token-saving add-ons.
//!
//! Subcommands:
//!   * `index`  - scan `~/.claude/projects/**/*.jsonl` into the local DB.
//!   * `stats`  - human tables (or `--json`) of token usage and estimated cost.
//!   * `doctor` - environment / data-health checks.
//!   * `parse`  - dump one file's parsed aggregate as JSON (the jq cross-check).
//!   * `list`   - the saver catalog with on/off state and claimed savings.
//!   * `install` / `remove` - turn a saver on (install) or fully off (uninstall).
//!   * `on` / `off` - fast toggle without uninstalling (the A/B path).
//!   * `sweep`  - find unused add-ons that cost tokens; `--apply N` disables one.
//!   * `probe`  - measure an MCP server's tool-schema cost by launching it once.
//!   * `report` - measured savings: per-saver attribution table + honest headline.
//!   * `ledger` - where context tokens come from; exact, needs no A/B.
//!   * `insights` - ledger findings, each with the lever that acts on it.
//!   * `claudemd` - the CLAUDE.md inventory every session loads, and its findings.
//!   * `holdout` - view or change the share of sessions that run with savers off.
//!   * `discover` - token-savers found on GitHub (cached; `--refresh` pulls).
//!   * `watch`  - index and tag new sessions live, in the foreground.
//!   * `restore-defaults` - undo everything Piggy changed.
//!   * `backups` - list the settings.json backups Piggy has taken.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use piggy_core::{
    advice::{self, GenerateOptions},
    attribution::{self, Badge, HeadlineBaseline},
    claudemd, config, discovery, engine, parse_file, probe, snapshots,
    stats::Totals,
    sweep, Catalog, Period, PiggyState, Pricing, SessionWatcher, Store,
};

#[derive(Parser)]
#[command(
    name = "piggy",
    version,
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
    /// Measure what your MCP servers' tool schemas cost, by launching one once.
    ///
    /// With no arguments this only lists what is configured and what has been
    /// measured; nothing is launched until you name a server or pass `--all`.
    Probe {
        /// Measure just this server (its key in `~/.claude.json`).
        #[arg(long, value_name = "KEY", conflicts_with = "all")]
        server: Option<String>,
        /// Measure every configured stdio server (requires `--yes`).
        #[arg(long)]
        all: bool,
        /// Confirm launching each configured server once.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Undo everything Piggy changed and restore settings.json to pre-Piggy.
    RestoreDefaults,
    /// List everything Piggy can put back: settings.json backups, files it
    /// edited, and MCP servers it re-scoped.
    Backups,
    /// Measured savings: per-saver attribution table + honest headline.
    Report {
        #[arg(long)]
        json: bool,
    },
    /// Findings from the ledger: what your tokens went to, and the lever for each.
    Insights {
        /// Only consider sessions started on/after this date (e.g. `2026-07-01`).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// The CLAUDE.md files every session loads: what they cost, and what is
    /// wrong with them.
    Claudemd {
        #[arg(long)]
        json: bool,
    },
    /// Suggestions with the evidence behind each: what to switch off, move,
    /// clean up or trim. Listing only - applying is done in the app.
    Advise {
        /// Look back over this many recent sessions for usage (default 50).
        #[arg(long, value_name = "N")]
        sessions: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Where your context tokens come from: exact per-source ledger, no A/B needed.
    Ledger {
        /// Only count sessions started on/after this date (e.g. `2026-07-01`).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// How many projects to show (default 10; the rest are summarized).
        #[arg(long, value_name = "N")]
        projects: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Per-project task spend and outcomes: what each project cost and how
    /// often its tool calls failed.
    Tasks {
        /// Time window (default `week`). The prior window of the same length is
        /// the comparison, so `--period all` reports no delta.
        #[arg(long, value_enum)]
        period: Option<PeriodArg>,
        /// How many projects to show (default 10; the rest are summarized).
        #[arg(long, value_name = "N")]
        projects: Option<usize>,
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
        Cmd::Probe {
            server,
            all,
            yes,
            json,
        } => cmd_probe(server.as_deref(), all, yes, json),
        Cmd::RestoreDefaults => cmd_restore_defaults(),
        Cmd::Backups => cmd_backups(),
        Cmd::Report { json } => cmd_report(json),
        Cmd::Insights { since, json } => cmd_insights(since.as_deref(), json),
        Cmd::Claudemd { json } => cmd_claudemd(json),
        Cmd::Advise { sessions, json } => cmd_advise(sessions, json),
        Cmd::Ledger {
            since,
            projects,
            json,
        } => cmd_ledger(since.as_deref(), projects.unwrap_or(10), json),
        Cmd::Tasks {
            period,
            projects,
            json,
        } => cmd_tasks(
            period.unwrap_or(PeriodArg::Week).into(),
            projects.unwrap_or(10),
            json,
        ),
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
            println!("  [{mark}] {desc} - {detail}");
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
                    // Whether the *count* is an estimate, which is not the same
                    // as where the bytes came from: the shipped tokenizer
                    // divides bytes by 3.5, so a measured manifest still yields
                    // an estimated token count. `piggy probe --json` says the
                    // same thing about the same row.
                    "estimated": i.tokens_estimated,
                    // "rough estimate" (config-size heuristic) or "measured
                    // manifest" (this server's schemas, as probed).
                    "costBasis": i.cost_basis,
                    "recommendDisable": i.recommend_disable,
                    // The project a user-scope MCP server should move to,
                    // when its calls all come from one.
                    "scopeTo": i.scope_to,
                    "reason": i.reason,
                })
            })
            .collect();
        let out = serde_json::json!({
            "sessionsConsidered": report.sessions_considered,
            "estRecoverableTokens": report.est_recoverable_tokens(),
            // The total is only exact when every count behind it is.
            "estimated": report.recommended().any(|i| i.tokens_estimated),
            "items": arr,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!(
        "Sweep - usage over the last {} session(s)",
        report.sessions_considered
    );
    if report.items.is_empty() {
        println!("  found no plugins, MCP servers, or skills to check.");
        return Ok(());
    }
    let headers = ["#", "Kind", "Add-on", "Used", "Tokens/session", "Suggestion"];
    let rows: Vec<Vec<String>> = report
        .items
        .iter()
        .map(|i| {
            vec![
                i.idx.to_string(),
                i.kind.clone(),
                i.id.clone(),
                commafy(i.used),
                // The tilde is about the count, the trailing label is about
                // where the bytes came from. A probed server measured with the
                // bytes/3.5 tokenizer is honestly both: "~12,345 measured
                // manifest".
                {
                    let tilde = if i.tokens_estimated { "~" } else { "" };
                    if i.cost_basis == sweep::COST_BASIS_MEASURED {
                        format!("{tilde}{} {}", commafy(i.est_tokens), i.cost_basis)
                    } else {
                        format!("{tilde}{}", commafy(i.est_tokens))
                    }
                },
                if i.recommend_disable {
                    format!("turn off - {}", i.reason)
                } else if let Some(project) = &i.scope_to {
                    format!("scope to {project}")
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
        // The total mixes bases the moment one server has been probed, so say so
        // rather than calling a measured figure a guess.
        let basis = match (
            report
                .recommended()
                .any(|i| i.cost_basis == sweep::COST_BASIS_MEASURED),
            report
                .recommended()
                .any(|i| i.cost_basis != sweep::COST_BASIS_MEASURED),
        ) {
            (true, true) => "measured where probed, estimated elsewhere",
            (true, false) => "measured manifests, estimated session impact",
            _ => "estimated",
        };
        println!(
            "{rec} unused add-on(s), ~{} tokens/session ({basis}). Turn one off: `piggy sweep --apply <#>`.",
            commafy(report.est_recoverable_tokens())
        );
    } else {
        println!("everything here is in use - nothing to sweep.");
    }
    let rescope = report.rescope().count();
    if rescope > 0 {
        println!(
            "{rescope} MCP server(s) are used in one project but configured at user scope, so every other session loads them for nothing. `piggy advise` lists the move with its evidence, and the app applies it with a one-click undo."
        );
    }
    if report
        .items
        .iter()
        .any(|i| i.cost_basis == sweep::COST_BASIS_MEASURED)
    {
        println!("token costs are estimates (config-size heuristic) except the rows marked `measured manifest`, whose tool schemas `piggy probe` read from the server itself. A `~` on one of those means the schema bytes are real and only their conversion to tokens is an estimate.");
    } else {
        println!("token costs are estimates (config-size heuristic), not measured. `piggy probe` measures an MCP server's real tool schemas.");
    }
    println!(
        "MCP usage is over the last {} session(s); plugin/skill usage is a lifetime total (Claude Code keeps no per-session count for those); hooks are informational.",
        report.sessions_considered
    );
    if report.recommended().any(|i| i.kind == "mcp") {
        println!(
            "note: a project you use only occasionally can fall outside a {}-session window - if an MCP server you rely on was flagged, re-run with a wider `--sessions <N>`.",
            report.sessions_considered
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

fn cmd_probe(server: Option<&str>, all: bool, yes: bool, json: bool) -> Result<()> {
    let home = config::piggy_home();
    let mut store = Store::open(&home)?;
    let servers = probe::configured_servers()?;

    // Which servers this run will launch, if any. No arguments launches nothing:
    // it is the listing.
    let targets: Vec<probe::ConfiguredServer> = match (server, all) {
        (Some(key), _) => {
            let hits: Vec<_> = servers.iter().filter(|s| s.key == key).cloned().collect();
            if hits.is_empty() {
                bail!(
                    "no MCP server named '{key}' in {} (run `piggy probe` to see the list)",
                    config::claude_json_path().display()
                );
            }
            hits
        }
        (None, true) => {
            let stdio = servers
                .iter()
                .filter(|s| s.transport == probe::Transport::Stdio)
                .count();
            // The user already told Claude Code to run these every session, but
            // Piggy does not execute anything on its own say-so.
            if !yes {
                bail!(
                    "`piggy probe --all` starts each of your {stdio} stdio MCP server(s) once, \
                     asks for its tool list, and stops it again. Nothing is installed, changed, \
                     or left running. Re-run with `--yes` if that is what you want."
                );
            }
            servers.clone()
        }
        (None, false) => Vec::new(),
    };

    // A failed probe is stored as a row like any other, so only a DB error stops
    // the run part-way.
    probe::probe_all(&mut store, &targets, &probe::ProbeOptions::default())?;

    // Report from the stored rows, so one server's line reads the same whether
    // it was just measured or measured last week.
    let manifests = store.mcp_manifests()?;
    let shown: Vec<&probe::ConfiguredServer> = if targets.is_empty() {
        servers.iter().collect()
    } else {
        servers
            .iter()
            .filter(|s| targets.iter().any(|t| t.key == s.key && t.scope() == s.scope()))
            .collect()
    };

    if json {
        let arr: Vec<serde_json::Value> = shown
            .iter()
            .map(|s| server_json(s, &probe::status(&manifests, s)))
            .collect();
        let out = serde_json::json!({
            "configPath": config::claude_json_path().display().to_string(),
            "probed": !targets.is_empty(),
            "servers": arr,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if servers.is_empty() {
        println!(
            "no MCP servers configured in {}.",
            config::claude_json_path().display()
        );
        return Ok(());
    }

    println!(
        "MCP manifests - {} server(s) configured in {}",
        servers.len(),
        config::claude_json_path().display()
    );
    let headers = ["Server", "Scope", "Transport", "Tools", "Measurement"];
    let rows: Vec<Vec<String>> = shown
        .iter()
        .map(|s| {
            let status = probe::status(&manifests, s);
            let tools = match &status {
                probe::MeasurementStatus::Measured(m) => m.tool_count.to_string(),
                _ => "-".to_string(),
            };
            vec![
                s.key.clone(),
                match &s.project {
                    None => "user".to_string(),
                    Some(p) => truncate_path(p, 32),
                },
                s.transport.label().to_string(),
                tools,
                measurement_cell(&status),
            ]
        })
        .collect();
    render_table(&headers, &rows);
    println!();
    if servers
        .iter()
        .any(|s| s.transport == probe::Transport::Remote)
    {
        println!(
            "http/sse servers are not probed in v1 (signing in as you is a different problem); sweep keeps its estimate for those."
        );
    }
    if targets.is_empty() {
        println!(
            "measure one with `piggy probe --server <name>`, or all of them with `piggy probe --all --yes`."
        );
        println!(
            "probing starts the server once and stops it: it is the only way to see the tool schemas it sends every session."
        );
    } else {
        println!("token counts are {} (schema bytes are measured; how the client charges them is not).", probe::TOKENIZER_BYTES_ESTIMATE);
    }
    Ok(())
}

/// One server's measurement state as a table cell: measured and when, stale,
/// never, or failed and why.
fn measurement_cell(status: &probe::MeasurementStatus) -> String {
    match status {
        probe::MeasurementStatus::Deferred => "deferred - http/sse".to_string(),
        probe::MeasurementStatus::Never => "never probed".to_string(),
        probe::MeasurementStatus::Stale(m) => format!(
            "stale - config changed since {}",
            day(&m.measured_at)
        ),
        probe::MeasurementStatus::Measured(m) => format!(
            "~{} tokens, {} bytes ({})",
            commafy(m.schema_tokens.max(0) as u64),
            commafy(m.schema_bytes.max(0) as u64),
            day(&m.measured_at)
        ),
        probe::MeasurementStatus::Failed(m) => format!(
            "failed {} - {}",
            day(&m.measured_at),
            truncate(m.error.as_deref().unwrap_or("no reason recorded"), 72)
        ),
    }
}

fn server_json(s: &probe::ConfiguredServer, status: &probe::MeasurementStatus) -> serde_json::Value {
    let m = status.manifest();
    // Numbers come from the status, not from the row's own `ok` flag. A row can
    // be `ok` and still describe a configuration that no longer exists: a
    // changed command/args/env makes it Stale, which is decided before `ok` is
    // ever consulted. Quoting a previous configuration's tool count under this
    // configuration's hash is how a machine surface tells a lie.
    let measured = match status {
        probe::MeasurementStatus::Measured(row) => Some(row),
        _ => None,
    };
    serde_json::json!({
        "server": s.key,
        "scope": s.scope(),
        "scopeLabel": s.project.as_deref().unwrap_or("user"),
        "transport": s.transport.label(),
        "configHash": s.config_hash(),
        // The config the stored row measured. Equal to `configHash` exactly when
        // the status is `measured` or `failed`, and different when it is
        // `stale`, so the payload says which configuration it describes instead
        // of leaving a consumer to infer it.
        "measuredConfigHash": m.map(|m| m.config_hash.clone()),
        "status": status.tag(),
        "measuredAt": m.map(|m| m.measured_at.clone()),
        "toolCount": measured.map(|m| m.tool_count),
        "schemaBytes": measured.map(|m| m.schema_bytes),
        "schemaTokens": measured.map(|m| m.schema_tokens),
        "tokenizer": m.map(|m| m.tokenizer.clone()),
        // Schema bytes are measured; the token count is only as good as the
        // tokenizer that produced it.
        "estimated": m.map(|m| m.tokenizer == probe::TOKENIZER_BYTES_ESTIMATE),
        "error": m.and_then(|m| m.error.clone()),
    })
}

/// The date half of an RFC3339 stamp, which is all a table cell has room for.
fn day(ts: &str) -> &str {
    ts.split('T').next().unwrap_or(ts)
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

    // File snapshots: the other backup ledger. `settings.json` has one
    // pre-Piggy target and a rolling timestamped history; the advice engine
    // backs up whole files it did not write (a CLAUDE.md is prose, not config,
    // so its restore target has to be the original bytes), one record per edit.
    let state = PiggyState::load()?;
    println!();
    println!(
        "Files Piggy edited ({} restorable, under {}):",
        state.file_snapshots.len(),
        snapshots::files_backup_dir().display()
    );
    if state.file_snapshots.is_empty() {
        println!("  (none yet)");
    }
    for record in state.file_snapshots.iter().rev().take(20) {
        let size = std::fs::metadata(&record.backup)
            .map(|m| m.len())
            .unwrap_or(0);
        println!(
            "  {}  ({} bytes, saved {})",
            record.path,
            commafy(size),
            record.applied_at
        );
    }
    if state.file_snapshots.len() > 20 {
        println!("  … and {} more", state.file_snapshots.len() - 20);
    }
    if !state.scope_moves.is_empty() {
        println!();
        println!(
            "MCP servers Piggy re-scoped ({}, reversible from the app or `piggy restore-defaults`):",
            state.scope_moves.len()
        );
        for record in state.scope_moves.iter().rev() {
            println!(
                "  {} → {}",
                record.server,
                record
                    .before_projects
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// index
// ---------------------------------------------------------------------------

fn cmd_index(full: bool) -> Result<()> {
    let home = config::piggy_home();
    // Every session-log root on this machine: Claude Code projects plus
    // Codex sessions/archived_sessions, whichever exist.
    let roots = piggy_core::default_roots();
    if roots.is_empty() {
        eprintln!(
            "error: no session logs found (looked for {} and {})",
            config::claude_projects_dir().display(),
            config::codex_dir().join("sessions").display()
        );
        std::process::exit(1);
    }
    let pricing = Pricing::load(&home);
    let mut store = Store::open(&home)?;

    let start = Instant::now();
    let rep = piggy_core::run_index_roots(&mut store, &pricing, &roots, full)?;
    let secs = start.elapsed().as_secs_f64();

    // Anchor the pre-install baseline the first time we index, then backfill the
    // `pre_install` tag onto every session that predates Piggy.
    let mut state = PiggyState::load()?;
    if state.ensure_created_at() {
        state.save()?;
    }
    let catalog = Catalog::embedded();
    let tagged = piggy_core::tagging::tag_pre_install_baseline(&mut store, &state, &catalog)?;

    let root_list = roots
        .iter()
        .map(|r| r.dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    println!("indexed {root_list} in {secs:.2}s");
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
        "{} - by {} (cost estimated)",
        period.label(),
        first.to_lowercase()
    );
    if table.is_empty() {
        println!("  (no data - run `piggy index`)");
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
                    "✅ pricing table loaded ({} models); no indexed tokens yet - run `piggy index`",
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

// ---------------------------------------------------------------------------
// insights
// ---------------------------------------------------------------------------

/// Print ledger findings. Arithmetic on observed tokens only - no predictions,
/// and an empty list is a real answer, not a failure.
fn cmd_insights(since: Option<&str>, json: bool) -> Result<()> {
    let home = config::piggy_home();
    let pricing = Pricing::load(&home);
    let store = Store::open(&home)?;
    let found = store.insights(since, &pricing)?;

    if json {
        let out: Vec<_> = found
            .iter()
            .map(|i| {
                serde_json::json!({
                    "id": i.id, "severity": i.severity.as_str(), "title": i.title,
                    "detail": i.detail, "tokens": i.tokens, "action": i.action,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if found.is_empty() {
        println!("Piggy insights - nothing worth flagging.");
        println!();
        println!("Your floor is a reasonable share of spend and no project is churning");
        println!("short sessions. Run `piggy ledger` for the full breakdown.");
        return Ok(());
    }

    println!("Piggy insights - {} findings, loudest first", found.len());
    for i in &found {
        println!();
        println!("[{}] {}", i.severity.as_str().to_uppercase(), i.title);
        println!("  {}", i.detail);
        println!("  → {}", i.action);
    }
    println!();
    println!("Every figure is measured tokens from your session logs. Injection and");
    println!("floor-component figures are bounded by content size; floor and");
    println!("conversation totals are exact.");
    Ok(())
}

// ---------------------------------------------------------------------------
// claudemd
// ---------------------------------------------------------------------------

/// Inventory the CLAUDE.md files Claude Code loads into every session, then
/// print what each costs and what the detectors found in it.
///
/// The scan writes the inventory (sizes and hashes) to the database; file
/// contents are read here and dropped. Every token figure is an estimate
/// (bytes / 3.5 x observed sessions) and is labelled as one.
fn cmd_claudemd(json: bool) -> Result<()> {
    let home = config::piggy_home();
    let mut store = Store::open(&home)?;
    let report = claudemd::scan(&mut store)?;

    if json {
        let files: Vec<serde_json::Value> = report
            .files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.file.path,
                    "scope": f.scope(),
                    "project": f.file.project,
                    "bytes": f.file.bytes,
                    "estTokens": f.file.est_tokens,
                    "sessions30d": f.sessions_30d,
                    "estTokensMonth": f.est_tokens_month,
                    "hash": f.file.hash,
                    "mtimeNs": f.file.mtime_ns,
                    "lastScanned": f.file.last_scanned,
                    "findings": f.findings.iter().map(finding_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        let out = serde_json::json!({
            "files": files,
            "estTokens": report.est_tokens(),
            "estTokensMonth": report.est_tokens_month(),
            "estimated": true,
            "removed": report.removed,
            "warnings": report.warnings,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if report.files.is_empty() {
        println!("Piggy CLAUDE.md - no CLAUDE.md files found.");
        println!();
        println!("Piggy looks in ~/.claude (CLAUDE.md, rules/*.md) and in every project it");
        println!("has indexed a session for. Run `piggy index` if this looks wrong.");
        return Ok(());
    }

    println!(
        "Piggy CLAUDE.md - {} file(s), ~{} tokens loaded, ~{} tokens/month (estimated)",
        report.files.len(),
        commafy(report.est_tokens().max(0) as u64),
        commafy(report.est_tokens_month().max(0) as u64)
    );
    println!();
    let headers = ["File", "Scope", "Bytes", "Est. tokens", "Sessions/30d", "Est. tokens/month"];
    let rows: Vec<Vec<String>> = report
        .files
        .iter()
        .map(|f| {
            vec![
                truncate_path(&f.file.path, 52),
                f.scope().to_string(),
                commafy(f.file.bytes.max(0) as u64),
                format!("~{}", commafy(f.file.est_tokens.max(0) as u64)),
                f.sessions_30d.to_string(),
                format!("~{}", commafy(f.est_tokens_month.max(0) as u64)),
            ]
        })
        .collect();
    render_table(&headers, &rows);

    let n_findings = report.findings().count();
    if n_findings == 0 {
        println!();
        println!("No dead references, duplicate blocks, or oversized files. Nothing to fix.");
    } else {
        println!();
        println!("Findings - {n_findings} across {} file(s)", report.files.iter().filter(|f| !f.findings.is_empty()).count());
        for f in report.files.iter().filter(|f| !f.findings.is_empty()) {
            println!();
            println!("{}", f.file.path);
            for finding in &f.findings {
                println!("  [{}] {}", finding.kind.as_str(), finding.claim);
                println!("    {}", finding.detail);
                println!("    → {}", finding.action);
            }
        }
    }

    for w in &report.warnings {
        println!();
        println!("skipped: {w}");
    }
    if !report.removed.is_empty() {
        println!();
        println!(
            "{} inventory row(s) dropped for files that are gone.",
            report.removed.len()
        );
    }
    println!();
    println!("Token counts are estimates (bytes / 3.5); the monthly figure multiplies them");
    println!("by sessions actually observed in the last 30 days. File contents stay on disk:");
    println!("Piggy stores sizes and hashes only.");
    Ok(())
}

/// One finding as JSON, with its kind-specific evidence flattened alongside the
/// shared fields.
fn finding_json(f: &piggy_core::Finding) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": f.id,
        "kind": f.kind.as_str(),
        "path": f.path,
        "claim": f.claim,
        "detail": f.detail,
        "estTokens": f.est_tokens,
        "estTokensMonth": f.est_tokens_month,
        "estimated": true,
        "action": f.action,
    });
    let obj = v.as_object_mut().expect("finding json is an object");
    match &f.kind {
        piggy_core::FindingKind::DeadRef {
            reference,
            resolved,
            more,
        } => {
            obj.insert("reference".into(), reference.clone().into());
            obj.insert("resolved".into(), resolved.clone().into());
            obj.insert("more".into(), (*more).into());
        }
        piggy_core::FindingKind::DuplicateBlock {
            others,
            label,
            bytes,
        } => {
            obj.insert("others".into(), others.clone().into());
            obj.insert("label".into(), label.clone().into());
            obj.insert("blockBytes".into(), (*bytes).into());
        }
        piggy_core::FindingKind::Oversize { threshold } => {
            obj.insert("threshold".into(), (*threshold).into());
        }
    }
    v
}

// ---------------------------------------------------------------------------
// advise
// ---------------------------------------------------------------------------

/// The candidate list: what Piggy would suggest, and the evidence behind each.
///
/// Listing only, on purpose. Applying an action writes to files this process was
/// not asked to touch, and the spec puts every one of those writes behind the
/// app's per-item consent (a diff for a content edit, a checkbox for the rest).
/// A CLI flag would be a second, quieter door onto the same writes.
///
/// Ranking here is by estimated tokens a month, full stop. The advisor's
/// re-ranking and its drafted CLAUDE.md rewrites are app-only in v1, and this
/// says so rather than pretending the list is the model's.
fn cmd_advise(sessions: Option<usize>, json: bool) -> Result<()> {
    let home = config::piggy_home();
    let mut store = Store::open(&home)?;
    let catalog = Catalog::embedded();
    let pricing = Pricing::load(&home);
    let state = PiggyState::load()?;
    let mut opts = GenerateOptions::new(&catalog, &pricing, &state);
    if let Some(n) = sessions {
        opts.n_sessions = n;
    }
    let candidates = advice::generate(&mut store, &opts)?;

    if json {
        let items: Vec<serde_json::Value> = candidates.iter().map(candidate_json).collect();
        let out = serde_json::json!({
            "candidates": items,
            "estTokensMonth": candidates.iter().map(|c| c.est_tokens_month).sum::<i64>(),
            "sessionsConsidered": opts.n_sessions,
            // Model ranking and CLAUDE.md drafts are app-only in v1.
            "ranking": "estimated-tokens-month",
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if candidates.is_empty() {
        println!("Piggy advice - nothing to suggest.");
        println!();
        println!("Every add-on is in use, the CLAUDE.md stack is clean, and no saver's own");
        println!("measurements argue for a different setting. Run `piggy index` if that looks");
        println!("wrong, or `piggy probe --all --yes` to replace the schema estimates with");
        println!("measurements.");
        return Ok(());
    }

    let total: i64 = candidates.iter().map(|c| c.est_tokens_month).sum();
    println!(
        "Piggy advice - {} suggestion(s), ~{} tokens/month between them",
        candidates.len(),
        commafy(total.max(0) as u64)
    );

    // Grouped by family, and each family in the order its members were ranked.
    // The groups themselves come out in the order they first appear in that
    // ranking, so the biggest single opportunity still leads the page.
    let mut groups: Vec<&str> = Vec::new();
    for candidate in &candidates {
        if !groups.contains(&candidate.kind.group_label()) {
            groups.push(candidate.kind.group_label());
        }
    }
    for group in groups {
        println!();
        println!("{group}");
        for candidate in candidates.iter().filter(|c| c.kind.group_label() == group) {
            println!();
            println!("  {} [{}]", candidate.title, candidate.status);
            println!(
                "    id {} · {} · risk {} · ~{} tokens/month",
                candidate.id,
                candidate.kind.as_str(),
                candidate.risk_tier,
                commafy(candidate.est_tokens_month.max(0) as u64)
            );
            for row in &candidate.evidence {
                // The two literal spaces are the gap: a label longer than the
                // column must not run into its own value.
                println!("      {:<44}  {}  ({})", row.label, row.value, row.basis);
            }
            for prereq in &candidate.prerequisites {
                println!("      needs: {}", prereq.note());
            }
        }
    }

    println!();
    println!("Every figure carries how it was arrived at: `observed` was counted in your");
    println!("session database, `measured manifest` is a real byte count of a server's tool");
    println!("schemas, `measured` is a randomized A/B result, and `estimated` is arithmetic");
    println!("over one of those. Apply any of these in the app - `piggy advise` only lists.");
    Ok(())
}

/// One candidate as JSON. The transform result is deliberately absent: a
/// rewritten CLAUDE.md is the user's prose, and it belongs in a diff view, not
/// in a command's stdout.
fn candidate_json(c: &piggy_core::Candidate) -> serde_json::Value {
    serde_json::json!({
        "id": c.id,
        "kind": c.kind.as_str(),
        "group": c.kind.group_label(),
        "target": c.target,
        "title": c.title,
        "status": c.status,
        "riskTier": c.risk_tier,
        "estTokensMonth": c.est_tokens_month,
        "fingerprint": c.fingerprint,
        "blocked": c.blocked(),
        "prerequisites": c.prerequisites.iter().map(|p| {
            serde_json::json!({ "id": p.as_str(), "note": p.note() })
        }).collect::<Vec<_>>(),
        "evidence": c.evidence.iter().map(|e| {
            serde_json::json!({ "label": e.label, "value": e.value, "basis": e.basis })
        }).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// ledger
// ---------------------------------------------------------------------------

/// Print the context ledger: what is in your context window and what it cost.
///
/// Unlike `report`, this needs no holdout, no rotation, and no waiting. Every
/// token was charged to a cause at parse time, so the table is exact from the
/// first indexed session.
fn cmd_ledger(since: Option<&str>, top_projects: usize, json: bool) -> Result<()> {
    let home = config::piggy_home();
    let pricing = Pricing::load(&home);
    let store = Store::open(&home)?;
    let l = store.ledger(since, &pricing)?;

    if json {
        let out = serde_json::json!({
            "total_tokens": l.total_tokens(),
            "removable_tokens": l.removable_tokens(),
            "overhead": l.overhead(),
            "headroom": l.headroom(),
            "removable_cost_share": l.removable_cost_share(),
            "cost_units": l.cost_units,
            "sources": l.rows.iter().map(|r| serde_json::json!({
                "kind": r.kind,
                "label": r.label(),
                "tokens": r.tokens,
                "charged_on": r.n,
                "share": l.share(r),
                "removable": r.removable(),
                "is_floor": r.is_floor(),
                "estimated": r.estimated(),
            })).collect::<Vec<_>>(),
            "projects": l.projects.iter().map(|p| serde_json::json!({
                "project": p.project,
                "sessions": p.sessions,
                "msgs_per_session": p.msgs_per_session(),
                "floor_tokens": p.floor_tokens,
                "work_tokens": p.work_tokens,
                "overhead": p.overhead(),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let total = l.total_tokens();
    if total == 0 {
        println!("Piggy ledger - nothing indexed yet. Run `piggy index` first.");
        return Ok(());
    }
    let sessions: u64 = l.projects.iter().map(|p| p.sessions).sum();
    match since {
        Some(d) => println!("Piggy ledger - where your context tokens come from ({sessions} sessions since {d})"),
        None => println!("Piggy ledger - where your context tokens come from ({sessions} sessions)"),
    }
    println!();
    println!("{:<52} {:>16} {:>7}", "source", "tokens", "share");
    println!("{}", "-".repeat(78));
    for r in &l.rows {
        println!(
            "{:<52} {:>16} {:>6.1}%",
            truncate(&r.label(), 52),
            commafy(r.tokens),
            l.share(r) * 100.0
        );
    }
    println!("{}", "-".repeat(78));
    println!("{:<52} {:>16}", "TOTAL cache-write tokens", commafy(total));

    let removable = l.removable_tokens();
    println!();
    println!(
        "Removable by configuration: {} tokens ({:.1}%) - injections you control, \
         not the floor or the work.",
        commafy(removable),
        removable as f64 / total as f64 * 100.0
    );
    println!(
        "Session overhead: {:.1}% of cache writes bought session startup rather than work.",
        l.overhead() * 100.0
    );

    println!();
    println!("Per project (heaviest first)");
    println!(
        "{:<44} {:>8} {:>9} {:>14} {:>14} {:>9}",
        "project", "sessions", "msg/sess", "floor", "work", "overhead"
    );
    println!("{}", "-".repeat(102));
    for p in l.projects.iter().take(top_projects) {
        println!(
            "{:<44} {:>8} {:>9.1} {:>14} {:>14} {:>8.1}%",
            truncate_path(&p.project, 44),
            p.sessions,
            p.msgs_per_session(),
            commafy(p.floor_tokens),
            commafy(p.work_tokens),
            p.overhead() * 100.0
        );
    }
    // Never let a display cap read as "that was everything".
    if l.projects.len() > top_projects {
        let rest = &l.projects[top_projects..];
        let tok: u64 = rest.iter().map(|p| p.floor_tokens + p.work_tokens).sum();
        println!(
            "{:<44} {:>8} {:>9} {:>14} {:>14}",
            format!("... and {} more projects", rest.len()),
            rest.iter().map(|p| p.sessions).sum::<u64>(),
            "",
            "",
            commafy(tok)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tasks
// ---------------------------------------------------------------------------

/// Print the task table: per-project spend joined to the outcome signal.
///
/// The ledger says what the tokens bought. This says which project bought it,
/// how many prompts it took, and how often the tools failed - the one column
/// that is an outcome rather than a cost.
fn cmd_tasks(period: Period, top_projects: usize, json: bool) -> Result<()> {
    let home = config::piggy_home();
    let pricing = Pricing::load(&home);
    let store = Store::open(&home)?;
    // `day_cutoff`, NOT `cutoff`: the task table windows on calendar days, and
    // the rolling instant reaches up to a day further back. Paired with it the
    // row would report a total its own `daily` series never accounts for.
    let l = store.ledger(period.day_cutoff().as_deref(), &pricing)?;
    let rows = store.task_table(period)?;
    let by_project: std::collections::HashMap<&str, &piggy_core::TaskRow> =
        rows.iter().map(|t| (t.project.as_str(), t)).collect();
    let total: u64 = l
        .projects
        .iter()
        .map(|p| p.floor_tokens + p.work_tokens)
        .sum();

    if json {
        let out = serde_json::json!({
            "period": format!("{period:?}").to_lowercase(),
            "total_tokens": total,
            // How many days each `daily` series covers. For `all` it is the last
            // 120 days rather than all of history, so that series does NOT sum
            // to `total_tokens`; for every other period it does.
            "daily_days": rows.iter().map(|r| r.daily.len()).max().unwrap_or(0),
            "projects": l.projects.iter().map(|p| {
                let t = by_project.get(p.project.as_str());
                let row_total = p.floor_tokens + p.work_tokens;
                serde_json::json!({
                    "project": p.project,
                    "sessions": p.sessions,
                    "floor_tokens": p.floor_tokens,
                    "work_tokens": p.work_tokens,
                    "total_tokens": row_total,
                    "share": if total == 0 { 0.0 } else { row_total as f64 / total as f64 },
                    // 0 tasks means the logs carry no promptId, NOT a clean run.
                    "tasks": t.map(|t| t.tasks).unwrap_or(0),
                    "turns": t.map(|t| t.turns).unwrap_or(0),
                    "turns_per_task": t.and_then(|t| t.turns_per_task()),
                    "tool_errors": t.map(|t| t.tool_errors).unwrap_or(0),
                    "failed_tasks": t.map(|t| t.failed_tasks).unwrap_or(0),
                    "failure_rate": t.and_then(|t| t.failure_rate()),
                    "daily": t.map(|t| t.daily.clone()).unwrap_or_default(),
                    "delta": t.and_then(|t| t.delta()),
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if l.projects.is_empty() {
        println!("Piggy tasks - nothing indexed for this window yet. Run `piggy index` first.");
        return Ok(());
    }

    println!("Piggy tasks - per-project spend and outcomes ({})", period.label());
    println!();
    println!(
        "{:<40} {:>8} {:>7} {:>13} {:>9} {:>8} {:>9}",
        "project", "sessions", "tasks", "tokens", "share", "turns/tk", "fail rate"
    );
    println!("{}", "-".repeat(100));
    for p in l.projects.iter().take(top_projects) {
        let t = by_project.get(p.project.as_str());
        let row_total = p.floor_tokens + p.work_tokens;
        let tasks = t.map(|t| t.tasks).unwrap_or(0);
        println!(
            "{:<40} {:>8} {:>7} {:>13} {:>8.1}% {:>8} {:>9}",
            truncate_path(&p.project, 40),
            p.sessions,
            // A dash, not a zero: the field did not exist in older logs, and a
            // zero here reads as "nothing failed" when it means "not recorded".
            if tasks == 0 { "-".to_string() } else { tasks.to_string() },
            commafy(row_total),
            if total == 0 { 0.0 } else { row_total as f64 / total as f64 * 100.0 },
            t.and_then(|t| t.turns_per_task())
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "-".into()),
            t.and_then(|t| t.failure_rate())
                .map(|v| format!("{:.1}%", v * 100.0))
                .unwrap_or_else(|| "-".into()),
        );
    }
    if l.projects.len() > top_projects {
        let rest = &l.projects[top_projects..];
        let tok: u64 = rest.iter().map(|p| p.floor_tokens + p.work_tokens).sum();
        println!(
            "{:<40} {:>8} {:>7} {:>13}",
            format!("... and {} more projects", rest.len()),
            rest.iter().map(|p| p.sessions).sum::<u64>(),
            "",
            commafy(tok)
        );
    }
    if rows.iter().all(|r| r.tasks == 0) {
        println!();
        println!("note: no task boundaries recorded in this window - these logs predate the");
        println!("`promptId` field, so per-task columns show `-` rather than zero.");
    }
    Ok(())
}

/// Trim a path to `max` columns from the LEFT, so the leaf directory survives.
/// `truncate` keeps the head, which is right for prose and useless for paths:
/// every project under one tree truncates to the same prefix.
fn truncate_path(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().skip(s.chars().count() - (max - 1)).collect();
    format!("…{keep}")
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
    // A live holdout is necessary but not sufficient for "measured": the full-on
    // side has to be randomized and the holdout has to have been clean. `ceiling`
    // is the authority on that (see `Headline::ceiling`); deriving this banner
    // from `hl.baseline` alone printed "measured savings" over a report whose
    // very next line said "estimated".
    match hl.baseline {
        HeadlineBaseline::Holdout if hl.ceiling == Badge::Measured => {
            println!("Piggy report - measured savings (holdout-based)")
        }
        HeadlineBaseline::Holdout => println!(
            "Piggy report - estimated savings (holdout-based, but not a randomized comparison)"
        ),
        HeadlineBaseline::PreInstall => println!(
            "Piggy report - estimated savings (observational pre-install baseline, no live holdout yet)"
        ),
        HeadlineBaseline::None => println!("Piggy report - not enough data yet"),
    }
    println!();
    let baseline_label = match hl.baseline {
        HeadlineBaseline::Holdout => "holdout",
        HeadlineBaseline::PreInstall => "pre-install history",
        HeadlineBaseline::None => "-",
    };
    if hl.baseline == HeadlineBaseline::None {
        println!("Headline: not enough data yet - need holdout or pre-install sessions.");
    } else {
        println!(
            "Headline (full-on {} vs {} {} sessions):",
            hl.n_full_on, baseline_label, hl.n_baseline
        );
        // A holdout baseline is not enough on its own. The full-on side has to be
        // randomized too, and the holdout has to have actually been all-off. Say
        // which one is missing rather than an unexplained "estimated".
        let per_turn_label = match hl.baseline {
            HeadlineBaseline::Holdout if hl.on_randomized && hl.baseline_clean => {
                "  measured per-turn savings:"
            }
            HeadlineBaseline::Holdout if !hl.on_randomized => {
                "  estimated per-turn savings (savers pinned on by hand, so not randomized):"
            }
            HeadlineBaseline::Holdout => {
                "  estimated per-turn savings (a pinned saver ran through the holdout, \
                 so it was not a no-savers baseline):"
            }
            _ => "  estimated per-turn savings (observational baseline):",
        };
        println!("{per_turn_label}");
        for s in &hl.streams {
            // Cache read is the cheap stream; keep it but de-emphasise below output.
            // Per docs UI copy: show the backing session count on the number line.
            println!("    {:<12}{}", s.stream.label(), stream_result_with_n(s));
        }
        // Same sample bar as the GUI headline (see backend.rs `map_headline`):
        // both sides must clear MIN_GROUP. Printing a multiplier off one session
        // per side would be a number we cannot back, whatever it is labelled.
        match hl.multiplier {
            Some(m) if hl.n_full_on >= attribution::MIN_GROUP
                && hl.n_baseline >= attribution::MIN_GROUP =>
            {
                println!();
                println!("  Your plan lasts {m:.1}× longer  (estimated: price-weighted, cache reads excluded)");
            }
            _ => {}
        }
        if hl.n_carried > 0 {
            println!();
            println!(
                "  includes {} sessions from an earlier saver set: {} measured as no change, \
                 so those sessions are the same treatment (capped at estimated - same \
                 sessions, different weeks)",
                hl.n_carried,
                hl.carried_savers.join(" and "),
            );
        }
        if let Some(w) = hl.waiting() {
            println!();
            println!("  {}", waiting_line(&w));
        }
        if hl.baseline == HeadlineBaseline::PreInstall {
            println!(
                "  note: baseline is pre-install history (observational - no live holdout yet)."
            );
        }
    }

    // ---- Per-saver attribution table -------------------------------------
    println!();
    if attribs.is_empty() {
        println!("No per-saver data yet. Run sessions with savers rotating on and off.");
        return Ok(());
    }
    // One line per saver BEFORE the table: the table is the evidence, and a
    // reader who only wants the finding should not have to derive it from five
    // rows of medians.
    println!("What each saver has shown so far");
    for a in &attribs {
        println!("  {:<22}{}", a.saver_id, a.summary());
        if let Some(c) = a.caveat() {
            println!("  {:<22}  ...but {c}", "");
        }
    }
    println!();
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
    // toward a measured badge - they are only a fallback for an `estimated`
    // figure when randomized OFF data is short.
    for a in &attribs {
        if let Some(n) = a.off_by_source.get("pre_install") {
            if *n > 0 {
                println!(
                    "  {}: {} pre-install (observational) OFF sessions - never used for a measured badge.",
                    a.saver_id, n
                );
            }
        }
    }
    Ok(())
}

/// One line saying what the headline is still waiting for and how long it has
/// left. "Measuring" with no end in sight is indistinguishable from broken, and
/// the ON arm in particular restarts silently every time the saver set changes -
/// so the date it restarted is the part that turns a stuck-looking screen back
/// into a running experiment.
fn waiting_line(w: &attribution::Waiting) -> String {
    let what = match w.arm {
        attribution::WaitingArm::On => "sessions on your current saver set",
        attribution::WaitingArm::Baseline => "all-off holdout sessions",
    };
    let mut s = format!("still measuring: {} of {} {what}", w.have, w.need);
    if let Some(since) = &w.since {
        // Date only. The hour a saver set came together is noise; the day is what
        // the user can match against "oh right, I installed something on Tuesday".
        let day = since.split('T').next().unwrap_or(since);
        s.push_str(&match w.arm {
            attribution::WaitingArm::On => format!(" · counting since your saver set last changed on {day}"),
            attribution::WaitingArm::Baseline => format!(" · counting since {day}"),
        });
    }
    match w.days_left {
        Some(d) if d < 1.5 => s.push_str(" · about a day to go at your recent pace"),
        Some(d) => s.push_str(&format!(" · about {:.0} days to go at your recent pace", d.ceil())),
        None => s.push_str(" · too early to estimate how long"),
    }
    s
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
        // A holdout baseline is not enough to call this randomized. Deriving
        // `observational` from the baseline kind alone told a machine consumer
        // `baseline: "holdout", observational: false` for a headline the core had
        // already capped at estimated, while the per-stream badges (which do flow
        // from `ceiling`) said otherwise in the same payload.
        "observational": hl.ceiling != Badge::Measured,
        "nFullOn": hl.n_full_on,
        "nBaseline": hl.n_baseline,
        // Why it is observational, when it is: the full-on side was pinned on by
        // hand, and/or the holdout had a pinned saver running through it.
        "onRandomized": hl.on_randomized,
        // How much of `nFullOn` is measured-eligible. `onRandomized: false` with a
        // non-zero count here is rotation running and still short, which is a wait;
        // with zero it is nothing rotating at all, which is not. Same flag, opposite
        // advice, so a consumer needs the count to tell them apart.
        "nFullOnRandomized": hl.n_full_on_randomized,
        "baselineClean": hl.baseline_clean,
        "multiplier": hl.multiplier,
        "multiplierEstimated": true,
        // Why `multiplier` is null, when it is. "Enough sessions but the estimate
        // was withheld as implausible" is not the same story as "still gathering",
        // and neither the multiplier nor the session counts can say which.
        "multiplierState": match hl.multiplier_state {
            attribution::MultiplierState::Shown => "shown",
            attribution::MultiplierState::NoData => "no_data",
            attribution::MultiplierState::WithheldCostMore => "withheld_cost_more",
        },
        // Sessions needed on EACH arm before a measured claim, so a consumer can
        // draw honest progress against the real bar instead of hard-coding 10.
        "minGroup": piggy_core::attribution::MIN_GROUP,
        "streams": hl.streams.iter().map(stream_json).collect::<Vec<_>>(),
        // The denominator, measured as its own arm. A negative delta means the
        // savers bought cheaper turns by needing more of them.
        "turns": stream_json(&hl.turns),
        // What the experiment is still waiting for. Null once both arms are full,
        // so a consumer can tell "warming up" from "held up by something else"
        // without re-deriving the sample gate.
        // Sessions folded in from an earlier saver set that differed only by a
        // saver measured as null, and the savers that made that legal.
        "nCarried": hl.n_carried,
        "carriedSavers": hl.carried_savers,
        "waiting": hl.waiting().map(|w| serde_json::json!({
            "arm": match w.arm {
                attribution::WaitingArm::On => "on",
                attribution::WaitingArm::Baseline => "baseline",
            },
            "have": w.have,
            "need": w.need,
            "since": w.since,
            "daysLeft": w.days_left,
        })),
    })
}

fn saver_json(a: &piggy_core::SaverAttribution) -> serde_json::Value {
    serde_json::json!({
        "saver": a.saver_id,
        "nOn": a.n_on,
        "nOff": a.n_off,
        "offBySource": a.off_by_source,
        // The one-line learning across every arm, so a consumer of the JSON
        // does not have to re-derive it from the streams.
        "summary": a.summary(),
        // What the summary does not cover: a thin arm, or an uncomparable turn
        // count under a per-turn saving.
        "caveat": a.caveat(),
        "streams": a.streams.iter().map(stream_json).collect::<Vec<_>>(),
        // The denominator the streams divide by. A saver can look green on all
        // four and still cost more by needing extra turns to finish the job.
        "turns": stream_json(&a.turns),
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
        // What the stream means when there is no figure: waiting, too small to
        // compare, flat, or too noisy. `reading` is the key to branch on;
        // `note` is the sentence to print.
        "note": s.note(),
        "reading": s.reading().key(),
        "ci": s.ci.map(|(lo, hi)| [lo * 100.0, hi * 100.0]),
        "nOn": s.n_on,
        "nOff": s.n_off,
        // The two medians the delta is a ratio of (tokens per assistant turn).
        // A percentage alone is a number the reader has to take on faith; with
        // both sides a consumer can show the comparison it came from, and an arm
        // with no sessions is visibly empty rather than silently absent. Always
        // present, including while `deltaPct` is null.
        "medianOn": s.median_on,
        "medianOff": s.median_off,
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
        println!("  (nothing found - try `piggy discover --refresh`)");
        return Ok(());
    }
    let headers = ["Stars", "Repo", "What it is"];
    let rows: Vec<Vec<String>> = cache
        .repos
        .iter()
        .map(|r| {
            let what = if r.listed_only {
                "listed only - not installable".to_string()
            } else {
                r.description.clone().unwrap_or_default()
            };
            vec![
                if r.listed_only {
                    "-".to_string()
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
                "  {} - {}",
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
    let pricing = Pricing::load(&home);

    // Anchor the pre-install baseline so live sessions are attributed correctly.
    let mut state = PiggyState::load()?;
    if state.ensure_created_at() {
        state.save()?;
    }

    // Watch every session-log root that exists (Claude Code + Codex). A fresh
    // machine may have neither - fall back to creating/watching the Claude
    // projects dir, the historical behaviour.
    let roots = piggy_core::default_roots();
    let (mut watcher, label) = if roots.is_empty() {
        let projects = config::claude_projects_dir();
        let label = projects.display().to_string();
        (SessionWatcher::new(projects, &home)?, label)
    } else {
        let label = roots
            .iter()
            .map(|r| r.dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        (
            SessionWatcher::with_roots(roots, &home, piggy_core::WatchBackend::Native)?,
            label,
        )
    };
    println!("watching {label} (Ctrl-C to stop)…");
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use piggy_core::store::SCOPE_USER;
    use piggy_core::McpManifest;
    use serde_json::json;

    /// One stored row for `atlas`, successful, measured against `config_hash`.
    fn row(config_hash: &str) -> McpManifest {
        McpManifest {
            server_key: "atlas".to_string(),
            scope: SCOPE_USER.to_string(),
            config_hash: config_hash.to_string(),
            tool_count: 24,
            schema_bytes: 4_200,
            schema_tokens: 1_200,
            tokenizer: probe::TOKENIZER_BYTES_ESTIMATE.to_string(),
            measured_at: "2026-08-01T10:00:00Z".to_string(),
            ok: true,
            error: None,
        }
    }

    /// `ok` is the row's own success flag, not a statement about freshness. A row
    /// that measured a previous command is still `ok = true`, and `status`
    /// returns `Stale` for it before `ok` is ever consulted - so gating the
    /// numbers on `ok` published a configuration the user has since changed,
    /// under the *current* configuration's hash.
    #[test]
    fn a_stale_row_publishes_no_numbers_and_says_which_config_it_measured() {
        let config = json!({
            "mcpServers": { "atlas": { "command": "node", "args": ["atlas.mjs"] } }
        });
        let server = probe::servers_from_root(&config).remove(0);

        let stale = vec![row("the-config-before-this-one")];
        let v = server_json(&server, &probe::status(&stale, &server));
        assert_eq!(v["status"], "stale");
        for field in ["toolCount", "schemaBytes", "schemaTokens"] {
            assert!(v[field].is_null(), "{field} came from a stale row: {v}");
        }
        // The row's own metadata still travels, and now says which configuration
        // it belongs to.
        assert_eq!(v["measuredAt"], "2026-08-01T10:00:00Z");
        assert_eq!(v["tokenizer"], probe::TOKENIZER_BYTES_ESTIMATE);
        assert_eq!(v["measuredConfigHash"], "the-config-before-this-one");
        assert_ne!(v["measuredConfigHash"], v["configHash"]);

        // The same row against the config it actually measured does publish.
        let fresh = vec![row(&server.config_hash())];
        let v = server_json(&server, &probe::status(&fresh, &server));
        assert_eq!(v["status"], "measured");
        assert_eq!(v["toolCount"], 24);
        assert_eq!(v["schemaBytes"], 4_200);
        assert_eq!(v["schemaTokens"], 1_200);
        assert_eq!(v["measuredConfigHash"], v["configHash"]);
    }

    /// A failed probe of the *current* config is a row with numbers of zero and
    /// a reason. It is not a measurement, so it publishes no numbers either.
    #[test]
    fn a_failed_row_publishes_no_numbers_but_keeps_its_reason() {
        let config = json!({
            "mcpServers": { "atlas": { "command": "node", "args": ["atlas.mjs"] } }
        });
        let server = probe::servers_from_root(&config).remove(0);
        let mut failed = row(&server.config_hash());
        failed.ok = false;
        failed.error = Some("the server stopped before answering".to_string());

        let v = server_json(&server, &probe::status(&[failed], &server));
        assert_eq!(v["status"], "failed");
        for field in ["toolCount", "schemaBytes", "schemaTokens"] {
            assert!(v[field].is_null(), "{field} came from a failed row: {v}");
        }
        assert_eq!(v["error"], "the server stopped before answering");
        assert_eq!(v["measuredConfigHash"], v["configHash"]);
    }
}
