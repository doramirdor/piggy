//! The bounded fact sheet handed to the model.
//!
//! Everything the advisor is allowed to say has to be *in here first*. That is
//! not a stylistic preference: [`super::guard`] builds its allow-list from this
//! payload, so a figure that is not a fact is mechanically unable to reach the
//! UI. The prompt asks the model to behave; this is what makes it not matter
//! whether it does.
//!
//! Two consequences for how facts are written:
//!
//! * **Pre-compute every aggregate we want quoted.** "Your 12 unused skills cost
//!   14,200 tokens per session" is a sum, and a sum is arithmetic. If the model
//!   should be able to say it, [`Facts::build`] must compute it.
//! * **Pre-round every percentage.** The ledger knows the overhead to full
//!   precision; the model will write "35%". Emitting the rounded form here is
//!   what makes that sentence a restatement rather than a new claim.
//!
//! The payload is capped at a few dozen rows because context is the budget we
//! are spending. A 4k window has to hold the fact sheet, the instructions, and
//! the answer, and a fact sheet that fills it is a fact sheet the model reads
//! badly.

use serde_json::{json, Value};

use crate::insights::Insight;
use crate::ledger::Ledger;
use crate::sweep::SweepReport;

/// Caps. Past these the model reads worse, not better, and the tail rows are
/// the ones no user would act on anyway.
///
/// These are sized against a **real** tree, not a fixture. On a machine with
/// 7,783 sessions the first draft of this sheet came to ~3,600 tokens, which
/// leaves nothing of a 4,096-token window for the instructions and the answer.
/// [`crate::advisor::facts`] is the budget, so the caps are the budget:
/// `real_data_fact_sheet_fits_the_context_window` is the test that holds them
/// honest, and it reads the developer's own database precisely because a
/// two-row fixture cannot.
const MAX_LEDGER_ROWS: usize = 8;
const MAX_PROJECTS: usize = 6;
const MAX_SWEEP: usize = 10;
const MAX_INSIGHTS: usize = 6;
/// Insight prose is already written for humans; the model needs it for grounding,
/// not verbatim reuse.
const MAX_DETAIL: usize = 150;

/// The fact sheet, plus the ids the model is permitted to annotate.
#[derive(Debug, Clone)]
pub struct Facts {
    /// The payload serialized into the prompt.
    pub value: Value,
    /// Ids from [`crate::insights`]. An annotation naming anything else is
    /// dropped: the model annotates findings, it does not create them.
    pub insight_ids: Vec<String>,
}

impl Facts {
    /// The exact text that goes into the prompt.
    ///
    /// Compact, not pretty-printed: indentation is roughly a third of the byte
    /// count on a sheet this nested, and the model reads compact JSON no worse.
    /// Every caller goes through here so the size test cannot measure one string
    /// while the prompt sends another.
    pub fn prompt_json(&self) -> String {
        serde_json::to_string(&self.value).unwrap_or_else(|_| "{}".to_string())
    }

    /// Assemble the fact sheet.
    ///
    /// `sweep` is optional because the advisor is still useful without it, but
    /// it is what upgrades advice from "trim your hooks" to naming the skill
    /// that has not been invoked in two hundred sessions.
    pub fn build(ledger: &Ledger, found: &[Insight], sweep: Option<&SweepReport>) -> Self {
        let total = ledger.total_tokens();

        let rows: Vec<Value> = ledger
            .rows
            .iter()
            .take(MAX_LEDGER_ROWS)
            .map(|r| {
                json!({
                    "name": r.label(),
                    "tokens": r.tokens,
                    "pct": pct(r.tokens as f64, total as f64),
                    // The UI must not present an estimate and a measured write
                    // as equally precise, and neither may the model.
                    "estimated": r.estimated(),
                    "removable": r.removable(),
                })
            })
            .collect();

        let projects: Vec<Value> = ledger
            .projects
            .iter()
            .take(MAX_PROJECTS)
            .map(|p| {
                json!({
                    "project": basename(&p.project),
                    "sessions": p.sessions,
                    "msgs_per_session": round1(p.msgs_per_session()),
                    "startup_tokens": p.floor_tokens,
                    "work_tokens": p.work_tokens,
                    "startup_pct": pct(p.floor_tokens as f64, (p.floor_tokens + p.work_tokens) as f64),
                })
            })
            .collect();

        let insights: Vec<Value> = found
            .iter()
            .take(MAX_INSIGHTS)
            .map(|i| {
                json!({
                    "id": i.id,
                    "severity": i.severity.as_str(),
                    "title": i.title,
                    "detail": clip(&i.detail, MAX_DETAIL),
                    "tokens": i.tokens,
                    "current_action": i.action,
                })
            })
            .collect();

        let mut value = json!({
            "totals": {
                "cache_write_tokens": total,
                "startup_pct": pct_of(ledger.overhead()),
                "removable_pct_of_cost": pct_of(ledger.removable_cost_share()),
                "sessions": ledger.projects.iter().map(|p| p.sessions).sum::<u64>(),
            },
            "context_ledger": rows,
            "projects": projects,
            "findings": insights,
        });

        if let Some(s) = sweep {
            value["configuration"] = sweep_facts(s);
        }
        if let Some(h) = ledger.headroom() {
            // Available headroom, never achieved savings. The distinction is the
            // one thing the advisor must not blur, so it is labelled in the key.
            value["totals"]["available_headroom_multiplier"] = json!(round1(h));
        }

        Facts {
            value,
            insight_ids: found.iter().take(MAX_INSIGHTS).map(|i| i.id.clone()).collect(),
        }
    }
}

/// Config items and the aggregates we want the model to be able to quote.
fn sweep_facts(s: &SweepReport) -> Value {
    let unused: Vec<_> = s.recommended().collect();
    let items: Vec<Value> = s
        .items
        .iter()
        .take(MAX_SWEEP)
        .map(|i| {
            json!({
                "kind": i.kind,
                "name": i.id,
                "used": i.used,
                // Without this flag the model would compare a lifetime counter
                // against a windowed one and call the difference a trend.
                "usage_is_windowed": i.used_windowed,
                "est_tokens_per_session": i.est_tokens,
                "unused": i.recommend_disable,
            })
        })
        .collect();

    json!({
        "items": items,
        // Pre-computed: these are the sums the advice wants to quote, and the
        // model is not permitted to add.
        "unused_count": unused.len(),
        "unused_tokens_per_session": s.est_recoverable_tokens(),
        "note": "est_tokens_per_session is an estimate, not a measured write",
    })
}

/// Last path segment, so the fact sheet carries `Stacked` rather than a full
/// home directory path. Shorter, and no reason to spend context on the prefix.
fn basename(p: &str) -> &str {
    p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}

/// Percentage, rounded to whole numbers, which is the precision the findings
/// prose already uses. Rounding here is what lets the guard accept the model
/// writing "35%".
fn pct(n: f64, d: f64) -> u64 {
    if d <= 0.0 {
        return 0;
    }
    (n / d * 100.0).round() as u64
}

fn pct_of(share: f64) -> u64 {
    (share * 100.0).round() as u64
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
