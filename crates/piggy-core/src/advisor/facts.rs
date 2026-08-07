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

use std::collections::BTreeMap;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::advice::{basis, Candidate, Params, Prerequisite};
use crate::attribution::{
    Badge, Headline, HeadlineBaseline, MultiplierState, SaverAttribution, Stream, StreamStat,
};
use crate::claudemd::{ClaudemdReport, ProjectMcpServers};
use crate::insights::Insight;
use crate::ledger::{Ledger, ProjectRow};
use crate::registry::Entry;
use crate::store::McpManifest;
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
/// Savers per sheet. The catalog is small, and only the ones with both arms of
/// a comparison reach the sheet at all.
const MAX_SAVERS: usize = 8;
/// Insight prose is already written for humans; the model needs it for grounding,
/// not verbatim reuse.
const MAX_DETAIL: usize = 150;

/// Caps for the advice sheet ([`Facts::advice`]) alone.
///
/// Bigger than the M4 caps above because this sheet runs in a 16,384-token
/// window rather than a 4,096-token one, and because it has to carry the whole
/// structured picture: the candidate list is the thing being ranked, so a
/// candidate the cap drops is a candidate the model cannot recommend.
///
/// `the_advice_sheet_fits_the_16k_window` is what holds these honest, and it is
/// not slack: a fixture at every cap at once comes to ~10,700 tokens against an
/// 11,000 bar. Raising one of these means running it.
const ADVICE_MAX_LEDGER_ROWS: usize = 10;
const ADVICE_MAX_PROJECTS: usize = 10;
const ADVICE_MAX_INSIGHTS: usize = 8;
const ADVICE_MAX_SWEEP: usize = 12;
const ADVICE_MAX_SAVERS: usize = 10;
const ADVICE_MAX_MANIFESTS: usize = 12;
const ADVICE_MAX_CLAUDEMD: usize = 10;
const ADVICE_MAX_PROJECT_MCP: usize = 10;
const ADVICE_MAX_SERVERS_PER_PROJECT: usize = 6;
const ADVICE_MAX_CANDIDATES: usize = 16;
const ADVICE_MAX_EVIDENCE_ROWS: usize = 4;
const ADVICE_MAX_SERVER_USAGE: usize = 10;
/// Projects listed under one server. The point of the row is which projects
/// call it and how lopsided that is, and past a handful the tail says neither.
const ADVICE_MAX_CALLERS: usize = 4;

/// Domain separator for [`Facts::hash`], so a facts hash can never collide with
/// another sha256 use in the codebase. The `v2` is the payload's version: a
/// change to the shape has to invalidate every cached overlay.
const FACTS_DOMAIN: &[u8] = b"piggy/facts/v2\n";

/// Hex characters of [`Facts::hash`]. 64 bits, readable in a log line, and a
/// cache key rather than a security boundary - the same reasoning as
/// [`crate::advice`]'s candidate ids.
const HASH_HEX_LEN: usize = 16;

/// The recent half of the floor trend, in days.
///
/// The two windows are adjacent and disjoint, and together they cover the same
/// 30 days the rest of the sheet describes. Nesting a 7-day window inside a
/// 30-day one damps exactly the recent change a trend exists to show.
pub const TREND_RECENT_DAYS: i64 = 7;
/// The window immediately before [`TREND_RECENT_DAYS`], ending where it starts.
pub const TREND_PRIOR_DAYS: i64 = 23;

/// The fact sheet, plus the ids the model is permitted to name.
#[derive(Debug, Clone)]
pub struct Facts {
    /// The payload serialized into the prompt.
    pub value: Value,
    /// Ids from [`crate::insights`]. An annotation naming anything else is
    /// dropped: the model annotates findings, it does not create them. Empty on
    /// the advice sheet.
    pub insight_ids: Vec<String>,
    /// Candidate ids from [`crate::advice`]. A pick naming anything else is
    /// dropped: the model ranks candidates, it does not create them. Empty on
    /// the two M4 sheets.
    pub candidate_ids: Vec<String>,
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
            candidate_ids: Vec::new(),
        }
    }

    /// A stable content hash of the exact bytes the prompt sends.
    ///
    /// Over [`Self::prompt_json`] rather than over the inputs it was built from,
    /// so any change that reaches the model changes the key and any change that
    /// does not, does not. That is what makes "same facts, same advice"
    /// (docs/m5-spec.md) a property of the cache rather than a hope.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(FACTS_DOMAIN);
        h.update(self.prompt_json().as_bytes());
        let hex = format!("{:x}", h.finalize());
        hex[..HASH_HEX_LEN].to_string()
    }

    /// A fact sheet about the **savers themselves**, for the per-saver advice
    /// pass ([`super::llama::Advisor::explain_savers`]).
    ///
    /// Same contract as [`Facts::build`], and the same reason for it: every
    /// figure here is already computed and already rounded, so anything the
    /// model writes is a restatement. What it adds is the sentence
    /// [`crate::attribution::SaverAttribution::summary`] cannot: what the
    /// finding means for *this* user's setup, and what to do about it.
    ///
    /// Savers with no comparison yet are left out. "Piggy has not measured this
    /// one" is already on screen, and a fact sheet that carries eleven savers of
    /// which eight say nothing invites the model to write about the eight.
    pub fn savers(rows: &[(&Entry, &SaverAttribution)]) -> Self {
        let mut ids = Vec::new();
        let items: Vec<Value> = rows
            .iter()
            .filter(|(_, a)| a.n_on > 0 && a.n_off > 0)
            // A saver whose every arm is still short has no result to advise
            // on, and the row already says "still measuring". Left on the
            // sheet, a 4B dutifully wrote "Barber needs more sessions to
            // confirm its impact" - true, already on screen, and printed as
            // though it were advice.
            .filter(|(_, a)| {
                !a.arms()
                    .all(|s| matches!(s.reading(), crate::attribution::Reading::Waiting { .. }))
            })
            .take(MAX_SAVERS)
            .map(|(e, a)| {
                let id = format!("{SAVER_PREFIX}{}", e.id);
                ids.push(id.clone());
                json!({
                    "id": id,
                    "saver": e.name,
                    "does": clip(&e.description, MAX_DETAIL),
                    "sessions_with_it_on": a.n_on,
                    "sessions_with_it_off": a.n_off,
                    // The deterministic finding. The model's job is to make this
                    // actionable, never to restate or re-derive it.
                    "finding": a.summary(),
                    // What the finding does not cover, when there is such a
                    // thing. This is the model's actual subject matter: it is
                    // the one part of the measurement not already printed on
                    // the row, so a line about it cannot be a restatement.
                    "caveat": a.caveat(),
                    "streams": a
                        .arms()
                        .map(|s| match s.shown_pct() {
                            // A settled stream: the percentage, and the two
                            // medians it is the ratio of.
                            Some(p) => settled_stream(s, p),
                            // An unsettled stream carries its sentence and NO
                            // figures. Given the medians, a live 4B wrote "no
                            // impact on output" about a stream whose own result
                            // said it was too noisy to call: a conclusion built
                            // out of two real numbers, which is the one thing
                            // `guard` cannot catch. Withholding the numbers is
                            // what makes that sentence unwritable rather than
                            // merely discouraged.
                            None => json!({
                                "stream": s.stream.label(),
                                "result": s.note(),
                            }),
                        })
                        .collect::<Vec<_>>(),
                })
            })
            .collect();

        Facts {
            candidate_ids: Vec::new(),
            value: json!({
                "savers": items,
                "note": "Every figure is a median in its own stream's unit: \
                         per_turn_* is per assistant turn, per_session_* is per \
                         session. Never convert between them. reduced_by_pct is \
                         what the saver took off a stream and increased_by_pct \
                         is what it added; a stream with neither was not \
                         measurable: its `result` says why, and it must never be \
                         described as unchanged.",
            }),
            insight_ids: ids,
        }
    }

    /// The advice sheet: the whole structured picture, plus the candidate list
    /// the model is asked to rank.
    ///
    /// Every block is optional and is omitted entirely when its source is
    /// absent, because an empty array on a sheet is an invitation to write about
    /// nothing. The key names are load-bearing in the same way
    /// [`settled_stream`]'s are: the direction and the unit live in the key, so
    /// a magnitude cannot be read with the wrong sign or the wrong denominator.
    pub fn advice(input: &AdviceInput) -> Self {
        let ledger = input.ledger;
        let total = ledger.total_tokens();

        let rows: Vec<Value> = ledger
            .rows
            .iter()
            .take(ADVICE_MAX_LEDGER_ROWS)
            .map(|r| {
                json!({
                    "name": r.label(),
                    "tokens": r.tokens,
                    "pct": pct(r.tokens as f64, total as f64),
                    "estimated": r.estimated(),
                    "removable": r.removable(),
                })
            })
            .collect();

        let projects: Vec<Value> = ledger
            .projects
            .iter()
            .take(ADVICE_MAX_PROJECTS)
            .map(|p| project_facts(p, input.trend.as_ref()))
            .collect();

        let findings: Vec<Value> = input
            .insights
            .iter()
            .take(ADVICE_MAX_INSIGHTS)
            .map(|i| {
                json!({
                    "id": i.id,
                    "severity": i.severity.as_str(),
                    "title": scrub_home(&i.title),
                    "detail": scrub_home(&clip(&i.detail, MAX_DETAIL)),
                    "tokens": i.tokens,
                    "current_action": scrub_home(&i.action),
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
            "findings": findings,
        });

        if let Some(h) = ledger.headroom() {
            value["totals"]["available_headroom_multiplier"] = json!(round1(h));
        }
        if let Some(s) = input.sweep {
            value["configuration"] = sweep_facts_capped(s, ADVICE_MAX_SWEEP);
        }
        if let Some(v) = manifest_facts(input.manifests) {
            value["measured_manifests"] = v;
        }
        if let Some(v) = server_usage_facts(input.server_usage) {
            value["server_usage"] = v;
        }
        if let Some(v) = project_mcp_facts(input.project_mcp) {
            value["project_mcp"] = v;
        }
        if let Some(v) = claudemd_facts(input.claudemd) {
            value["claudemd"] = v;
        }
        if let Some(v) = advice_saver_facts(input.savers) {
            value["savers"] = v;
        }
        if let Some(h) = input.headline {
            value["holdout"] = holdout_facts(h);
        }

        let shown: Vec<&Candidate> = input.candidates.iter().take(ADVICE_MAX_CANDIDATES).collect();
        if !shown.is_empty() {
            value["candidates"] = Value::Array(shown.iter().map(|c| candidate_facts(c)).collect());
            value["candidate_totals"] = json!({
                "listed": shown.len(),
                // Over EVERY candidate, not just the listed ones, and split the
                // way `advice::total_savings` splits them: a burden summed into
                // a savings total is a ~10x overstatement in the shape a reader
                // is most likely to believe.
                "savings_tokens_month": crate::advice::total_savings(input.candidates),
                "burden_tokens_month": crate::advice::total_burden(input.candidates),
            });
        }

        value["note"] = json!(ADVICE_NOTE);

        Facts {
            value,
            insight_ids: Vec::new(),
            candidate_ids: shown.iter().map(|c| c.id.clone()).collect(),
        }
    }
}

/// The closing note on the advice sheet.
///
/// Held as a constant so the size test measures the string the prompt sends. It
/// says the three things the payload's shape cannot: that the arithmetic is
/// done, which direction each signed key runs in, and that a burden is not a
/// saving.
const ADVICE_NOTE: &str = "Every figure here is already computed and already \
rounded. Copy one verbatim or leave it out. est_tokens_month is what applying a \
candidate saves in a month, EXCEPT where est_is says burden: there it is what \
the target costs today, which is the ceiling on what a rewrite could give back \
and never a promised saving. reduced_by_pct is what a saver took OFF a stream \
and increased_by_pct is what it ADDED; a stream with neither was not \
measurable. floor_direction compares the last 7 days against the 23 before \
them, so up means the startup cost of a session grew.";

/// Everything [`Facts::advice`] needs, assembled by the caller so `facts.rs`
/// stays a pure function of already-computed data and links neither the store
/// nor llama.
///
/// Nothing here may carry an MCP server's `env`. That is where people keep API
/// tokens, it is the reason [`crate::advice::Params::ServerScope`] stopped
/// carrying a config, and a prompt payload is a worse place for a token than a
/// database row: it would be handed to a model and, on a bad day, written back
/// out in prose. So the manifest block carries counts and never configuration.
pub struct AdviceInput<'a> {
    /// The 30-day ledger, the window every other figure on the sheet describes.
    pub ledger: &'a Ledger,
    /// The two adjacent windows behind the per-project floor trend. `None`
    /// suppresses the trend rather than inventing a direction.
    pub trend: Option<FloorTrend<'a>>,
    pub insights: &'a [Insight],
    pub sweep: Option<&'a SweepReport>,
    pub manifests: &'a [McpManifest],
    /// server -> project -> calls, exactly [`crate::advice::Inputs::server_usage`].
    pub server_usage: &'a BTreeMap<String, BTreeMap<String, u64>>,
    pub claudemd: &'a ClaudemdReport,
    pub project_mcp: &'a ProjectMcpServers,
    /// `(entry, enabled, attribution)` in catalog order, which is the order the
    /// user sees on the Savers screen and therefore the order the cap should
    /// take.
    pub savers: &'a [(&'a Entry, bool, &'a SaverAttribution)],
    pub headline: Option<&'a Headline>,
    /// The deterministic candidate list, in the order
    /// [`crate::advice::generate`] produced it.
    pub candidates: &'a [Candidate],
}

/// The two windows behind the floor trend, adjacent and disjoint.
pub struct FloorTrend<'a> {
    /// The last [`TREND_RECENT_DAYS`] days.
    pub recent: &'a Ledger,
    /// The [`TREND_PRIOR_DAYS`] days before that.
    pub prior: &'a Ledger,
}

/// Ids in the saver sheet are prefixed so an annotation can never be attached to
/// a ledger finding by a model that saw the other sheet, and so the UI can tell
/// the two apart in one payload.
pub const SAVER_PREFIX: &str = "saver:";

/// One settled arm: the percentage under a key that says which way it went,
/// plus the two medians it is the ratio of, under keys that say what they are
/// medians *of*.
///
/// The direction lives in the **key** and the figure is always a positive
/// magnitude. [`StreamStat::shown_pct`] is signed, and a negative one under
/// `reduced_by_pct` is a saver that made the stream worse, filed under a name
/// that says it improved: the model reads the name, not the sign, and phrases
/// the regression as a win. Zero goes under `increased_by_pct` for one reason
/// only: that is the side [`SaverAttribution::summary`] puts it on ("0% more"),
/// and the sheet may not disagree with the sentence printed beside it about
/// where a boundary falls.
///
/// The magnitude matters as much as the name. [`super::guard`] admits a figure
/// by its digits and its scan does not consume a leading minus, so a sheet
/// carrying `-12` never admitted `12` and the guard dropped the one honest
/// sentence about it ("12% more output") as a fabricated number.
///
/// The **unit** is the third thing the key carries, and it is not the same for
/// every arm. [`SaverAttribution::arms`] chains the turns comparison onto the
/// token streams, and its medians are turns per *session*, not per turn. One
/// pair of key names for both put a per-session figure on a sheet whose rules
/// tell the model to copy figures verbatim, labelled per turn: a mislabel the
/// guard cannot catch, because the number itself is real.
fn settled_stream(s: &StreamStat, pct: f64) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("stream".into(), json!(s.stream.label()));
    let key = if pct <= 0.0 {
        "increased_by_pct"
    } else {
        "reduced_by_pct"
    };
    o.insert(key.into(), json!(pct.abs().round()));
    // Rounding goes with the unit. Token streams sit in the thousands, where a
    // whole number is the honest precision; turns per session is single digits
    // to low tens, where it is not, and 4.0 against 2.6 rounded to "4" and "3"
    // hands the model two figures whose ratio contradicts the percentage sitting
    // beside them.
    let (off, on, median_off, median_on) = match s.stream {
        Stream::Turns => (
            "per_session_with_it_off",
            "per_session_with_it_on",
            round1(s.median_off),
            round1(s.median_on),
        ),
        _ => (
            "per_turn_with_it_off",
            "per_turn_with_it_on",
            s.median_off.round(),
            s.median_on.round(),
        ),
    };
    o.insert(off.into(), json!(median_off));
    o.insert(on.into(), json!(median_on));
    Value::Object(o)
}

/// One project row, with the floor trend attached when both windows saw it.
fn project_facts(p: &ProjectRow, trend: Option<&FloorTrend>) -> Value {
    let mut o = json!({
        "project": basename(&p.project),
        "sessions": p.sessions,
        "msgs_per_session": round1(p.msgs_per_session()),
        "startup_tokens": p.floor_tokens,
        "work_tokens": p.work_tokens,
        "startup_pct": pct(p.floor_tokens as f64, (p.floor_tokens + p.work_tokens) as f64),
    });
    let Some(t) = trend else {
        return o;
    };
    let (Some(recent), Some(prior)) = (
        floor_per_session(t.recent, &p.project),
        floor_per_session(t.prior, &p.project),
    ) else {
        // A project that ran in only one of the two windows has no trend, and a
        // direction computed against a window it was absent from would read as
        // a collapse or a spike that never happened.
        return o;
    };
    // Pre-divided and pre-rounded, because a ratio is arithmetic and the model
    // may not do arithmetic. The key names carry the windows so a reader (and
    // the model) cannot mistake which side is which.
    o[format!("floor_tokens_per_session_last_{TREND_RECENT_DAYS}d")] = json!(recent);
    o[format!("floor_tokens_per_session_prior_{TREND_PRIOR_DAYS}d")] = json!(prior);
    o["floor_direction"] = json!(match recent.cmp(&prior) {
        std::cmp::Ordering::Greater => "up",
        std::cmp::Ordering::Less => "down",
        // Two figures that round to the same integer are the same figure at the
        // precision this sheet reports. Anything finer is a false trend on a
        // seven-day window.
        std::cmp::Ordering::Equal => "flat",
    });
    o
}

/// Startup tokens per session for one project in one window, or `None` when the
/// window holds no session for it.
fn floor_per_session(ledger: &Ledger, project: &str) -> Option<u64> {
    let row = ledger.projects.iter().find(|p| p.project == project)?;
    (row.sessions > 0).then(|| (row.floor_tokens as f64 / row.sessions as f64).round() as u64)
}

/// Probe measurements, largest schema first.
///
/// Counts only. A server's `env` is where people keep API tokens, and none of it
/// belongs in a prompt: [`McpManifest`] carries no config and this block does
/// not reach for one.
fn manifest_facts(manifests: &[McpManifest]) -> Option<Value> {
    let mut rows: Vec<&McpManifest> = manifests.iter().filter(|m| m.ok).collect();
    if rows.is_empty() {
        return None;
    }
    rows.sort_by(|a, b| {
        b.schema_tokens
            .cmp(&a.schema_tokens)
            .then_with(|| a.server_key.cmp(&b.server_key))
    });
    let items: Vec<Value> = rows
        .iter()
        .take(ADVICE_MAX_MANIFESTS)
        .map(|m| {
            json!({
                "server": m.server_key,
                "scope": scope_name(&m.scope),
                "tool_count": m.tool_count,
                "schema_tokens": m.schema_tokens,
                "basis": if m.tokenizer == crate::probe::TOKENIZER_BYTES_ESTIMATE {
                    basis::ESTIMATED
                } else {
                    basis::MEASURED_MANIFEST
                },
            })
        })
        .collect();
    Some(json!({
        "items": items,
        // Once for the block rather than once per row: it is the same sentence
        // about every row, and sixteen copies of it is context spent saying one
        // thing sixteen times.
        "note": "schema tokens are measured; how the client charges them is an estimate",
    }))
}

/// Which projects call each MCP server. The matrix behind a re-scope.
fn server_usage_facts(usage: &BTreeMap<String, BTreeMap<String, u64>>) -> Option<Value> {
    if usage.is_empty() {
        return None;
    }
    let items: Vec<Value> = usage
        .iter()
        .take(ADVICE_MAX_SERVER_USAGE)
        .map(|(server, by_project)| {
            let mut projects: Vec<(&String, &u64)> = by_project.iter().collect();
            projects.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
            json!({
                "server": server,
                "projects_calling": by_project.len(),
                "projects": projects
                    .iter()
                    .take(ADVICE_MAX_CALLERS)
                    .map(|(project, calls)| json!({
                        "project": basename(project),
                        "calls": *calls,
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    Some(Value::Array(items))
}

/// What each project checked into its own `.mcp.json`.
///
/// Read-only awareness, and the reason a server configured by a repo is never
/// called unused: the spec forbids writing to `.mcp.json` in v1, so a suggestion
/// about one would be a suggestion Piggy cannot apply.
fn project_mcp_facts(p: &ProjectMcpServers) -> Option<Value> {
    if p.by_project.is_empty() {
        return None;
    }
    let items: Vec<Value> = p
        .by_project
        .iter()
        .take(ADVICE_MAX_PROJECT_MCP)
        .map(|(project, servers)| {
            json!({
                "project": basename(project),
                "servers": servers
                    .iter()
                    .take(ADVICE_MAX_SERVERS_PER_PROJECT)
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    Some(Value::Array(items))
}

/// The CLAUDE.md inventory: what each file costs and what was found in it.
///
/// File contents never appear here. They enter a prompt only for the one file a
/// drafting call is about (docs/m5-spec.md: "Contents are read at call time,
/// never stored"), and this sheet is not that call.
fn claudemd_facts(report: &ClaudemdReport) -> Option<Value> {
    if report.files.is_empty() {
        return None;
    }
    let mut files: Vec<&crate::claudemd::ScannedFile> = report.files.iter().collect();
    files.sort_by(|a, b| {
        b.est_tokens_month
            .cmp(&a.est_tokens_month)
            .then_with(|| a.file.path.cmp(&b.file.path))
    });
    let items: Vec<Value> = files
        .iter()
        .take(ADVICE_MAX_CLAUDEMD)
        .map(|f| {
            let mut kinds: Vec<&str> = f.findings.iter().map(|x| x.kind.as_str()).collect();
            kinds.sort_unstable();
            kinds.dedup();
            json!({
                "file": basename(&f.file.path),
                "project": f.file.project.as_deref().map(basename),
                "scope": f.scope(),
                "est_tokens": f.file.est_tokens,
                "sessions_30d": f.sessions_30d,
                "est_tokens_month": f.est_tokens_month,
                "findings": kinds,
            })
        })
        .collect();
    Some(json!({
        "files": items,
        "total_est_tokens_month": report.est_tokens_month(),
    }))
}

/// The savers, as the advice sheet needs them: the same measurements the saver
/// sheet carries, plus whether each one is currently on.
///
/// Same two filters as [`Facts::savers`], and for the same reason: a saver with
/// no comparison yet has no result to rank a suggestion against, and one left on
/// the sheet is one the model will dutifully write about.
fn advice_saver_facts(rows: &[(&Entry, bool, &SaverAttribution)]) -> Option<Value> {
    let items: Vec<Value> = rows
        .iter()
        .filter(|(_, _, a)| a.n_on > 0 && a.n_off > 0)
        .filter(|(_, _, a)| {
            !a.arms()
                .all(|s| matches!(s.reading(), crate::attribution::Reading::Waiting { .. }))
        })
        .take(ADVICE_MAX_SAVERS)
        .map(|(e, enabled, a)| {
            let (rand_on, rand_off) = a.randomized_counts();
            json!({
                // No `saver:` prefix here. Ids on this sheet come from
                // `advice::candidate_id`, which already prefixes with the kind,
                // and a second disambiguator would only be one more string for
                // the model to get wrong.
                "id": e.id,
                "saver": e.name,
                "does": clip(&e.description, MAX_DETAIL),
                "enabled": enabled,
                "sessions_with_it_on": a.n_on,
                "sessions_with_it_off": a.n_off,
                "randomized_on": rand_on,
                "randomized_off": rand_off,
                "badge": best_badge(a),
                "finding": a.summary(),
                "caveat": a.caveat(),
                "streams": a
                    .arms()
                    .map(|s| match s.shown_pct() {
                        Some(p) => settled_stream(s, p),
                        None => json!({
                            "stream": s.stream.label(),
                            "result": s.note(),
                        }),
                    })
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    (!items.is_empty()).then(|| Value::Array(items))
}

/// The strongest claim any one of a saver's arms can back, in the same words
/// [`crate::advice::basis`] labels an evidence row with. The badge and the
/// number have to travel together, and they have to be called the same thing on
/// the card and on the sheet.
fn best_badge(a: &SaverAttribution) -> &'static str {
    if a.arms().any(|s| s.badge == Badge::Measured) {
        basis::MEASURED
    } else if a.arms().any(|s| s.badge == Badge::Estimated) {
        basis::ESTIMATED_AB
    } else {
        basis::MEASURING
    }
}

/// The headline comparison: what the whole saver set has been measured against.
///
/// The multiplier is emitted only when [`MultiplierState::Shown`] says it may
/// be. Every other state carries the reason instead, because a withheld figure
/// and a figure that does not exist yet are different things and want different
/// words.
fn holdout_facts(h: &Headline) -> Value {
    let mut o = json!({
        "baseline": match h.baseline {
            HeadlineBaseline::Holdout => "holdout",
            HeadlineBaseline::PreInstall => "pre-install",
            HeadlineBaseline::None => "none",
        },
        "baseline_is_clean_holdout": h.baseline_clean,
        "sessions_all_savers_on": h.n_full_on,
        "sessions_all_savers_on_randomized": h.n_full_on_randomized,
        "sessions_in_baseline": h.n_baseline,
        "badge_ceiling": match h.ceiling {
            Badge::Measured => basis::MEASURED,
            Badge::Estimated => basis::ESTIMATED_AB,
            Badge::Measuring => basis::MEASURING,
        },
    });
    match (h.multiplier_state, h.multiplier) {
        (MultiplierState::Shown, Some(m)) => o["multiplier"] = json!(round1(m)),
        (MultiplierState::NoData, _) => {
            o["multiplier_withheld_because"] = json!("neither side has a priced rate to compare")
        }
        (MultiplierState::WithheldCostMore, _) => {
            o["multiplier_withheld_because"] =
                json!("the savers came out behind an observational baseline, so the estimate was withheld")
        }
        // `Shown` with no figure cannot happen, and inventing a reason for it
        // would be inventing the one thing this block exists to state honestly.
        (MultiplierState::Shown, None) => {}
    }
    o
}

/// One candidate, as the thing being ranked.
///
/// Evidence values go in verbatim (minus the home directory) because they are
/// already formatted the way the card renders them, and because
/// [`super::guard::Allowlist`] reads numbers out of strings: a value of
/// `"~14,200 tokens"` is what makes `14,200` a figure a rationale may quote.
fn candidate_facts(c: &Candidate) -> Value {
    json!({
        "id": c.id,
        "kind": c.kind.as_str(),
        // The names this candidate is about, which is what a rationale has to
        // name to be about anything. `super::guard` builds its anti-vacuity
        // anchors from exactly this array.
        "about": about(c),
        "title": scrub_home(&c.title),
        "est_tokens_month": c.est_tokens_month,
        // Never "savings". `ClaudemdTrim`'s figure is what the file costs today,
        // and it is the largest number Piggy computes: called a saving, it would
        // put a cost at the top of a savings ranking.
        "est_is": if c.kind.est_is_burden() { "burden" } else { "saving" },
        "risk_tier": c.risk_tier,
        "blocked": c.blocked(),
        "needs_advisor": c
            .prerequisites
            .iter()
            .any(|p| matches!(p, Prerequisite::NeedsAdvisor)),
        "evidence": c
            .evidence
            .iter()
            .take(ADVICE_MAX_EVIDENCE_ROWS)
            .map(|e| json!({
                "label": e.label,
                "value": scrub_home(&e.value),
                "basis": e.basis,
            }))
            .collect::<Vec<_>>(),
    })
}

/// The names a candidate is about, in display form.
///
/// Derived from [`Params`] rather than from [`Candidate::target`], which is a
/// display string that can carry a whole project path. Every element here is a
/// basename or a configured key, so the sheet never spends context on a home
/// directory and never puts one in front of the model.
fn about(c: &Candidate) -> Vec<String> {
    let mut out = Vec::new();
    match &c.params {
        Params::ServerDisable { id, source, .. } => {
            out.push(id.clone());
            out.push(scope_name(source.as_deref().unwrap_or(crate::store::SCOPE_USER)));
        }
        Params::ServerScope { server, projects } => {
            out.push(server.clone());
            out.extend(projects.iter().map(|p| basename(p).to_string()));
        }
        Params::Claudemd { path } => {
            out.push(basename(path).to_string());
            // The directory the file sits in, which for a project file is the
            // project a reader would name it by.
            if let Some(parent) = path.rsplit('/').nth(1).filter(|s| !s.is_empty()) {
                out.push(parent.to_string());
            }
        }
        Params::SaverMix { saver, .. } => out.push(saver.clone()),
    }
    out.retain(|s| !s.is_empty());
    out.dedup();
    out
}

/// A scope as the sheet names it: `user scope`, or the project's basename.
fn scope_name(scope: &str) -> String {
    if scope == crate::store::SCOPE_USER {
        return "user scope".to_string();
    }
    basename(scope).to_string()
}

/// Config items and the aggregates we want the model to be able to quote.
fn sweep_facts(s: &SweepReport) -> Value {
    sweep_facts_capped(s, MAX_SWEEP)
}

fn sweep_facts_capped(s: &SweepReport, max: usize) -> Value {
    let unused: Vec<_> = s.recommended().collect();
    // Only the unused, and only the ones that cost something.
    //
    // Both filters were added after reading real generated output. The full list
    // is mostly hooks estimated at ~0 tokens and add-ons the user actively uses,
    // and both are traps: the model spent its annotations writing
    // "ponytail@ponytail is used 20,303 times and is linked to skill_listing",
    // a fabricated edge built on a true figure, the sort of claim
    // [`super::guard`] cannot catch, because every number in it is real.
    //
    // An item in use explains a cost the reader chose to pay, so it can never be
    // the point of an annotation. Withholding it is what makes the wrong
    // sentence unwritable rather than merely discouraged, which is this module's
    // rule everywhere else.
    let items: Vec<Value> = unused
        .iter()
        .filter(|i| i.est_tokens > 0)
        .take(max)
        .map(|i| {
            json!({
                "kind": i.kind,
                "name": i.id,
                // Which floor component this item is part of, so the model
                // matches on a field instead of inferring the edge. Told to
                // infer it, a 4B produced "ponytail@ponytail is linked to
                // skill_listing" for a plugin in daily use; told nothing, it
                // returned an empty array and missed the one true link on the
                // sheet. The edge is product behaviour, not a measurement, so
                // it is named here rather than in the ledger.
                "inflates": inflates(&i.kind),
                "used": i.used,
                // Without this flag the model would compare a lifetime counter
                // against a windowed one and call the difference a trend.
                "usage_is_windowed": i.used_windowed,
                "est_tokens_per_session": i.est_tokens,
                // Where that figure came from. Since M5.1 an MCP row can be a
                // real probe measurement rather than the config-size heuristic,
                // and the label has to travel with the number or a sheet that
                // says "estimate" everywhere makes a measured figure a guess.
                "cost_basis": i.cost_basis,
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

/// The floor component an add-on of this kind is loaded as part of, matching the
/// attachment kinds the parser charges under `floor:`.
///
/// Hooks are absent deliberately: their output is charged to `hook_success`, but
/// the sweep cannot tell which configured hook wrote which attachment, and a
/// guess there would name the wrong hook rather than none.
fn inflates(kind: &str) -> Option<&'static str> {
    match kind {
        "plugin" | "skill" => Some("skill_listing"),
        "mcp" => Some("deferred_tools_delta"),
        _ => None,
    }
}

/// Last path segment, so the fact sheet carries `Stacked` rather than a full
/// home directory path. Shorter, and no reason to spend context on the prefix.
fn basename(p: &str) -> &str {
    p.rsplit('/').find(|s| !s.is_empty()).unwrap_or(p)
}

/// The home directory, once per process.
static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// `~` in place of the user's home directory.
///
/// Every path this sheet builds itself is already a basename, but two strings
/// come through from elsewhere and can carry a whole path: an evidence value
/// listing the references a CLAUDE.md makes, and a finding's own prose. Neither
/// is a secret, and neither is worth the context or worth handing to a model
/// that might write it back out. Only the prefix changes, so no figure in the
/// string moves and the allow-list sees exactly what the card shows.
fn scrub_home(s: &str) -> String {
    let Some(home) = HOME
        .get_or_init(|| {
            dirs::home_dir().map(|h| h.to_string_lossy().trim_end_matches('/').to_string())
        })
        .as_deref()
        .filter(|h| !h.is_empty() && *h != "/")
    else {
        return s.to_string();
    };
    if !s.contains(home) {
        return s.to_string();
    }
    s.replace(home, "~")
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
