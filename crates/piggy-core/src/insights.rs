//! Findings derived from the context ledger.
//!
//! Every insight here is **arithmetic on observed tokens**, never a prediction
//! and never a statistical claim. [`crate::attribution`] is where uncertainty
//! lives; this module only ever says "you spent X on Y", plus a lever that is
//! true by construction (open half as many sessions, pay half as many floors).
//!
//! The rules that keep it honest:
//!
//! * A detector fires on a **threshold over real tokens**, not a ranking. Always
//!   showing "your top 3 problems" manufactures findings on a clean setup.
//! * `tokens` is what was *actually spent* on the thing named. Where a lever is
//!   quoted, the arithmetic behind it is stated in `detail` so the user can
//!   disagree with the assumption rather than the number.
//! * Nothing is claimed about savings from a saver. That needs randomization.

use anyhow::Result;

use crate::ledger::Ledger;
use crate::pricing::Pricing;
use crate::parser::CTX_FLOOR_PREFIX;
use crate::store::Store;

/// How loud a finding is. Ordering matters: [`Insight`]s sort by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth knowing.
    Info,
    /// Costing real tokens; a lever exists.
    Notable,
    /// Dominating spend.
    High,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Notable => "notable",
            Severity::Info => "info",
        }
    }
}

/// One finding: what it is, what it cost, and what to do.
#[derive(Debug, Clone)]
pub struct Insight {
    /// Stable id, so a UI can dismiss or link one without matching on prose.
    pub id: String,
    pub severity: Severity,
    pub title: String,
    /// The arithmetic, stated so the user can check it.
    pub detail: String,
    /// Tokens actually spent on whatever the finding is about.
    pub tokens: u64,
    /// The lever. Imperative, specific, and never "consider".
    pub action: String,
}

/// A project pays this many session floors before it is worth mentioning.
const CHURN_MIN_SESSIONS: u64 = 20;
/// …and averages fewer than this many assistant messages per session.
const CHURN_MAX_MSGS: f64 = 4.0;
/// …and actually wastes tokens on it. Short sessions are only a finding when
/// the floor dominates them: a 37-session project at 21% overhead is someone
/// working in small chunks, not a harness burning startup cost, and flagging it
/// trains the user to ignore the list.
const CHURN_MIN_OVERHEAD: f64 = 0.5;
/// Floor share of all cache writes that counts as dominating.
const FLOOR_HIGH: f64 = 0.35;
/// A named floor component must cost at least this per session to be a finding.
/// Below it, turning the thing off is not worth the user's attention.
const COMPONENT_MIN_PER_SESSION: u64 = 500;

/// Every finding the ledger supports, loudest first.
pub fn insights(ledger: &Ledger) -> Vec<Insight> {
    let mut out = Vec::new();
    let total = ledger.total_tokens();
    if total == 0 {
        return out;
    }
    let sessions: u64 = ledger.projects.iter().map(|p| p.sessions).sum();

    floor_dominates(ledger, total, &mut out);
    floor_components(ledger, sessions, &mut out);
    session_churn(ledger, &mut out);
    per_turn_injections(ledger, total, &mut out);

    // Loudest first, then by tokens: two High findings should not be ordered by
    // whichever detector happened to run first.
    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.tokens.cmp(&a.tokens)));
    out
}

/// The headline: how much of everything went to opening sessions.
fn floor_dominates(l: &Ledger, total: u64, out: &mut Vec<Insight>) {
    let overhead = l.overhead();
    if overhead < FLOOR_HIGH {
        return;
    }
    let floor: u64 = l.rows.iter().filter(|r| r.is_floor()).map(|r| r.tokens).sum();
    let sessions: u64 = l.projects.iter().map(|p| p.sessions).sum();
    let per = floor / sessions.max(1);
    out.push(Insight {
        id: "floor-dominates".into(),
        severity: Severity::High,
        title: format!("{:.0}% of your tokens went to starting sessions", overhead * 100.0),
        detail: format!(
            "{} of {} cache-write tokens were the session floor, across {} sessions \
             ({} per session before you typed anything).",
            commas(floor),
            commas(total),
            commas(sessions),
            commas(per)
        ),
        tokens: floor,
        action: "Fewer, longer sessions pay this once instead of many times. \
                 The per-project table shows which ones churn."
            .into(),
    });
}

/// What the floor is actually made of, for the parts that are logged.
fn floor_components(l: &Ledger, sessions: u64, out: &mut Vec<Insight>) {
    if sessions == 0 {
        return;
    }
    for r in l.rows.iter().filter(|r| r.kind.starts_with(CTX_FLOOR_PREFIX)) {
        let per = r.tokens / sessions.max(1);
        if per < COMPONENT_MIN_PER_SESSION {
            continue;
        }
        // Off the KIND, not the label: ids are for dismissing and linking, and
        // must not move when display prose changes.
        let name = r.kind.strip_prefix(CTX_FLOOR_PREFIX).unwrap_or(&r.kind);
        out.push(Insight {
            id: format!("floor-component:{name}"),
            severity: if per >= 3_000 { Severity::High } else { Severity::Notable },
            title: format!("{name} costs ~{} tokens every session", commas(per)),
            detail: format!(
                "{} in total across {} sessions. It is loaded into context before your \
                 first message, so you pay it whether or not the session uses it.",
                commas(r.tokens),
                commas(sessions)
            ),
            tokens: r.tokens,
            action: format!(
                "If you don't need {name} in every project, scope or disable it and the \
                 floor shrinks by roughly that much per session."
            ),
        });
    }
}

/// Projects that open many sessions and barely use them.
fn session_churn(l: &Ledger, out: &mut Vec<Insight>) {
    for p in &l.projects {
        if p.sessions < CHURN_MIN_SESSIONS
            || p.msgs_per_session() >= CHURN_MAX_MSGS
            || p.overhead() < CHURN_MIN_OVERHEAD
            || p.floor_tokens == 0
        {
            continue;
        }
        let name = p.project.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&p.project);
        out.push(Insight {
            id: format!("churn:{}", p.project),
            severity: if p.overhead() >= 0.8 { Severity::High } else { Severity::Notable },
            title: format!(
                "{} ran {} sessions averaging {:.1} messages",
                name,
                commas(p.sessions),
                p.msgs_per_session()
            ),
            detail: format!(
                "{} tokens of session floor bought {} tokens of work ({:.0}% overhead). \
                 Each of those sessions paid the full startup cost for about one exchange.",
                commas(p.floor_tokens),
                commas(p.work_tokens),
                p.overhead() * 100.0
            ),
            tokens: p.floor_tokens,
            action: "If a script or harness drives this, reuse one session across \
                     iterations instead of starting a new one each time."
                .into(),
        });
    }
}

/// Per-turn injections, reported together: individually they are small, and a
/// finding per attachment type would bury the ones that matter.
fn per_turn_injections(l: &Ledger, total: u64, out: &mut Vec<Insight>) {
    let rows: Vec<_> = l
        .rows
        .iter()
        .filter(|r| r.removable() && !r.is_floor())
        .collect();
    let sum: u64 = rows.iter().map(|r| r.tokens).sum();
    // Under 1% this is noise, and saying so is more useful than a finding.
    if sum == 0 || (sum as f64 / total as f64) < 0.01 {
        return;
    }
    let top = rows.first().map(|r| r.label()).unwrap_or_default();
    out.push(Insight {
        id: "per-turn-injections".into(),
        severity: Severity::Notable,
        title: format!(
            "Per-turn injections cost {} tokens ({:.1}%)",
            commas(sum),
            sum as f64 / total as f64 * 100.0
        ),
        detail: format!(
            "Across {} kinds, largest is {}. These are hooks, reminders and listings \
             re-sent during a session rather than at startup.",
            rows.len(),
            top
        ),
        tokens: sum,
        action: "Trim the noisiest hooks if they fire on every tool call.".into(),
    });
}

/// Thousands separators without pulling in a formatting crate.
fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Store {
    /// Build the ledger for `since` and derive its findings.
    pub fn insights(&self, since: Option<&str>, pricing: &Pricing) -> Result<Vec<Insight>> {
        Ok(insights(&self.ledger(since, pricing)?))
    }
}
