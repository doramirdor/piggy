//! The check that makes a local model safe to put next to a receipt.
//!
//! [`crate::insights`] promises every number is arithmetic on observed tokens.
//! A language model writing prose next to those numbers threatens that promise
//! in one specific way: it invents a figure that reads exactly like the real
//! ones. Prompting against it reduces the rate; it does not make the guarantee.
//!
//! So the guarantee is enforced here instead. Every number in the fact sheet
//! goes into an allow-list, every number in the model's output is extracted, and
//! **any figure that is not already a fact rejects the annotation**. The model
//! cannot state a token count, a percentage, or a multiplier that Piggy did not
//! compute, because there is no path from its output to the UI that does not
//! pass through this function.
//!
//! Everything here fails closed. A malformed response, an unknown insight id, an
//! unquotable number: all of them drop the annotation and leave the
//! deterministic finding rendering exactly as it would have without a model.

use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::facts::Facts;

/// Caps on what an annotation may be. A model that ignores them is a model
/// producing something other than the two short strings we asked for.
const MAX_HEADLINE: usize = 120;
const MAX_WHY: usize = 400;

/// Words that mean the model is guessing at a cause.
///
/// The prompt already asks it not to. Asking is not enough: on a live 4B run,
/// two of three annotations dropped the hedging after the rule was added and the
/// third still wrote "This suggests the hook is configured to...". That is the
/// same lesson as the numeric allow-list, so it gets the same treatment. Next to
/// a receipt, a confident-sounding guess about *why* is worth less than nothing,
/// because the reader cannot tell it apart from the measured half.
const HEDGES: &[&str] = &[
    "likely",
    "probably",
    "suggests",
    "appears to",
    "may be",
    "might be",
    "possibly",
    "presumably",
    "seems to",
    "i think",
    "perhaps",
];

/// One accepted annotation, attached to a finding the ledger already produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// Must match an id from [`crate::insights`].
    pub insight_id: String,
    /// A sharper restatement of the finding's title.
    pub headline: String,
    /// Why it is happening, grounded in the user's configuration.
    pub why: String,
}

/// Numbers the model is permitted to write.
#[derive(Debug, Clone, Default)]
pub struct Allowlist(BTreeSet<String>);

impl Allowlist {
    /// Harvest every number reachable in the fact sheet, from numeric JSON
    /// values *and* from inside strings.
    ///
    /// Strings matter as much as numbers: the findings carry prose like
    /// "1,234,567 of 3,000,000 cache-write tokens", and those figures are facts
    /// the model is entitled to repeat.
    pub fn from_facts(facts: &Facts) -> Self {
        let mut set = BTreeSet::new();
        walk(&facts.value, &mut set);
        Allowlist(set)
    }

    fn admit(set: &mut BTreeSet<String>, v: f64) {
        if !v.is_finite() {
            return;
        }
        set.insert(key(v));
        // A restatement that rounds is still a restatement of the same fact.
        // Rounding *up or down* is not admitted, only nearest: `ceil` on 1.2
        // would hand the model a "2" it could attach to something else.
        set.insert(key(v.round()));
        set.insert(key((v * 10.0).round() / 10.0));
    }

    /// Whether every number in `text` is a fact.
    ///
    /// Returns the offending figures, so a rejection can be logged with the
    /// thing the model made up rather than just a count.
    pub fn offenders(&self, text: &str) -> Vec<String> {
        numbers_in(text)
            .into_iter()
            .filter(|n| !self.0.contains(&key(*n)))
            .map(|n| key(n))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn walk(v: &Value, set: &mut BTreeSet<String>) {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                Allowlist::admit(set, f);
            }
        }
        Value::String(s) => {
            for n in numbers_in(s) {
                Allowlist::admit(set, n);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, set)),
        Value::Object(o) => o.values().for_each(|x| walk(x, set)),
        _ => {}
    }
}

/// Canonical text for a number, so `35`, `35.0` and `35.00` are one entry.
fn key(v: f64) -> String {
    let r = (v * 1000.0).round() / 1000.0;
    let mut s = format!("{r:.3}");
    while s.contains('.') && (s.ends_with('0') || s.ends_with('.')) {
        s.pop();
    }
    if s == "-0" {
        s = "0".into();
    }
    s
}

/// Pull every number out of free text.
///
/// Handles the forms this output actually contains: thousands separators
/// (`1,234,567`), decimals (`34.7`), percentages (`35%`), and multipliers
/// (`1.4x`). A `k`/`m` suffix is expanded strictly (`12k` is twelve thousand and
/// nothing else) so an abbreviation can never borrow an unrelated small integer
/// from the facts.
///
/// Hand-rolled rather than a regex because the crate has no regex dependency and
/// this is not worth adding one for.
fn numbers_in(text: &str) -> Vec<f64> {
    let b: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        // Integer part, allowing `,` only when it separates groups of digits.
        while i < b.len()
            && (b[i].is_ascii_digit()
                || (b[i] == ',' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
        {
            i += 1;
        }
        // Fractional part. A trailing '.' is sentence punctuation, not a decimal.
        if i < b.len() && b[i] == '.' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
        let raw: String = b[start..i].iter().filter(|c| **c != ',').collect();
        let Ok(mut v) = raw.parse::<f64>() else {
            continue;
        };
        // Scale suffixes, only when they terminate the token.
        if i < b.len() {
            let next_is_word = b.get(i + 1).map(|c| c.is_alphanumeric()).unwrap_or(false);
            if !next_is_word {
                match b[i] {
                    'k' | 'K' => {
                        v *= 1_000.0;
                        i += 1;
                    }
                    'm' | 'M' => {
                        v *= 1_000_000.0;
                        i += 1;
                    }
                    _ => {}
                }
            }
        }
        out.push(v);
    }
    out
}

/// Parse the model's response and keep only annotations that survive every check.
///
/// Rejections are silent by design. The caller renders the deterministic
/// findings either way, so a dropped annotation costs the user nothing, while a
/// surviving fabrication costs them their trust in the numbers next to it.
pub fn accept(raw: &str, facts: &Facts) -> Result<Vec<Annotation>> {
    let names = config_names(facts);
    // A ledger annotation exists to name the configuration item behind a
    // finding, so one that names none has nothing to say.
    accept_with(raw, facts, &|_id, text| {
        names.iter().any(|n| text.contains(n.as_str()))
    })
}

/// The saver sheet's acceptance pass ([`super::facts::Facts::savers`]).
///
/// Identical checks, with one substitution: the thing an annotation must name
/// is the **saver it is about**, not a configuration item. Without that swap
/// every saver annotation would be dropped, since the saver sheet carries no
/// `configuration` block at all; with it, a model that writes about the wrong
/// saver or about savers in general still fails closed.
pub fn accept_savers(raw: &str, facts: &Facts) -> Result<Vec<Annotation>> {
    let names = saver_names(facts);
    let findings = findings(facts);
    let example = example();
    accept_with(raw, facts, &|id, text| {
        if !names
            .iter()
            .any(|(fid, name)| fid == id && text.contains(name.as_str()))
        {
            return false;
        }
        if unsupported(text) || restates(&example, text) {
            return false;
        }
        !findings
            .iter()
            .any(|(fid, finding)| fid == id && restates(finding, text))
    })
}

/// Claims about a stream that was never measurable.
///
/// The saver sheet withholds the medians of an unsettled stream precisely so
/// this sentence cannot be built (see [`super::facts::Facts::savers`]), and a
/// live 4B wrote it anyway, from nothing: "enabling RTK improves efficiency
/// without increasing output or turns", about a saver whose turns arm the sheet
/// says was too small to compare. Reassurance about an unmeasured stream is the
/// most expensive thing this advisor could print, because it is the exact claim
/// the measurement refused to make.
const UNSUPPORTED: &[&str] = &[
    "no impact",
    "no effect",
    "no downside",
    "without increasing",
    "without affecting",
    "no increase",
    "not increase",
    "no additional",
    "no extra",
    "no risk",
    "without risk",
    "risk-free",
    "risk free",
    "safe to enable",
    "no cost",
    "minimal overhead",
    "no overhead",
];

fn unsupported(text: &str) -> bool {
    UNSUPPORTED.iter().any(|p| text.contains(p))
}

/// The worked example in the saver prompt, which a small model will happily
/// paste back with a different saver's id on it.
///
/// The example earns its place: without it a 4B wrote about the three biggest
/// percentages and ignored the saver that had been shown to do nothing, which is
/// the line the reader most needs. With it, the model picked the right saver and
/// returned the example's own sentences word for word. A pasted example is not an
/// analysis, and the next time it is pasted it may land on a saver it is false
/// about, so it is rejected the same way a restatement is.
///
/// These two constants are the **only** copy of that example: the prompt splices
/// them in rather than spelling the sentences out again (`llama::saver_preamble`).
/// An earlier version kept its own copy here, the prompt was rewritten, and the
/// check spent that whole time matching a sentence no model had ever been shown.
pub const EXAMPLE_HEADLINE: &str = "Example cuts cache write";
pub const EXAMPLE_WHY: &str = "It reduces cache write and lowers your token costs.";

/// The example in the shape [`accept_with`] hands to the anti-vacuity check:
/// headline and `why` joined, lowercased.
fn example() -> String {
    format!("{EXAMPLE_HEADLINE} {EXAMPLE_WHY}").to_lowercase()
}

/// Whether an annotation is the finding in different words.
///
/// The one thing the prompt cannot make a small model stop doing. Told twice,
/// in the system prompt and again in the task, a 4B still answered
/// "93% less cache write" with "Reduces cache write by 93%" and called it
/// advice. The reader has the finding on the same row, so a restatement is
/// worse than silence: it doubles the height of the panel and adds nothing.
///
/// Measured by content-word overlap in the direction that matters: how much of
/// the annotation came *out of* the finding. A line that is mostly the
/// finding's own words is a restatement however it is arranged.
fn restates(finding: &str, text: &str) -> bool {
    let from_finding: BTreeSet<&str> = content_words(finding).collect();
    if from_finding.is_empty() {
        return false;
    }
    let words: Vec<&str> = content_words(text).collect();
    if words.is_empty() {
        return true;
    }
    let borrowed = words.iter().filter(|w| from_finding.contains(*w)).count();
    borrowed * 10 >= words.len() * 6
}

/// Words worth comparing: everything that is not punctuation, a number, or one
/// of the joins every English sentence has.
fn content_words(s: &str) -> impl Iterator<Item = &str> {
    const STOP: &[&str] = &[
        "the", "a", "an", "and", "or", "but", "with", "without", "on", "off", "in", "of", "to",
        "it", "its", "this", "that", "is", "are", "was", "were", "be", "been", "has", "have",
        "had", "for", "by", "at", "as", "from", "than", "then", "so", "you", "your", "per",
    ];
    s.split(|c: char| !c.is_alphanumeric() && c != '%')
        .filter(|w| w.len() > 2)
        .filter(|w| !w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(|w| w.trim_end_matches('%'))
        .filter(|w| !w.is_empty() && !STOP.contains(&w.to_lowercase().as_str()))
}

/// `(id, finding)` for every saver on the sheet, lowercased.
fn findings(facts: &Facts) -> Vec<(String, String)> {
    let Some(items) = facts.value.get("savers").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|i| {
            Some((
                i.get("id")?.as_str()?.to_string(),
                i.get("finding")?.as_str()?.to_lowercase(),
            ))
        })
        .collect()
}

/// The shared body. `must_name` is the anti-vacuity check, and it is the only
/// thing the two sheets disagree about: it receives the annotation's id and its
/// lowercased text, and returns whether the annotation names the thing it is
/// required to name.
fn accept_with(
    raw: &str,
    facts: &Facts,
    must_name: &dyn Fn(&str, &str) -> bool,
) -> Result<Vec<Annotation>> {
    let allow = Allowlist::from_facts(facts);
    let parsed: Value = serde_json::from_str(&extract_json(raw))
        .context("the model did not return the requested JSON array")?;
    let Some(items) = parsed.as_array() else {
        bail!("the model returned {} rather than an array", kind_of(&parsed));
    };

    let mut out: Vec<Annotation> = Vec::new();
    for item in items {
        let Some(id) = item.get("insight_id").and_then(|v| v.as_str()) else {
            continue;
        };
        // The model annotates findings. It does not get to name a new one.
        if !facts.insight_ids.iter().any(|k| k == id) {
            continue;
        }
        // One annotation per finding: a second is the model repeating itself.
        if out.iter().any(|a| a.insight_id == id) {
            continue;
        }
        let headline = item.get("headline").and_then(|v| v.as_str()).unwrap_or("").trim();
        let why = item.get("why").and_then(|v| v.as_str()).unwrap_or("").trim();
        if headline.is_empty() || why.is_empty() {
            continue;
        }
        if headline.chars().count() > MAX_HEADLINE || why.chars().count() > MAX_WHY {
            continue;
        }
        if !allow.offenders(headline).is_empty() || !allow.offenders(why).is_empty() {
            continue;
        }
        if hedges(headline) || hedges(why) {
            continue;
        }
        // The annotation's whole job is to name the configuration item behind a
        // finding, so one that names none has nothing to say. Told to skip those
        // findings, a live 4B instead returned "No configuration item inflates
        // hook_success" as an annotation: true, useless, and printed under a
        // finding as though it were an explanation. Silence is the correct
        // rendering of "nothing to add", and this is what produces it.
        if !must_name(id, &format!("{headline} {why}").to_lowercase()) {
            continue;
        }
        out.push(Annotation {
            insight_id: id.to_string(),
            headline: headline.to_string(),
            why: why.to_string(),
        });
    }
    Ok(out)
}

/// Every configuration item the fact sheet offered, plus the bare form of a
/// `plugin@marketplace` name, which is how a model refers to it half the time.
///
/// Empty when the sweep did not run. Nothing is annotated in that case, which is
/// correct rather than unfortunate: with no configuration there is no link to
/// draw, and the deterministic findings already say everything else.
fn config_names(facts: &Facts) -> Vec<String> {
    let Some(items) = facts.value.pointer("/configuration/items").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in items.iter().filter_map(|i| i.get("name")?.as_str()) {
        out.push(name.to_lowercase());
        if let Some(bare) = name.split('@').next().filter(|b| !b.is_empty() && *b != name) {
            out.push(bare.to_lowercase());
        }
    }
    out
}

/// `(id, saver name)` for every saver on the sheet, lowercased. The name rather
/// than the id: the model is writing for a reader, and "Sweep" is what the
/// reader sees on the row.
fn saver_names(facts: &Facts) -> Vec<(String, String)> {
    let Some(items) = facts.value.get("savers").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|i| {
            Some((
                i.get("id")?.as_str()?.to_string(),
                i.get("saver")?.as_str()?.to_lowercase(),
            ))
        })
        .collect()
}

/// Slice out the JSON array, tolerating a code fence, a preamble, and a
/// **truncated tail**.
///
/// Truncation is the common case, not an edge case: a small model writes far
/// longer `why` strings than it is asked to and runs into the token ceiling
/// mid-object. Since the array's items are independent, losing the whole
/// response because the last one is half-written throws away good annotations
/// for no reason. So an unterminated array is closed after its last complete
/// item.
///
/// This is a **parsing** repair, not a content one. Every salvaged item still
/// goes through the id check and the numeric allow-list; nothing here decides
/// what is true.
fn extract_json(raw: &str) -> String {
    let Some(s) = raw.find('[') else {
        return raw.trim().to_string();
    };

    // Depth-aware scan, so a bracket inside a string cannot end the array early.
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    // Byte index just past the last `}` that closed a top-level item.
    let mut last_item_end: Option<usize> = None;

    for (i, c) in raw[s..].char_indices() {
        let at = s + i;
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' | '{' => depth += 1,
            ']' | '}' => {
                depth -= 1;
                // Back to depth 1 means an item just closed inside the array.
                if c == '}' && depth == 1 {
                    last_item_end = Some(at + c.len_utf8());
                }
                // Depth 0 is the array's own close: a complete response.
                if depth == 0 {
                    return raw[s..=at].to_string();
                }
            }
            _ => {}
        }
    }

    // Unterminated. Keep whole items and close the array ourselves.
    match last_item_end {
        Some(end) => format!("{}]", &raw[s..end]),
        // Not even one complete item survived, so there is nothing to salvage.
        None => raw[s..].to_string(),
    }
}

/// Whether `text` hedges, i.e. admits to guessing at a cause.
fn hedges(text: &str) -> bool {
    let lower = text.to_lowercase();
    HEDGES.iter().any(|h| lower.contains(h))
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}
