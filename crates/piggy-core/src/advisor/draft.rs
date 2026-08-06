//! The checks a drafted CLAUDE.md has to pass, and the splitter that decides
//! how many calls it takes.
//!
//! [`super::guard`] is the same idea one level up: there the model re-words
//! facts and the check is that every figure was already one. Here the model
//! rewrites the **user's own file**, so the checks are about what a rewrite may
//! not do: grow, invent a path, invent a heading, or quietly change a number
//! inside guidance the user still relies on.
//!
//! Not feature-gated, for the same reason [`super::prompts`] is not: these are
//! the rules, and rules whose tests only run under `local-llm` are rules almost
//! nobody runs.
//!
//! A failed check demotes the candidate rather than failing the pass. Concretely
//! [`crate::advice::Candidate::new_content`] stays `None`,
//! [`crate::advice::Prerequisite::NeedsAdvisor`] stays in `prerequisites`, and
//! [`crate::advice::Candidate::blocked`] therefore stays true, which is the
//! deterministic presentation the spec asks for.

use std::collections::BTreeSet;

use super::guard::{key, numbers_in};
use super::prompts::{DRAFT_CLOSE, DRAFT_OPEN};

/// Why a draft was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftReject {
    /// The markers the prompt asked for were not both there, which usually means
    /// the generation was cut off mid-file.
    NoSentinels,
    Empty,
    /// Did not shrink by at least a tenth.
    TooLarge { original: usize, draft: usize },
    /// A path or URL the source never contained.
    NewReference(String),
    /// A heading the source never contained.
    NewHeading(String),
    /// A figure the source never contained.
    NewNumber(String),
}

impl DraftReject {
    /// One line for a log or a live test. Never shown to a user: a refused draft
    /// is invisible by design, and the candidate just stays blocked.
    pub fn reason(&self) -> String {
        match self {
            DraftReject::NoSentinels => "the model did not return the file between the markers".into(),
            DraftReject::Empty => "the draft was empty".into(),
            DraftReject::TooLarge { original, draft } => {
                format!("the draft is {draft} bytes against {original}, under a tenth smaller")
            }
            DraftReject::NewReference(r) => format!("the draft introduced a reference: {r}"),
            DraftReject::NewHeading(h) => format!("the draft introduced a heading: {h}"),
            DraftReject::NewNumber(n) => format!("the draft introduced a number: {n}"),
        }
    }
}

/// A draft has to be at least this much smaller, as a fraction expressed in
/// integers so no float rounding decides an acceptance.
/// `draft * 10 <= original * 9` is "at least 10% smaller" (docs/m5-spec.md).
const SHRINK_NUM: usize = 9;
const SHRINK_DEN: usize = 10;

/// Content checks only: everything except the whole-file shrink rule.
///
/// Used per section, where shrinking is not required of any one section, and
/// again on the joined file by [`accept_draft`].
pub fn check_draft_content(original: &str, raw: &str) -> Result<String, DraftReject> {
    let draft = normalize(original, between_sentinels(raw).ok_or(DraftReject::NoSentinels)?);
    if draft.trim().is_empty() {
        return Err(DraftReject::Empty);
    }

    let was = refs_in(original);
    if let Some(r) = refs_in(&draft).into_iter().find(|r| !was.contains(r)) {
        return Err(DraftReject::NewReference(r));
    }
    let was = headings_in(original);
    if let Some(h) = headings_in(&draft).into_iter().find(|h| !was.contains(h)) {
        return Err(DraftReject::NewHeading(h));
    }
    let was: BTreeSet<String> = numbers_in(original).into_iter().map(key).collect();
    if let Some(n) = numbers_in(&draft).into_iter().map(key).find(|n| !was.contains(n)) {
        return Err(DraftReject::NewNumber(n));
    }
    Ok(draft)
}

/// Every content check, plus the shrink rule.
///
/// The shrink rule applies to the **whole file** and never to a section: a
/// section that legitimately cannot shrink must not veto the file, and a file
/// that did not shrink is not worth showing a diff for.
pub fn accept_draft(original: &str, raw: &str) -> Result<String, DraftReject> {
    let draft = check_draft_content(original, raw)?;
    // Integer arithmetic on the normalized bytes, so the figure the rule tests
    // is the figure the file would land at. A draft the same size or larger
    // fails this same comparison, so there is no separate "no growth" check.
    if draft.len() * SHRINK_DEN > original.len() * SHRINK_NUM {
        return Err(DraftReject::TooLarge {
            original: original.len(),
            draft: draft.len(),
        });
    }
    Ok(draft)
}

/// A draft that has already been checked, joined from checked sections.
///
/// The join is what [`accept_draft`] runs on, so a file assembled section by
/// section is held to exactly the rules a one-call draft is.
pub fn accept_joined(original: &str, joined: &str) -> Result<String, DraftReject> {
    accept_draft(original, &format!("{DRAFT_OPEN}{joined}{DRAFT_CLOSE}"))
}

/// The text strictly between the first opening marker and the first closing
/// marker after it.
fn between_sentinels(raw: &str) -> Option<&str> {
    let open = raw.find(DRAFT_OPEN)? + DRAFT_OPEN.len();
    let rest = &raw[open..];
    let close = rest.find(DRAFT_CLOSE)?;
    Some(&rest[..close])
}

/// Put the draft back into the source's shape before anything measures it.
///
/// Line endings and the trailing newline are not the model's to change. A draft
/// that silently rewrote CRLF to LF would make Undo restore bytes the user never
/// had, which is the one property journey 3 in the spec turns on. Runs before
/// the size comparison so the byte counts are the ones that would be written.
///
/// A BOM is stripped here and put back by [`crate::advice::attach_draft`], which
/// is the only place that knows whether the file had one.
fn normalize(original: &str, draft: &str) -> String {
    let draft = draft.strip_prefix('\u{FEFF}').unwrap_or(draft);
    let mut out = draft.replace("\r\n", "\n");
    if original.contains("\r\n") {
        out = out.replace('\n', "\r\n");
    }
    let trimmed = out.trim_end_matches(['\n', '\r']).to_string();
    if original.ends_with('\n') {
        // Exactly one, whatever the model volunteered.
        return format!("{trimmed}{}", if original.contains("\r\n") { "\r\n" } else { "\n" });
    }
    trimmed
}

/// Every path-like or URL-like token in `text`, scanning **every** line
/// including fenced code blocks.
///
/// Deliberately over-collects, and deliberately does not reuse
/// [`crate::claudemd`]'s path scanner. That one skips fenced blocks, so a draft
/// smuggling a new path into a code fence would pass unchecked, and it rejects
/// URLs and unanchored relative paths on purpose, both of which the spec's rule
/// covers.
///
/// Both sides of the comparison run through this same function, so a token that
/// exists in both is fine however loosely it was matched. The only thing this
/// decides is whether the draft INTRODUCED one. Over-collecting costs a rejected
/// draft, which is a designed state; under-collecting lets an invented path into
/// the user's own guidance. That asymmetry is why the net is wide.
pub fn refs_in(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for token in text.split_whitespace() {
        // A markdown link contributes its target, not its label.
        let token = match token.rfind("](") {
            Some(i) => &token[i + 2..],
            None => token,
        };
        let token = trim_punctuation(token);
        // `docs/x.md#section` and `docs/x.md` are the same reference.
        let token = token.split('#').next().unwrap_or(token);
        if token.is_empty() {
            continue;
        }
        let looks_like_ref = token.contains("://")
            || token.starts_with("www.")
            || token.starts_with("mailto:")
            || token.starts_with("~/")
            || token.starts_with("./")
            || token.starts_with("../")
            || token.starts_with('/')
            || token.contains('/');
        if looks_like_ref {
            // Lowercased: a case-only change to a path is not a change worth
            // policing, and case sensitivity here produces false rejections on
            // macOS's case-insensitive filesystem.
            out.insert(token.to_lowercase());
        }
    }
    out
}

/// Strip brackets, quotes, backticks and sentence punctuation from both ends,
/// repeatedly until nothing more comes off.
fn trim_punctuation(mut s: &str) -> &str {
    loop {
        let trimmed = s
            .trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']' | '<' | '>' | '`' | '"' | '\'' | '*' | '_'))
            .trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if trimmed == s {
            return s;
        }
        s = trimmed;
    }
}

/// Every ATX heading in `text`, normalized.
///
/// A `BTreeSet` is what makes "merges allowed" (docs/m5-spec.md) fall out for
/// free: the draft may drop any heading and may not add one.
///
/// Setext headings (`===` / `---` underlines) are not recognised and do not need
/// to be. A draft that converted one to ATX would be introducing a "new"
/// heading and would be refused, which is the conservative outcome.
pub fn headings_in(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut fenced = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // Up to three leading spaces, then one to six hashes, then whitespace.
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        if !(1..=6).contains(&hashes) {
            continue;
        }
        let rest = &trimmed[hashes..];
        if !rest.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let text = rest.trim().trim_end_matches('#').trim();
        if text.is_empty() {
            continue;
        }
        out.insert(
            text.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase(),
        );
    }
    out
}

/// One piece of a file the drafting call handles on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The heading text this section opens with, or `None` for the preamble
    /// above the first `##`.
    pub heading: Option<String>,
    /// The section's own text, separators included, so a plain concatenation
    /// rebuilds the file byte for byte.
    pub text: String,
    /// True when this section alone is over the per-call cap.
    ///
    /// Such a section is passed through verbatim rather than drafted. Failing
    /// the whole file because one section is enormous throws away the shrink
    /// available everywhere else, and a pass-through is visible in the diff as
    /// exactly what it is: unchanged.
    pub too_large: bool,
}

/// Split a file on level-two headings, marking any section that is over the cap
/// on its own.
///
/// `tokens` measures a string the way the loaded model does, so the cap is a
/// real token count rather than a guess. A `##` inside a fenced code block is
/// not a boundary: the fence tracking is the same discipline
/// [`crate::claudemd`] uses when it walks a file for paths.
pub fn split_sections(text: &str, cap: usize, tokens: &dyn Fn(&str) -> usize) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut current = String::new();
    let mut heading: Option<String> = None;
    let mut fenced = false;

    for line in crate::advice::lines_with_endings(text) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        }
        let boundary = !fenced
            && line.starts_with("##")
            && !line.starts_with("###")
            && line[2..].starts_with(|c: char| c.is_whitespace());
        if boundary && !current.is_empty() {
            out.push(section(heading.take(), std::mem::take(&mut current), cap, tokens));
        }
        if boundary {
            heading = Some(line.trim_start_matches('#').trim().to_string());
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        out.push(section(heading, current, cap, tokens));
    }
    out
}

fn section(
    heading: Option<String>,
    text: String,
    cap: usize,
    tokens: &dyn Fn(&str) -> usize,
) -> Section {
    let too_large = tokens(&text) > cap;
    Section {
        heading,
        text,
        too_large,
    }
}
