//! `piggy` — measure Claude Code token usage from session logs.
//!
//! Subcommands:
//!   * `index`  — scan `~/.claude/projects/**/*.jsonl` into the local DB.
//!   * `stats`  — human tables (or `--json`) of token usage and estimated cost.
//!   * `doctor` — environment / data-health checks.
//!   * `parse`  — dump one file's parsed aggregate as JSON (the jq cross-check).

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use piggy_core::{config, parse_file, run_index, stats::Totals, Period, Pricing, Store};

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
    }
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

    println!("indexed {} in {:.2}s", projects.display(), secs);
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
