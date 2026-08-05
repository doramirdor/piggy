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

use crate::attribution::{SaverAttribution, Stream, StreamStat};
use crate::insights::Insight;
use crate::ledger::Ledger;
use crate::registry::Entry;
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

/// Config items and the aggregates we want the model to be able to quote.
fn sweep_facts(s: &SweepReport) -> Value {
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
        .take(MAX_SWEEP)
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
