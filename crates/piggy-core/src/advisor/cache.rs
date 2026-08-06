//! Where a finished advice pass lives until the facts move.
//!
//! **Memory only, and that is a decision rather than an omission.** A draft is
//! derived from a CLAUDE.md's contents, and the spec is explicit that contents
//! are read at call time and never stored in the database. So the overlay lives
//! in this process and nowhere else, and an app restart legitimately re-runs the
//! pass. The alternative was a schema bump and a migration in a milestone that
//! was not scoped for one, to save a minute of background generation.
//!
//! What *does* persist is provenance: [`crate::Store::set_advice_facts_hash`]
//! stamps which fact sheet produced a candidate. That is a hash, not content.
//!
//! Not feature-gated, so the key derivations and the eviction rule are testable
//! in the default build.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use super::guard::Suggestion;

/// Domain separators, so no two of Piggy's sha256 uses can collide.
const ADVICE_DOMAIN: &[u8] = b"piggy/advice-llm/v1\n";
const DRAFT_DOMAIN: &[u8] = b"piggy/advice-draft/v1\n";

/// Hex characters kept from each key, matching [`super::facts::Facts::hash`].
const KEY_HEX_LEN: usize = 16;

/// The cache key for one model's reading of one fact sheet.
///
/// The ranking is a function of the facts **and** of which model wrote it: the
/// same sheet through Qwen and through Gemma is two different sets of sentences,
/// and a cache that could not tell them apart would serve one model's prose
/// under the other's name.
pub fn advice_key(model_id: &str, facts_hash: &str) -> String {
    let mut h = Sha256::new();
    h.update(ADVICE_DOMAIN);
    h.update(model_id.as_bytes());
    h.update([0]);
    h.update(facts_hash.as_bytes());
    truncate(h)
}

/// The cache key for one drafted file.
///
/// A draft is a function of the model and of the **file**, not of the fact
/// sheet: the whole ledger can move without changing a line of the file being
/// trimmed, and re-drafting it would be minutes of generation for a
/// byte-identical answer. `source_hash` is
/// [`crate::advice::Candidate::source_hash`], which for the content kinds is the
/// file's sha256 as it sits on disk, so a file that changed underneath gets a
/// new key rather than a stale draft.
pub fn draft_key(model_id: &str, candidate_id: &str, source_hash: &str) -> String {
    let mut h = Sha256::new();
    h.update(DRAFT_DOMAIN);
    h.update(model_id.as_bytes());
    h.update([0]);
    h.update(candidate_id.as_bytes());
    h.update([0]);
    h.update(source_hash.as_bytes());
    truncate(h)
}

/// Field separators above are NUL bytes, which none of the inputs can contain,
/// so no two different inputs hash the same string.
fn truncate(h: Sha256) -> String {
    let hex = format!("{:x}", h.finalize());
    hex[..KEY_HEX_LEN].to_string()
}

/// One drafted file, with what it takes to write it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// The candidate this rewrites. Carried because [`draft_key`] is a hash and
    /// the UI needs to know which cards have a diff to open.
    pub candidate_id: String,
    /// The replacement text, BOM excluded and line endings already matched to
    /// the source.
    pub text: String,
    /// Whether the file on disk began with a byte order mark.
    /// [`crate::advice::attach_draft`] puts it back; the guard never sees it.
    pub had_bom: bool,
}

/// What one finished pass produced.
#[derive(Debug, Clone, Default)]
pub struct AdviceOverlay {
    /// The fact sheet this was computed from.
    pub facts_hash: String,
    /// The model that wrote it. Shown in the UI: text generated locally by a 4B
    /// must never look like it came from the same place as the receipt.
    pub model_id: String,
    pub suggestion: Suggestion,
    /// [`draft_key`] -> the validated replacement.
    pub drafts: BTreeMap<String, Draft>,
}

impl AdviceOverlay {
    /// This overlay's own key.
    pub fn key(&self) -> String {
        advice_key(&self.model_id, &self.facts_hash)
    }

    /// The candidates that have a drafted rewrite waiting, in id order.
    pub fn drafted_candidates(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .drafts
            .values()
            .map(|d| d.candidate_id.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// One entry deep.
///
/// The overlay is per fact sheet, the facts move whenever the ledger does, and
/// holding a history of superseded advice would be holding derived file contents
/// in memory for no reader.
#[derive(Debug, Default)]
pub struct AdviceCache {
    current: Option<AdviceOverlay>,
}

impl AdviceCache {
    /// The overlay for `key`, or `None`.
    pub fn get(&self, key: &str) -> Option<&AdviceOverlay> {
        self.current.as_ref().filter(|o| o.key() == key)
    }

    /// Whatever overlay is held, whichever fact sheet it came from.
    ///
    /// For the drafts, and only for them. A draft's own [`draft_key`] carries
    /// the model, the candidate and the file's hash, which is the whole of what
    /// makes a draft stale: the ledger can move under it without changing a byte
    /// of the file. Matching on the facts hash as well would throw away a
    /// perfectly good draft because a session landed between opening the sheet
    /// and pressing Apply.
    pub fn current(&self) -> Option<&AdviceOverlay> {
        self.current.as_ref()
    }

    /// Store an overlay, evicting whatever was there.
    pub fn put(&mut self, overlay: AdviceOverlay) {
        self.current = Some(overlay);
    }

    /// Drop the overlay. This is the whole of "Refresh advice": the next read
    /// misses, and the pass runs again.
    pub fn clear(&mut self) {
        self.current = None;
    }
}
