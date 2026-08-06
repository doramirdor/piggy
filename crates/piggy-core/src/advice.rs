//! The advice engine: deterministic candidate actions, applied reversibly.
//!
//! Piggy already knows what a session costs ([`crate::ledger`]), what an MCP
//! server's schemas weigh ([`crate::probe`]), what is in the CLAUDE.md stack
//! ([`crate::claudemd`]), what is installed and unused ([`crate::sweep`]), and
//! what each saver actually did ([`crate::attribution`]). None of that was
//! actionable in one place. This module turns all five into one list of
//! **candidates**: a plain-language claim, the numbers behind it with the label
//! that says how each was arrived at, and a reversible apply.
//!
//! The rules it lives by, all inherited:
//!
//! * **Pure code proposes; nothing here is a model.** Every generator is a
//!   function over the database, the configs, the registry and the attribution
//!   tables. M5.4's advisor may re-rank this list and draft the one piece of
//!   prose ([`ActionKind::ClaudemdTrim`]) that a human has to read as prose - it
//!   may not add a candidate, a target, or a number.
//! * **Every figure carries its basis** ([`EvidenceRow::basis`]). A probe
//!   measurement is never shown as a guess, an A/B `estimated` is never shown as
//!   `measured`, and a session count that was counted says so.
//! * **Nothing is applied that cannot be undone.** Each kind's apply records
//!   exactly what it needs to put the world back: the sweep snapshot, the
//!   before-JSON of both scopes, the file's original bytes, or the saver's prior
//!   state. Undo reports per item, so one unwritable file never hides the rest.
//!
//! Lifecycle is the store's (`open -> applied | dismissed | stale`). [`generate`]
//! owns the transitions that are *derived*: a candidate that no longer
//! regenerates goes stale, and a dismissal stops suppressing once the evidence
//! doubles.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::attribution::{self, Badge, Reading, SaverAttribution, Stream, StreamStat};
use crate::claudemd::{self, ClaudemdReport, FindingKind, ProjectMcpServers};
use crate::config;
use crate::engine;
use crate::insights::commas;
use crate::pricing::Pricing;
use crate::probe::{self, ConfiguredServer, Transport};
use crate::registry::{Catalog, Entry};
use crate::snapshots::{self, FileSnapshot};
use crate::state::PiggyState;
use crate::stats::{Period, Totals};
use crate::store::{advice_status, AdviceRow, McpManifest, Store};
use crate::sweep::{self, SweepReport};

/// Domain separator so a candidate id can never collide with another sha256 use
/// in this crate ([`probe`]'s config hash, [`crate::settings::hash_bytes`]).
const ID_DOMAIN: &[u8] = b"piggy/advice/v1\n";

/// Hex characters kept from a candidate's sha256. Sixteen is 64 bits: at the few
/// hundred candidates a busy machine can produce, a collision is not a thing
/// that happens, and a short id is one a person can read off the CLI and paste
/// back.
const ID_HEX_LEN: usize = 16;

/// Most projects a [`ActionKind::ServerScope`] candidate will pin a server to.
/// Past two, "it belongs to these projects" stops being true and the honest
/// answer is that the server is general and belongs at user scope.
pub const MAX_SCOPE_PROJECTS: usize = 2;

/// Randomized sessions required **per side** before Piggy will suggest turning a
/// behaviour-changing saver off for having done nothing.
///
/// Three times [`attribution::MIN_GROUP`], which is the bar for showing a number
/// at all. Telling someone to drop a saver they chose is a stronger claim than
/// reporting a percentage, so it takes more evidence, and it takes *randomized*
/// evidence: see [`SaverAttribution::randomized_counts`].
pub const MIN_RANDOMIZED_PER_SIDE: usize = 30;

/// How far a dismissed target's estimated cost has to move before the same
/// suggestion comes back. "Roughly doubles", from the spec: anything smaller and
/// "Not for me" would not stick.
pub const REOPEN_MULTIPLIER: i64 = 2;

/// Bootstrap seed for the attribution [`saver_mix`] reads.
///
/// Fixed rather than clock-derived on purpose: the spec's determinism rule is
/// "same facts, same advice", and the confidence intervals are resampled - a
/// per-run seed would move an interval enough to flip a [`Reading`] and with it
/// whether a suggestion exists at all.
pub const ATTRIBUTION_SEED: u64 = 0x5049_4747_594d_3533;

/// A UTF-8 byte order mark. [`claudemd::FileText::text`] has it stripped, so a
/// rewrite of a file that had one has to put it back or the edit silently
/// changes the first three bytes.
const BOM: char = '\u{FEFF}';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The five candidate families, in three groups: server placement, CLAUDE.md
/// cleanup, and the saver mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    /// An add-on nothing has used: turn it off. Covers every kind
    /// [`crate::sweep`] recommends disabling - MCP server, plugin or skill -
    /// because they are one decision with one undo, not three.
    ServerDisable,
    /// A user-scope MCP server whose calls come from one or two projects: pin it
    /// to those, so every other session stops loading its schemas.
    ServerScope,
    /// The deterministic CLAUDE.md edit: dead-reference lines removed, blocks
    /// duplicated from a file that is already loaded alongside dropped.
    ClaudemdFix,
    /// An oversized CLAUDE.md worth rewriting. The rewrite itself is prose, so
    /// it needs the advisor ([`Prerequisite::NeedsAdvisor`]); this milestone
    /// generates the case for it, not the draft.
    ClaudemdTrim,
    /// Turn a saver off (it changes behaviour and has measurably done nothing)
    /// or on (it is installed, off, and measured favourable).
    SaverMix,
}

impl ActionKind {
    /// Stable machine name, matching the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            ActionKind::ServerDisable => "server-disable",
            ActionKind::ServerScope => "server-scope",
            ActionKind::ClaudemdFix => "claudemd-fix",
            ActionKind::ClaudemdTrim => "claudemd-trim",
            ActionKind::SaverMix => "saver-mix",
        }
    }

    /// Parse a stored `advice.kind` back.
    pub fn parse(s: &str) -> Option<ActionKind> {
        match s {
            "server-disable" => Some(ActionKind::ServerDisable),
            "server-scope" => Some(ActionKind::ServerScope),
            "claudemd-fix" => Some(ActionKind::ClaudemdFix),
            "claudemd-trim" => Some(ActionKind::ClaudemdTrim),
            "saver-mix" => Some(ActionKind::SaverMix),
            _ => None,
        }
    }

    /// Group heading for a listing, in the registry's plain voice.
    pub fn group_label(&self) -> &'static str {
        match self {
            ActionKind::ServerDisable | ActionKind::ServerScope => "Add-ons",
            ActionKind::ClaudemdFix | ActionKind::ClaudemdTrim => "CLAUDE.md",
            ActionKind::SaverMix => "Savers",
        }
    }

    /// Whether this kind edits file *content* (as opposed to config or a
    /// toggle). The content kinds are the ones gated on a source hash.
    pub fn edits_content(&self) -> bool {
        matches!(self, ActionKind::ClaudemdFix | ActionKind::ClaudemdTrim)
    }
}

/// How a number on an evidence row was arrived at. The label travels with the
/// value so no surface can show a guess as a measurement, or the reverse.
pub mod basis {
    /// Counted in the session database. A fact about what happened, not a
    /// projection: session counts, tool-call counts.
    pub const OBSERVED: &str = "observed";
    /// Derived through a divisor or a size heuristic (bytes / 3.5, config size),
    /// or an observed number multiplied by one of those.
    pub const ESTIMATED: &str = "estimated";
    /// A [`crate::probe`] measurement of this server's *current* config: the
    /// schema bytes are real. What the client charges for them is still an
    /// estimate, which is why the monthly figure derived from it is not this.
    pub const MEASURED_MANIFEST: &str = "measured manifest";
    /// A randomized A/B result ([`super::Badge::Measured`]).
    pub const MEASURED: &str = "measured";
    /// An A/B figure resting on an observational baseline
    /// ([`super::Badge::Estimated`]) - a number, but not a randomized one.
    pub const ESTIMATED_AB: &str = "estimated (observational)";
    /// Compared, with no number to show yet ([`super::Badge::Measuring`]).
    pub const MEASURING: &str = "not enough data yet";
}

/// The basis label for an A/B badge.
fn badge_basis(badge: Badge) -> &'static str {
    match badge {
        Badge::Measured => basis::MEASURED,
        Badge::Estimated => basis::ESTIMATED_AB,
        Badge::Measuring => basis::MEASURING,
    }
}

/// One line of the evidence table: what was counted, what it came to, and how it
/// was arrived at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRow {
    /// What this number is, in the user's terms.
    pub label: String,
    /// The number, already formatted (thousands separators, units, `~` for an
    /// estimate) so every surface renders it the same way.
    pub value: String,
    /// One of [`basis`].
    pub basis: String,
}

impl EvidenceRow {
    fn new(label: impl Into<String>, value: impl Into<String>, basis: &str) -> EvidenceRow {
        EvidenceRow {
            label: label.into(),
            value: value.into(),
            basis: basis.to_string(),
        }
    }
}

/// Something that has to be true before a candidate can be applied at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Prerequisite {
    /// The local advisor has to be downloaded and on: this candidate's action is
    /// a piece of writing, and Piggy will not author guidance with a heuristic.
    NeedsAdvisor,
}

impl Prerequisite {
    pub fn as_str(&self) -> &'static str {
        match self {
            Prerequisite::NeedsAdvisor => "needs-advisor",
        }
    }

    /// What the user has to do about it.
    pub fn note(&self) -> &'static str {
        match self {
            Prerequisite::NeedsAdvisor => {
                "turn on the local advisor in Settings for a drafted rewrite"
            }
        }
    }
}

/// A reversible toggle: nothing on disk changes shape, and Undo is the same
/// switch the other way.
pub const RISK_TOGGLE: u8 = 1;
/// A config move: one entry changes place inside a file Piggy already writes,
/// with the before-JSON of both ends snapshotted.
pub const RISK_CONFIG_MOVE: u8 = 2;
/// A content edit: prose the user wrote changes. Always diff-reviewed, always
/// byte-snapshotted first.
pub const RISK_CONTENT_EDIT: u8 = 3;

/// Everything apply and undo need that is not evidence, per kind.
///
/// Externally tagged (serde's default) rather than internally tagged: with
/// `serde_json`'s `arbitrary_precision`, an internally tagged enum cannot
/// round-trip a [`Value`] carrying numbers, and [`Params::ServerScope`] carries
/// the server's config verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Params {
    /// One [`crate::sweep`] row, addressed the way sweep addresses it.
    ServerDisable {
        /// `"mcp"`, `"plugin"` or `"skill"`.
        item_kind: String,
        id: String,
        /// The `~/.claude.json` project the server sits under, or `None` for
        /// user scope (for plugins/skills, sweep's own `source`).
        source: Option<String>,
        /// The look-back window the recommendation was computed over, so apply
        /// re-scans with the same one.
        n_sessions: usize,
    },
    ServerScope {
        server: String,
        /// The projects the server moves into, in path order.
        projects: Vec<String>,
        /// The exact config object being moved.
        config: Value,
    },
    /// Both CLAUDE.md kinds: the file is the whole address.
    Claudemd { path: String },
    SaverMix {
        saver: String,
        /// True to turn it on, false to turn it off.
        turn_on: bool,
    },
}

/// One MCP server the advice engine moved between scopes, with the exact
/// before-JSON of both ends.
///
/// This is the m2 rule's one documented exception (spec: "Supersedes m2-spec's
/// 'will not move config it did not write' for `ServerScope` only"): the entry
/// stays inside the same `~/.claude.json`, secrets never move file, and both
/// ends are recorded here so Undo is exact rather than reconstructed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeMove {
    /// The advice row this move belongs to.
    pub advice_id: String,
    /// The `mcpServers` key that moved.
    pub server: String,
    /// The top-level entry before the move (`null` if there was none).
    #[serde(default)]
    pub before_user: Value,
    /// Project path -> that project's entry before the move (`null` = absent, so
    /// Undo removes the key rather than writing a null).
    #[serde(default)]
    pub before_projects: BTreeMap<String, Value>,
    /// Containers this move had to create, so Undo can take them away again and
    /// leave the file structurally as it started. Each is only removed when it
    /// comes back empty: if the user has since put something of their own in
    /// one, it is theirs.
    #[serde(default)]
    pub created: CreatedContainers,
    pub moved_at: String,
}

/// The empty scaffolding a [`ScopeMove`] had to write before it could put a
/// server in a project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreatedContainers {
    /// True when `~/.claude.json` had no top-level `projects` object at all.
    #[serde(default)]
    pub projects_root: bool,
    /// Projects whose whole `projects.<path>` entry was created.
    #[serde(default)]
    pub entries: Vec<String>,
    /// Projects that had an entry but no `mcpServers` map in it.
    #[serde(default)]
    pub maps: Vec<String>,
}

/// One proposed action: pure data, computed by pure code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    /// Stable sha256 over kind, target, [`Self::fingerprint`] and every evidence
    /// row, truncated and prefixed with the kind. Same inputs, same id; any
    /// number on the card moving produces a different one.
    pub id: String,
    pub kind: ActionKind,
    /// What this is about, in a form a person reads: a server and its scope, a
    /// file path, a saver id. Display and dedupe only - never parsed. The
    /// structured address is [`Self::params`].
    pub target: String,
    /// The claim, in the registry `plainLabel` voice.
    pub title: String,
    pub evidence: Vec<EvidenceRow>,
    /// Estimated tokens a month this action is worth.
    ///
    /// A saving for every kind except [`ActionKind::ClaudemdTrim`], where it is
    /// the file's monthly *burden* - the ceiling on what a rewrite could save,
    /// since how much a rewrite removes is not known until it is drafted. The
    /// evidence row says which.
    pub est_tokens_month: i64,
    /// [`RISK_TOGGLE`], [`RISK_CONFIG_MOVE`] or [`RISK_CONTENT_EDIT`].
    pub risk_tier: u8,
    pub prerequisites: Vec<Prerequisite>,
    /// What this plan was computed against: the file's content hash for the
    /// CLAUDE.md kinds, the server's `config_hash` for the server kinds, the
    /// saver's current state for [`ActionKind::SaverMix`]. Apply re-checks it,
    /// so a target that moved underneath a candidate is refused rather than
    /// overwritten. See [`Self::source_hash`].
    pub fingerprint: String,
    pub params: Params,
    /// The transform result for a content kind: the file exactly as it would be
    /// written, BOM and line endings included.
    ///
    /// **Never serialized.** CLAUDE.md contents are read at call time and never
    /// stored (spec: "Contents are never stored in the DB"), so this lives in
    /// memory for the length of one generate-then-apply and nowhere else. `None`
    /// for [`ActionKind::ClaudemdTrim`] until M5.4 drafts one.
    #[serde(skip)]
    pub new_content: Option<String>,
    /// The row's lifecycle status, filled in by [`generate`] from the store.
    /// Not part of the payload: the column is the truth, and a copy would rot.
    #[serde(skip)]
    pub status: String,
}

impl Candidate {
    /// The content hash this candidate was computed against, for the kinds that
    /// edit content. `None` for the others, whose fingerprint is a config hash.
    pub fn source_hash(&self) -> Option<&str> {
        self.kind
            .edits_content()
            .then_some(self.fingerprint.as_str())
    }

    /// Whether anything blocks applying this right now: a drafting candidate
    /// with no draft in it yet.
    pub fn blocked(&self) -> bool {
        self.prerequisites
            .iter()
            .any(|p| matches!(p, Prerequisite::NeedsAdvisor) && self.new_content.is_none())
    }

    /// This candidate as a fresh `open` row.
    fn row(&self, created_at: &str) -> Result<AdviceRow> {
        Ok(AdviceRow {
            id: self.id.clone(),
            kind: self.kind.as_str().to_string(),
            target: self.target.clone(),
            created_at: created_at.to_string(),
            // M5.4 owns the facts payload and its hash; a value invented here
            // would claim a provenance this milestone does not have.
            facts_hash: None,
            est_tokens_month: self.est_tokens_month,
            status: advice_status::OPEN.to_string(),
            payload_json: Some(serde_json::to_string(self)?),
            applied_at: None,
            restore_ref: None,
            dismiss_note: None,
        })
    }

    /// Rebuild a candidate from its stored row. Undo runs long after the world
    /// stopped regenerating the candidate, so it reads the payload rather than
    /// asking the generators for it again.
    pub fn from_row(row: &AdviceRow) -> Result<Candidate> {
        let payload = row
            .payload_json
            .as_deref()
            .ok_or_else(|| anyhow!("advice '{}' has no stored payload", row.id))?;
        let mut candidate: Candidate = serde_json::from_str(payload)
            .with_context(|| format!("parsing the payload of advice '{}'", row.id))?;
        candidate.status = row.status.clone();
        Ok(candidate)
    }
}

/// What a dismissal recorded, so a later regeneration can tell "the same
/// suggestion again" from "the same suggestion, twice as expensive".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DismissNote {
    /// The user's own words, when they gave any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `est_tokens_month` at the moment of dismissal: the baseline
    /// [`REOPEN_MULTIPLIER`] is measured against. `None` for a note Piggy did
    /// not write, which never auto-reopens - a "no" with no baseline stays a no.
    #[serde(default)]
    pub est_tokens_month: Option<i64>,
}

impl DismissNote {
    /// Parse a stored `dismiss_note`, tolerating anything that is not ours.
    pub fn parse(raw: &str) -> DismissNote {
        serde_json::from_str(raw).unwrap_or(DismissNote {
            note: Some(raw.to_string()),
            est_tokens_month: None,
        })
    }

    /// Whether `est_tokens_month` has moved enough to bring the suggestion back.
    fn reopens_at(&self, est_tokens_month: i64) -> bool {
        match self.est_tokens_month {
            // Strictly greater as well as doubled, so a baseline of zero is not
            // met by another zero.
            Some(base) => {
                est_tokens_month > base
                    && est_tokens_month >= base.saturating_mul(REOPEN_MULTIPLIER)
            }
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/// Everything [`generate`] needs from outside the database.
pub struct GenerateOptions<'a> {
    pub catalog: &'a Catalog,
    pub pricing: &'a Pricing,
    /// Piggy's ledger, for which savers are installed and on.
    pub state: &'a PiggyState,
    /// Look-back window for usage cross-reference. [`sweep::DEFAULT_N_SESSIONS`]
    /// unless a caller widens it.
    pub n_sessions: usize,
}

impl<'a> GenerateOptions<'a> {
    pub fn new(catalog: &'a Catalog, pricing: &'a Pricing, state: &'a PiggyState) -> Self {
        GenerateOptions {
            catalog,
            pricing,
            state,
            n_sessions: sweep::DEFAULT_N_SESSIONS,
        }
    }
}

/// One installed saver as [`saver_mix`] sees it.
pub struct SaverInput {
    pub entry: Entry,
    /// Whether the saver is currently on.
    pub enabled: bool,
    pub attribution: SaverAttribution,
}

/// Everything the generators read, loaded once. Each generator is a pure
/// function of this, so a fixture can drive one with no database in sight.
pub struct Inputs {
    /// Sweep's scan: the unused-add-on analysis, unchanged.
    pub sweep: SweepReport,
    /// Every server `~/.claude.json` configures, user scope first.
    pub servers: Vec<ConfiguredServer>,
    /// Probe measurements, for the servers that have one.
    pub manifests: Vec<McpManifest>,
    /// MCP calls per server (normalized name) per project over the window.
    pub server_usage: BTreeMap<String, BTreeMap<String, u64>>,
    /// Sessions in the last 30 days, per project and in total.
    pub sessions_30d: claudemd::SessionCounts,
    /// What each project checked into its own `.mcp.json` (read-only).
    pub project_mcp: ProjectMcpServers,
    /// The CLAUDE.md inventory and its findings.
    pub claudemd: ClaudemdReport,
    /// The contents of the files a CLAUDE.md candidate will transform, re-read
    /// at generation time so the hash a candidate carries is the hash of the
    /// bytes it was computed from.
    pub texts: Vec<claudemd::FileText>,
    pub savers: Vec<SaverInput>,
    /// Tokens actually spent in the last 30 days, per stream. The observed half
    /// of a [`ActionKind::SaverMix`] "turn it on" figure.
    pub tokens_30d: Totals,
    pub n_sessions: usize,
}

/// Generate every candidate, reconcile the advice table, and hand back the list.
///
/// Three things happen to the table, all derived and none of them a user action:
///
/// * every new candidate is inserted `open` (existing rows keep their history -
///   [`Store::insert_advice`] never overwrites);
/// * an `open` row that no longer regenerates goes `stale`: its evidence moved,
///   so its plan describes a world that is gone, and applying it would be
///   applying a stale plan;
/// * a `dismissed` target whose cost has since doubled comes back, and the
///   dismissal that suppressed it is retired so it cannot suppress twice.
pub fn generate(store: &mut Store, opts: &GenerateOptions) -> Result<Vec<Candidate>> {
    let inputs = load_inputs(store, opts)?;
    let mut candidates = generate_from(&inputs);

    // Biggest saving first, ties on id: the same facts must produce the same
    // list in the same order, because the UI shows the top few.
    candidates.sort_by(|a, b| {
        b.est_tokens_month
            .cmp(&a.est_tokens_month)
            .then_with(|| a.id.cmp(&b.id))
    });

    // Computed before dismissal filtering: a suggestion the user waved away is
    // still a suggestion the world supports, so it must not retire its own row.
    let live: BTreeSet<String> = candidates.iter().map(|c| c.id.clone()).collect();
    for row in store.advice_by_status(advice_status::OPEN)? {
        if !live.contains(&row.id) {
            store.set_advice_status(&row.id, advice_status::STALE, None, None, None)?;
        }
    }

    let dismissed = store.advice_by_status(advice_status::DISMISSED)?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut out = Vec::new();
    for mut candidate in candidates {
        // Every dismissal of this target, not just the newest: the highest
        // baseline governs, so waving the same thing away twice does not lower
        // the bar for it coming back.
        let suppressing: Vec<&AdviceRow> = dismissed
            .iter()
            .filter(|r| r.kind == candidate.kind.as_str() && r.target == candidate.target)
            .collect();
        if !suppressing.is_empty() {
            let reopens = suppressing.iter().all(|r| {
                r.dismiss_note
                    .as_deref()
                    .map(DismissNote::parse)
                    .is_some_and(|d| d.reopens_at(candidate.est_tokens_month))
            });
            if !reopens {
                continue;
            }
            // Spent: retire it so the same dismissal cannot suppress a third
            // time from a baseline the user has already been shown past.
            for row in &suppressing {
                store.set_advice_status(&row.id, advice_status::STALE, None, None, None)?;
            }
        }
        store.insert_advice(&candidate.row(&now)?)?;
        candidate.status = store
            .advice(&candidate.id)?
            .map(|r| r.status)
            .unwrap_or_else(|| advice_status::OPEN.to_string());
        out.push(candidate);
    }
    Ok(out)
}

/// Every generator, over already-loaded inputs. Pure.
pub fn generate_from(inputs: &Inputs) -> Vec<Candidate> {
    let mut out = Vec::new();
    out.extend(server_disable(inputs));
    out.extend(server_scope(inputs));
    out.extend(claudemd_fix(inputs));
    out.extend(claudemd_trim(inputs));
    out.extend(saver_mix(inputs));
    out
}

/// Read everything the generators need. The one impure half of [`generate`].
pub fn load_inputs(store: &mut Store, opts: &GenerateOptions) -> Result<Inputs> {
    let sweep_report = sweep::scan(store, opts.n_sessions)?;
    let servers = probe::configured_servers()?;
    let manifests = store.mcp_manifests()?;
    let sessions_30d = store.session_counts_since(Period::Month.cutoff().as_deref())?;
    let project_mcp = claudemd::project_mcp_servers(store)?;
    let tokens_30d = store.totals(Period::Month)?;

    // Tool calls folded from `mcp__<server>__<tool>` down to the server, per
    // project, exactly as sweep folds them - one usage model, not two.
    let mut server_usage: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for (tool, by_project) in store.recent_tool_usage(opts.n_sessions)? {
        let Some(server) = sweep::mcp_server_of(&tool) else {
            continue;
        };
        let entry = server_usage.entry(sweep::normalize(server)).or_default();
        for (project, n) in by_project {
            *entry.entry(project).or_insert(0) += n;
        }
    }

    let claudemd_report = claudemd::scan(store)?;
    // Only the files something might be removed from are read back. The scan
    // dropped their contents (it stores sizes and hashes), and a transform has
    // to be computed against bytes we hold.
    let mut texts = Vec::new();
    for file in &claudemd_report.files {
        let transformable = file.findings.iter().any(|f| {
            matches!(
                f.kind,
                FindingKind::DeadRef { .. } | FindingKind::DuplicateBlock { .. }
            )
        });
        if !transformable {
            continue;
        }
        if let Ok(t) =
            claudemd::read_file_text(Path::new(&file.file.path), file.file.project.clone())
        {
            texts.push(t);
        }
    }

    // Attribution for every installed saver the catalog still knows. The rate
    // map is a full-table scan, so it is built once for all of them.
    let rate_map = store.session_rate_map(opts.pricing)?;
    let mut savers = Vec::new();
    for (id, saver) in &opts.state.savers {
        let Some(entry) = opts.catalog.get(id) else {
            continue;
        };
        let attribution = attribution::attribute_with_map(store, &rate_map, id, ATTRIBUTION_SEED)?;
        savers.push(SaverInput {
            entry: entry.clone(),
            enabled: saver.enabled,
            attribution,
        });
    }

    Ok(Inputs {
        sweep: sweep_report,
        servers,
        manifests,
        server_usage,
        sessions_30d,
        project_mcp,
        claudemd: claudemd_report,
        texts,
        savers,
        tokens_30d,
        n_sessions: opts.n_sessions,
    })
}

// ---------------------------------------------------------------------------
// Generator: ServerDisable
// ---------------------------------------------------------------------------

/// Sweep's unused add-ons, as candidates.
///
/// Sweep stays the implementation - this is the front door. The one judgement
/// added here is the `.mcp.json` guard the spec asks for: a server a project
/// checked into its own repo is configured deliberately, and Piggy will not call
/// it unused on the strength of a session window that may not have visited that
/// project.
pub fn server_disable(inputs: &Inputs) -> Vec<Candidate> {
    let project_configured: BTreeSet<&str> = inputs
        .project_mcp
        .by_project
        .values()
        .flat_map(|names| names.iter().map(String::as_str))
        .collect();

    let mut out = Vec::new();
    for item in inputs.sweep.recommended() {
        if item.kind == "mcp" && project_configured.contains(item.id.as_str()) {
            continue;
        }
        // Sessions that pay for this add-on every time: a project-scoped server
        // costs that project's sessions, everything else costs all of them.
        let sessions = match (item.kind.as_str(), item.source.as_deref()) {
            ("mcp", Some(project)) => inputs
                .sessions_30d
                .by_project
                .get(project)
                .copied()
                .unwrap_or(0),
            _ => inputs.sessions_30d.total,
        };
        let est_tokens_month = item.est_tokens as i64 * sessions;
        let measured = item.cost_basis == sweep::COST_BASIS_MEASURED;

        let noun = match item.kind.as_str() {
            "mcp" => "server",
            other => other,
        };
        let title = format!("Turn off the {} {noun}", item.id);
        let target = match (item.kind.as_str(), item.source.as_deref()) {
            ("mcp", source) => server_target(&item.id, source),
            _ => format!("{}:{}", item.kind, item.id),
        };
        let evidence = vec![
            EvidenceRow::new(
                if item.used_windowed {
                    format!(
                        "Uses in the last {} sessions",
                        inputs.sweep.sessions_considered
                    )
                } else {
                    "Uses, ever".to_string()
                },
                commas(item.used),
                basis::OBSERVED,
            ),
            EvidenceRow::new(
                "Context cost per session",
                format!(
                    "{}{} tokens",
                    if measured { "" } else { "~" },
                    commas(item.est_tokens)
                ),
                if measured {
                    basis::MEASURED_MANIFEST
                } else {
                    basis::ESTIMATED
                },
            ),
            EvidenceRow::new(
                "Sessions in the last 30 days that load it",
                commas(sessions.max(0) as u64),
                basis::OBSERVED,
            ),
            EvidenceRow::new(
                "Tokens a month it costs you",
                format!("~{}", commas(est_tokens_month.max(0) as u64)),
                basis::ESTIMATED,
            ),
        ];
        let params = Params::ServerDisable {
            item_kind: item.kind.clone(),
            id: item.id.clone(),
            source: item.source.clone(),
            n_sessions: inputs.n_sessions,
        };
        // An MCP server's plan is invalidated by its command changing; a plugin
        // or a skill has no such fingerprint, and for those "does it still
        // regenerate" is the whole staleness test.
        let fingerprint = match item.kind.as_str() {
            "mcp" => configured_server(inputs, &item.id, item.source.as_deref())
                .map(|s| s.config_hash())
                .unwrap_or_default(),
            _ => String::new(),
        };
        out.push(build(
            ActionKind::ServerDisable,
            target,
            title,
            evidence,
            est_tokens_month,
            RISK_TOGGLE,
            Vec::new(),
            fingerprint,
            params,
            None,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Generator: ServerScope
// ---------------------------------------------------------------------------

/// User-scope MCP servers that only one or two projects actually call.
///
/// A user-scope server is loaded by every session, so its schemas are paid for
/// by every project whether or not that project has ever called it. When the
/// calls concentrate, the fix is not to remove the server - the user wants it -
/// but to move it to the projects that use it.
pub fn server_scope(inputs: &Inputs) -> Vec<Candidate> {
    let mut out = Vec::new();
    for server in inputs
        .servers
        .iter()
        .filter(|s| s.project.is_none() && s.transport == Transport::Stdio)
    {
        let Some(raw) = inputs.server_usage.get(&sweep::normalize(&server.key)) else {
            continue; // never called: that is `ServerDisable`'s question, not this one
        };
        let by_project = sweep::fold_subpaths(raw);
        let total: u64 = by_project.values().sum();
        if total == 0 {
            continue;
        }
        // Sessions with no recorded project cannot say where a server belongs.
        // They count toward `total` so that a server used mostly from unknown
        // directories fails the concentration test rather than being pinned on
        // the strength of the few calls we can place.
        let named: BTreeMap<&str, u64> = by_project
            .iter()
            .filter(|(project, _)| !project.is_empty())
            .map(|(project, n)| (project.as_str(), *n))
            .collect();
        let named_total: u64 = named.values().sum();
        if (named_total as f64) < total as f64 * sweep::SCOPE_CONCENTRATION {
            continue;
        }
        // A project that checked this server into its own `.mcp.json` already
        // has its own copy: its calls came from that one, and pinning a second
        // entry there would duplicate config Piggy is not allowed to write.
        let projects: Vec<String> = named
            .keys()
            .filter(|project| {
                !inputs
                    .project_mcp
                    .by_project
                    .get(**project)
                    .is_some_and(|names| names.iter().any(|n| n == &server.key))
            })
            .map(|p| p.to_string())
            .collect();
        if projects.is_empty() || projects.len() > MAX_SCOPE_PROJECTS {
            continue;
        }

        let pinned_sessions: i64 = projects
            .iter()
            .map(|p| inputs.sessions_30d.by_project.get(p).copied().unwrap_or(0))
            .sum();
        // Every session that is not one of those stops loading the schemas.
        let freed_sessions = inputs.sessions_30d.total - pinned_sessions;
        if freed_sessions <= 0 {
            continue;
        }
        let (tokens, measured) = match probe::measured_tokens(&inputs.manifests, server) {
            Some(t) => (t.max(0) as u64, true),
            None => (sweep::est_mcp_tokens(&server.config), false),
        };
        let est_tokens_month = tokens as i64 * freed_sessions;

        let mut evidence = Vec::new();
        for (project, n) in &named {
            evidence.push(EvidenceRow::new(
                format!("Calls from {}", short_project(project)),
                commas(*n),
                basis::OBSERVED,
            ));
        }
        let unplaced = total - named_total;
        if unplaced > 0 {
            evidence.push(EvidenceRow::new(
                "Calls from a session with no recorded project",
                commas(unplaced),
                basis::OBSERVED,
            ));
        }
        evidence.push(EvidenceRow::new(
            "Tool schemas it loads",
            format!(
                "{}{} tokens",
                if measured { "" } else { "~" },
                commas(tokens)
            ),
            if measured {
                basis::MEASURED_MANIFEST
            } else {
                basis::ESTIMATED
            },
        ));
        evidence.push(EvidenceRow::new(
            "Sessions a month that would stop loading it",
            commas(freed_sessions.max(0) as u64),
            basis::OBSERVED,
        ));
        evidence.push(EvidenceRow::new(
            "Tokens a month that buys back",
            format!("~{}", commas(est_tokens_month.max(0) as u64)),
            basis::ESTIMATED,
        ));

        let title = match projects.as_slice() {
            [one] => format!("Pin the {} server to {}", server.key, short_project(one)),
            many => format!("Pin the {} server to {} projects", server.key, many.len()),
        };
        out.push(build(
            ActionKind::ServerScope,
            server_target(&server.key, None),
            title,
            evidence,
            est_tokens_month,
            RISK_CONFIG_MOVE,
            Vec::new(),
            server.config_hash(),
            Params::ServerScope {
                server: server.key.clone(),
                projects,
                config: server.config.clone(),
            },
            None,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Generator: ClaudemdFix
// ---------------------------------------------------------------------------

/// The deterministic CLAUDE.md edit, one candidate per file.
///
/// Two transforms, both mechanical and both computed here so the app can show a
/// diff before anything is written:
///
/// * a line carrying a reference that does not resolve is deleted;
/// * a paragraph that also sits in a file loaded *alongside* this one is deleted
///   from whichever copy is the redundant one (see [`duplicate_is_redundant`]).
///
/// One candidate per file rather than one per finding: both transforms are
/// computed against the same bytes, so applying them separately would leave the
/// second one stale the moment the first landed.
pub fn claudemd_fix(inputs: &Inputs) -> Vec<Candidate> {
    // Where each scanned file lives, for the duplicate rule (which copy of a
    // shared block is the one that is loaded anyway).
    let projects: BTreeMap<&str, Option<&str>> = inputs
        .claudemd
        .files
        .iter()
        .map(|f| (f.file.path.as_str(), f.file.project.as_deref()))
        .collect();

    let mut out = Vec::new();
    for scanned in &inputs.claudemd.files {
        let Some(text) = inputs
            .texts
            .iter()
            .find(|t| t.path.to_string_lossy() == scanned.file.path)
        else {
            continue;
        };
        let Some(edit) = claudemd_edit(text, &scanned.findings, &projects) else {
            continue;
        };
        let est_tokens = claudemd::est_tokens(edit.removed_bytes);
        let est_tokens_month = est_tokens * scanned.sessions_30d;
        let label = claudemd_label(&scanned.file.path, scanned.file.project.as_deref());

        let refs = plural(edit.dead_refs.len(), "dead reference", "dead references");
        let blocks = plural(edit.dup_blocks, "duplicated block", "duplicated blocks");
        let title = match (edit.dead_refs.len(), edit.dup_blocks) {
            (0, _) => format!("Drop {blocks} from {label}"),
            (_, 0) => format!("Drop {refs} from {label}"),
            _ => format!("Clean up {label}: {refs} and {blocks}"),
        };
        let mut evidence = Vec::new();
        if !edit.dead_refs.is_empty() {
            evidence.push(EvidenceRow::new(
                "References that point at nothing",
                list_briefly(&edit.dead_refs),
                basis::OBSERVED,
            ));
        }
        for label in &edit.dup_labels {
            evidence.push(EvidenceRow::new(
                "Block already loaded from",
                label.clone(),
                basis::OBSERVED,
            ));
        }
        evidence.push(EvidenceRow::new(
            "Lines removed",
            commas(edit.removed_lines as u64),
            basis::OBSERVED,
        ));
        evidence.push(EvidenceRow::new(
            "Bytes removed",
            commas(edit.removed_bytes.max(0) as u64),
            basis::OBSERVED,
        ));
        evidence.push(EvidenceRow::new(
            "Sessions in the last 30 days that load it",
            commas(scanned.sessions_30d.max(0) as u64),
            basis::OBSERVED,
        ));
        evidence.push(EvidenceRow::new(
            "Tokens a month it saves",
            format!("~{}", commas(est_tokens_month.max(0) as u64)),
            basis::ESTIMATED,
        ));

        out.push(build(
            ActionKind::ClaudemdFix,
            scanned.file.path.clone(),
            title,
            evidence,
            est_tokens_month,
            RISK_CONTENT_EDIT,
            Vec::new(),
            text.hash.clone(),
            Params::Claudemd {
                path: scanned.file.path.clone(),
            },
            Some(edit.content),
        ));
    }
    out
}

/// One file's deterministic edit.
struct ClaudemdEdit {
    /// The file exactly as it would be written, BOM and line endings included.
    content: String,
    /// Distinct references whose lines go, in file order.
    dead_refs: Vec<String>,
    /// How many duplicated paragraphs go.
    dup_blocks: usize,
    /// For each of those, where the surviving copy lives.
    dup_labels: Vec<String>,
    removed_lines: usize,
    removed_bytes: i64,
}

/// Compute the edit for one file, or `None` when there is nothing to remove.
fn claudemd_edit(
    text: &claudemd::FileText,
    findings: &[claudemd::Finding],
    projects: &BTreeMap<&str, Option<&str>>,
) -> Option<ClaudemdEdit> {
    let path = text.path.to_string_lossy().into_owned();
    let mut drop_lines: BTreeSet<usize> = BTreeSet::new();

    // Dead references, every occurrence rather than the capped display list -
    // but only the ones that name a file. A CLAUDE.md that lists its project's
    // URL routes is full of tokens that resolve like paths and are not paths,
    // and this transform deletes whole lines. Reporting one of those as a
    // finding costs a shrug; deleting the line costs the user a rule.
    let mut dead_refs: Vec<String> = Vec::new();
    for dead in claudemd::dead_refs_located(text) {
        if !claudemd::has_file_extension(&dead.reference) {
            continue;
        }
        if !dead_refs.contains(&dead.reference) {
            dead_refs.push(dead.reference.clone());
        }
        drop_lines.insert(dead.line);
    }

    // Duplicated blocks: only the copy that is redundant, and only when the
    // other copy is loaded in the same sessions as this one.
    let located = claudemd::paragraphs_located(&text.text);
    let mut dup_blocks = 0usize;
    let mut dup_labels: Vec<String> = Vec::new();
    for finding in findings {
        let FindingKind::DuplicateBlock {
            others,
            label,
            bytes,
        } = &finding.kind
        else {
            continue;
        };
        let Some(keeper) = duplicate_is_redundant(&path, text.project.as_deref(), others, projects)
        else {
            continue;
        };
        // The finding names the block by its first 60 normalized characters and
        // its normalized length, which is enough to find it again in this file
        // and not enough to be confused with another paragraph.
        let Some(para) = located.iter().find(|p| {
            p.text.len() == *bytes && p.text.chars().take(label.chars().count()).eq(label.chars())
        }) else {
            continue;
        };
        for line in para.start..para.end {
            drop_lines.insert(line);
        }
        dup_blocks += 1;
        dup_labels.push(file_name(&keeper));
    }

    if drop_lines.is_empty() {
        return None;
    }
    let lines = lines_with_endings(&text.text);
    let kept: String = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !drop_lines.contains(i))
        .map(|(_, l)| *l)
        .collect();
    let removed_bytes = text.text.len() as i64 - kept.len() as i64;
    if removed_bytes <= 0 {
        return None;
    }
    let content = if text.had_bom {
        let mut s = String::with_capacity(kept.len() + BOM.len_utf8());
        s.push(BOM);
        s.push_str(&kept);
        s
    } else {
        kept
    };
    Some(ClaudemdEdit {
        content,
        dead_refs,
        dup_blocks,
        dup_labels,
        removed_lines: drop_lines.len(),
        removed_bytes,
    })
}

/// Which copy of a duplicated block survives, when this one is the redundant
/// one. `None` means keep this copy (or the two are never loaded together, so
/// neither is redundant).
///
/// Two files are only both charged when a session loads both:
///
/// * a global file is loaded by every session, so a project file's copy of a
///   global block is pure duplication - the project copy goes;
/// * two global files, or two files in the same project, are always loaded
///   together - the first in path order survives, which is arbitrary but stable;
/// * two files in *different* projects are never loaded together, and deleting
///   either would remove guidance from a session that only had one copy.
fn duplicate_is_redundant(
    path: &str,
    project: Option<&str>,
    others: &[String],
    projects: &BTreeMap<&str, Option<&str>>,
) -> Option<String> {
    let scope_of = |p: &str| projects.get(p).copied().flatten();
    if project.is_some() {
        if let Some(global) = others
            .iter()
            .filter(|o| scope_of(o.as_str()).is_none())
            .min()
        {
            return Some(global.clone());
        }
    }
    // Same-scope neighbours only: for a global file that is the other globals,
    // for a project file the rest of its own project.
    let mut peers: Vec<&str> = others
        .iter()
        .filter(|o| scope_of(o.as_str()) == project)
        .map(String::as_str)
        .collect();
    peers.push(path);
    let winner = peers.into_iter().min()?;
    (winner != path).then(|| winner.to_string())
}

// ---------------------------------------------------------------------------
// Generator: ClaudemdTrim
// ---------------------------------------------------------------------------

/// Oversized CLAUDE.md files worth a rewrite.
///
/// No draft here: shortening someone's guidance without changing what it means
/// is writing, and Piggy will not do that with a heuristic. The candidate states
/// the case, carries [`Prerequisite::NeedsAdvisor`], and leaves
/// [`Candidate::new_content`] empty for M5.4 to fill after its guard has checked
/// the draft.
pub fn claudemd_trim(inputs: &Inputs) -> Vec<Candidate> {
    let mut out = Vec::new();
    for scanned in &inputs.claudemd.files {
        let Some(finding) = scanned
            .findings
            .iter()
            .find(|f| matches!(f.kind, FindingKind::Oversize { .. }))
        else {
            continue;
        };
        let FindingKind::Oversize { threshold } = finding.kind else {
            continue;
        };
        let title = format!(
            "Trim {}",
            claudemd_label(&scanned.file.path, scanned.file.project.as_deref())
        );
        let evidence = vec![
            EvidenceRow::new(
                "What this file costs per load",
                format!("~{} tokens", commas(scanned.file.est_tokens.max(0) as u64)),
                basis::ESTIMATED,
            ),
            EvidenceRow::new(
                "The line a file is worth trimming past",
                format!("{} tokens", commas(threshold.max(0) as u64)),
                basis::ESTIMATED,
            ),
            EvidenceRow::new(
                "Sessions in the last 30 days that load it",
                commas(scanned.sessions_30d.max(0) as u64),
                basis::OBSERVED,
            ),
            EvidenceRow::new(
                "Tokens a month it costs you",
                format!("~{}", commas(scanned.est_tokens_month.max(0) as u64)),
                basis::ESTIMATED,
            ),
        ];
        out.push(build(
            ActionKind::ClaudemdTrim,
            scanned.file.path.clone(),
            title,
            evidence,
            // The burden, not a promised saving: how much a rewrite removes is
            // unknown until it is drafted, and this is its ceiling.
            scanned.est_tokens_month,
            RISK_CONTENT_EDIT,
            vec![Prerequisite::NeedsAdvisor],
            scanned.file.hash.clone(),
            Params::Claudemd {
                path: scanned.file.path.clone(),
            },
            None,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Generator: SaverMix
// ---------------------------------------------------------------------------

/// Savers whose own measurements argue for a different setting.
///
/// Two cases, both grounded entirely in [`crate::attribution`] and both quoting
/// its numbers verbatim with the badge they were earned under:
///
/// * a saver that **changes how Claude answers** and has been compared over
///   [`MIN_RANDOMIZED_PER_SIDE`] randomized sessions a side without moving
///   anything: the behaviour change is being paid for nothing;
/// * a saver that is installed, off, and has a measured favourable delta on some
///   stream: it is being left on the table. Skipped when it conflicts with a
///   saver that is currently on, because that is a trade, not a free win.
pub fn saver_mix(inputs: &Inputs) -> Vec<Candidate> {
    let enabled: BTreeSet<&str> = inputs
        .savers
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.entry.id.as_str())
        .collect();

    let mut out = Vec::new();
    for saver in &inputs.savers {
        let attr = &saver.attribution;
        let (rand_on, rand_off) = attr.randomized_counts();
        let name = &saver.entry.name;

        if saver.enabled && saver.entry.behavior_changing {
            let enough = rand_on >= MIN_RANDOMIZED_PER_SIDE && rand_off >= MIN_RANDOMIZED_PER_SIDE;
            // Every arm has to have been compared and found flat or unreadable,
            // and at least one has to have actually settled - "all inconclusive"
            // is not a result, it is noise.
            let settled = attr
                .arms()
                .any(|s| matches!(s.reading(), Reading::NoChange { .. }));
            let nothing_moved = attr.arms().all(|s| {
                matches!(
                    s.reading(),
                    Reading::NoChange { .. } | Reading::Inconclusive | Reading::Quiet
                )
            });
            if enough && settled && nothing_moved {
                let mut evidence = vec![EvidenceRow::new(
                    "Randomized sessions compared",
                    format!("{rand_on} with it on, {rand_off} with it off"),
                    basis::OBSERVED,
                )];
                evidence.extend(attr.arms().map(stream_evidence));
                out.push(build(
                    ActionKind::SaverMix,
                    saver.entry.id.clone(),
                    format!("{name} has not moved the needle; turn it off"),
                    evidence,
                    // No measured saving, so none is claimed. The case for this
                    // one is the behaviour change it stops paying for.
                    0,
                    RISK_TOGGLE,
                    Vec::new(),
                    saver_fingerprint(saver.enabled),
                    Params::SaverMix {
                        saver: saver.entry.id.clone(),
                        turn_on: false,
                    },
                    None,
                ));
            }
            continue;
        }

        if !saver.enabled {
            // Conflicts are symmetric: either entry may declare the pair, and a
            // conflicting saver that is on means turning this one on is a swap.
            let conflicts = saver
                .entry
                .conflicts_with
                .iter()
                .any(|other| enabled.contains(other.as_str()))
                || inputs.savers.iter().any(|other| {
                    other.enabled
                        && other
                            .entry
                            .conflicts_with
                            .iter()
                            .any(|c| c == &saver.entry.id)
                });
            if conflicts {
                continue;
            }
            let favourable: Vec<&StreamStat> = attr
                .arms()
                .filter(|s| s.badge == Badge::Measured && s.delta.is_some_and(|d| d > 0.0))
                .collect();
            if favourable.is_empty() {
                continue;
            }
            let est_tokens_month: i64 = favourable
                .iter()
                .filter_map(|s| Some((stream_tokens_30d(s.stream, &inputs.tokens_30d)?, s.delta?)))
                .map(|(tokens, delta)| (tokens as f64 * delta).round() as i64)
                .sum();
            let best = favourable
                .iter()
                .max_by(|a, b| a.delta.unwrap_or(0.0).total_cmp(&b.delta.unwrap_or(0.0)))
                .expect("favourable is non-empty");
            let pct = best.measured_pct().unwrap_or(0.0).round() as i64;

            let mut evidence = vec![EvidenceRow::new(
                "Randomized sessions compared",
                format!("{rand_on} with it on, {rand_off} with it off"),
                basis::OBSERVED,
            )];
            evidence.extend(attr.arms().map(stream_evidence));
            for stat in &favourable {
                if let Some(tokens) = stream_tokens_30d(stat.stream, &inputs.tokens_30d) {
                    evidence.push(EvidenceRow::new(
                        format!("{} tokens in the last 30 days", stat.stream.label()),
                        commas(tokens),
                        basis::OBSERVED,
                    ));
                }
            }
            evidence.push(EvidenceRow::new(
                "Tokens a month that delta would have saved",
                format!("~{}", commas(est_tokens_month.max(0) as u64)),
                basis::ESTIMATED,
            ));
            out.push(build(
                ActionKind::SaverMix,
                saver.entry.id.clone(),
                format!(
                    "{name} measured {pct}% less {}; turn it on",
                    best.stream.label()
                ),
                evidence,
                est_tokens_month,
                RISK_TOGGLE,
                Vec::new(),
                saver_fingerprint(saver.enabled),
                Params::SaverMix {
                    saver: saver.entry.id.clone(),
                    turn_on: true,
                },
                None,
            ));
        }
    }
    out
}

/// One comparison arm as an evidence row, quoting the numbers verbatim.
fn stream_evidence(stat: &StreamStat) -> EvidenceRow {
    let value = match (stat.shown_pct(), stat.note()) {
        (Some(pct), _) => format!(
            "{:.0}% ({} sessions on, {} off)",
            pct, stat.n_on, stat.n_off
        ),
        (None, Some(note)) => format!("{note} ({} on, {} off)", stat.n_on, stat.n_off),
        (None, None) => format!("{} sessions on, {} off", stat.n_on, stat.n_off),
    };
    EvidenceRow::new(stat.stream.label(), value, badge_basis(stat.badge))
}

/// Tokens actually spent on one stream in the last 30 days. `None` for
/// [`Stream::Turns`], which is a count of turns and not a token total.
fn stream_tokens_30d(stream: Stream, totals: &Totals) -> Option<u64> {
    match stream {
        Stream::Input => Some(totals.input_tokens),
        Stream::Output => Some(totals.output_tokens),
        Stream::CacheCreate => Some(totals.cache_creation_tokens),
        Stream::CacheRead => Some(totals.cache_read_tokens),
        Stream::Turns => None,
    }
}

/// A saver's plan is invalidated by the very thing it proposes to change, so the
/// fingerprint is its current on/off state: flip it by hand and the suggestion
/// retires itself.
fn saver_fingerprint(enabled: bool) -> String {
    if enabled { "on" } else { "off" }.to_string()
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// What one apply did.
#[derive(Debug, Clone)]
pub struct Applied {
    pub id: String,
    pub kind: ActionKind,
    pub target: String,
    /// The handle [`undo`] reads back. Human-readable on purpose: it lands in a
    /// database column somebody will eventually read with `sqlite3`.
    pub restore_ref: String,
    pub applied_at: String,
    /// What changed, in the user's terms.
    pub message: String,
    /// Non-fatal problems worth surfacing.
    pub warnings: Vec<String>,
}

/// Apply one candidate and stamp its row `applied`.
///
/// Refuses rather than guesses: a row that is already applied, a plan whose
/// target moved since it was computed, a draft that does not exist yet. Every
/// path that does write leaves a restore record behind first.
pub fn apply(
    store: &mut Store,
    state: &mut PiggyState,
    catalog: &Catalog,
    candidate: &Candidate,
) -> Result<Applied> {
    let now = chrono::Utc::now().to_rfc3339();
    // A candidate applied straight from a fresh generate may not have a row yet
    // (nothing inserted it, or the caller built it itself); an existing row is
    // left exactly as it is.
    store.insert_advice(&candidate.row(&now)?)?;
    if let Some(row) = store.advice(&candidate.id)? {
        match row.status.as_str() {
            advice_status::APPLIED => {
                bail!("'{}' is already applied - undo it first", candidate.title)
            }
            advice_status::STALE => bail!(
                "'{}' was computed against something that has since changed; re-run the scan",
                candidate.title
            ),
            _ => {}
        }
    }

    let mut warnings = Vec::new();
    let (restore_ref, message) = match &candidate.params {
        Params::ServerDisable {
            item_kind,
            id,
            source,
            n_sessions,
        } => apply_server_disable(store, state, item_kind, id, source.as_deref(), *n_sessions)?,
        Params::ServerScope {
            server,
            projects,
            config,
        } => apply_server_scope(
            state,
            &candidate.id,
            server,
            projects,
            config,
            &candidate.fingerprint,
        )?,
        Params::Claudemd { path } => apply_claudemd(state, candidate, path)?,
        Params::SaverMix { saver, turn_on } => {
            let (r, m, w) = apply_saver_mix(state, catalog, saver, *turn_on)?;
            warnings = w;
            (r, m)
        }
    };

    store.set_advice_status(
        &candidate.id,
        advice_status::APPLIED,
        Some(&now),
        Some(&restore_ref),
        None,
    )?;
    Ok(Applied {
        id: candidate.id.clone(),
        kind: candidate.kind,
        target: candidate.target.clone(),
        restore_ref,
        applied_at: now,
        message,
        warnings,
    })
}

/// `restore_ref` prefix for a [`crate::sweep`] record.
const REF_SWEEP: &str = "sweep:";
/// `restore_ref` prefix for a [`ScopeMove`] record.
const REF_SCOPE_MOVE: &str = "scope-move:";
/// `restore_ref` prefix for a [`FileSnapshot`] record.
const REF_FILE_SNAPSHOT: &str = "file-snapshot:";
/// `restore_ref` prefix for a saver toggle; the suffix is the state to go back
/// to.
const REF_SAVER: &str = "saver:";

fn apply_server_disable(
    store: &Store,
    state: &mut PiggyState,
    item_kind: &str,
    id: &str,
    source: Option<&str>,
    n_sessions: usize,
) -> Result<(String, String)> {
    // Sweep addresses its rows by index against a fresh scan, so the candidate's
    // stable address has to be resolved to one. Nothing about sweep's apply path
    // changes.
    let report = sweep::scan(store, n_sessions)?;
    let item = report
        .items
        .iter()
        .find(|i| i.kind == item_kind && i.id == id && i.source.as_deref() == source)
        .ok_or_else(|| {
            anyhow!(
                "'{id}' is no longer configured under {}",
                scope_label(source)
            )
        })?;
    if !item.recommend_disable {
        bail!(
            "'{id}' is in use again ({} call(s) in the window); Piggy will not switch it off",
            item.used
        );
    }
    sweep::apply(store, state, item.idx, n_sessions)?;
    Ok((
        format!("{REF_SWEEP}{item_kind}|{id}|{}", source.unwrap_or("")),
        format!("turned off the {id} {item_kind}"),
    ))
}

fn apply_server_scope(
    state: &mut PiggyState,
    advice_id: &str,
    server: &str,
    projects: &[String],
    config: &Value,
    fingerprint: &str,
) -> Result<(String, String)> {
    let path = config::claude_json_path();
    let mut before_user = Value::Null;
    let mut before_projects: BTreeMap<String, Value> = BTreeMap::new();
    let mut created = CreatedContainers::default();

    sweep::edit_json_atomic(&path, |root| {
        let user = sweep::mcp_servers_mut(root, None, false)
            .and_then(|m| m.get(server).cloned())
            .ok_or_else(|| {
                anyhow!(
                    "'{server}' is no longer configured at user scope in {}",
                    path.display()
                )
            })?;
        // The plan was computed against a specific command, args and env. If any
        // of them changed, the schemas we costed are not the schemas that would
        // move.
        let current = ConfiguredServer {
            key: server.to_string(),
            project: None,
            transport: Transport::Stdio,
            config: user.clone(),
        };
        if current.config_hash() != fingerprint {
            bail!("'{server}' has been reconfigured since Piggy read it; re-run the scan");
        }
        before_user = user.clone();

        created.projects_root = root.get("projects").is_none();
        for project in projects {
            created.entries.extend(
                root.get("projects")
                    .and_then(|p| p.get(project))
                    .is_none()
                    .then(|| project.clone()),
            );
            created.maps.extend(
                root.get("projects")
                    .and_then(|p| p.get(project))
                    .and_then(|p| p.get("mcpServers"))
                    .is_none()
                    .then(|| project.clone()),
            );
            // Claude Code keys `projects` by working directory; a project Piggy
            // has seen sessions in may still be absent from the map, so the
            // entry is created rather than refused - and recorded above, so Undo
            // takes the scaffolding away with it.
            let projects_map = root
                .as_object_mut()
                .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?
                .entry("projects")
                .or_insert_with(|| Value::Object(Map::new()));
            let slot = projects_map
                .as_object_mut()
                .ok_or_else(|| anyhow!("{} has a non-object `projects`", path.display()))?
                .entry(project.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !slot.is_object() {
                bail!(
                    "{} has a non-object entry for project {project}",
                    path.display()
                );
            }
            let servers = sweep::mcp_servers_mut(root, Some(project), true).ok_or_else(|| {
                anyhow!(
                    "{} has no `mcpServers` map for project {project}",
                    path.display()
                )
            })?;
            let prior = servers.insert(server.to_string(), config.clone());
            before_projects.insert(project.clone(), prior.unwrap_or(Value::Null));
        }

        // Removed last, so a failure above leaves the file untouched.
        if let Some(m) = sweep::mcp_servers_mut(root, None, false) {
            m.remove(server);
        }
        Ok(())
    })?;

    state.scope_moves.push(ScopeMove {
        advice_id: advice_id.to_string(),
        server: server.to_string(),
        before_user,
        before_projects,
        created,
        moved_at: chrono::Utc::now().to_rfc3339(),
    });
    state.save()?;
    Ok((
        format!("{REF_SCOPE_MOVE}{advice_id}"),
        format!(
            "moved {server} out of user scope into {}",
            projects.join(", ")
        ),
    ))
}

fn apply_claudemd(
    state: &mut PiggyState,
    candidate: &Candidate,
    path: &str,
) -> Result<(String, String)> {
    let Some(content) = candidate.new_content.as_deref() else {
        let note = candidate
            .prerequisites
            .first()
            .map(Prerequisite::note)
            .unwrap_or("there is no drafted replacement for this file");
        bail!("nothing to write for {path}: {note}");
    };
    let file = Path::new(path);
    // The content hash, not the mtime: a file that was touched but not edited is
    // not a conflict, and one rewritten inside the same mtime tick still is.
    // Boxed as an error so a caller can `downcast_ref::<Conflict>()` and tell
    // "someone edited it" from "it is gone".
    snapshots::check_unchanged(file, &candidate.fingerprint).map_err(anyhow::Error::new)?;
    let record = snapshots::snapshot(file, Some(&candidate.id), state)?;
    snapshots::write_atomic(file, content.as_bytes())?;
    state.save()?;
    Ok((
        format!("{REF_FILE_SNAPSHOT}{}", record.backup),
        format!("rewrote {path} (original backed up to {})", record.backup),
    ))
}

fn apply_saver_mix(
    state: &mut PiggyState,
    catalog: &Catalog,
    saver: &str,
    turn_on: bool,
) -> Result<(String, String, Vec<String>)> {
    let prior = state.savers.get(saver).map(|s| s.enabled);
    if prior == Some(turn_on) {
        bail!(
            "'{saver}' is already {}",
            if turn_on { "on" } else { "off" }
        );
    }
    let report = engine::set_enabled(catalog, saver, turn_on)?;
    // `set_enabled` loads, mutates and saves state itself, so the caller's copy
    // is a version behind the moment it returns.
    *state = PiggyState::load()?;
    Ok((
        format!("{REF_SAVER}{saver}:{}", if turn_on { "off" } else { "on" }),
        format!("turned {saver} {}", if turn_on { "on" } else { "off" }),
        report.warnings,
    ))
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

/// One thing an undo could not put back, and why.
#[derive(Debug, Clone)]
pub struct UndoFailure {
    /// The file, server or saver that is still not back.
    pub item: String,
    pub reason: String,
}

/// What one undo did. A failure is reported, never thrown away: an Undo that
/// restored three of four files has to say which one it did not.
#[derive(Debug, Clone)]
pub struct Undone {
    pub id: String,
    pub kind: ActionKind,
    pub restored: usize,
    pub failures: Vec<UndoFailure>,
    pub message: String,
}

impl Undone {
    /// Whether everything came back (and the row therefore returned to `open`).
    pub fn complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Reverse an applied advice row and return it to `open`.
///
/// The row's own payload is the source of truth, not a fresh generate: by the
/// time somebody clicks Undo the candidate has usually stopped regenerating,
/// which is exactly what applying it was supposed to achieve.
pub fn undo(
    store: &mut Store,
    state: &mut PiggyState,
    catalog: &Catalog,
    id: &str,
) -> Result<Undone> {
    let row = store
        .advice(id)?
        .ok_or_else(|| anyhow!("no advice with id '{id}'"))?;
    if row.status != advice_status::APPLIED {
        bail!("advice '{id}' is {}, not applied", row.status);
    }
    let restore_ref = row
        .restore_ref
        .clone()
        .ok_or_else(|| anyhow!("advice '{id}' was applied without a restore reference"))?;
    let candidate = Candidate::from_row(&row)?;

    let mut failures = Vec::new();
    let mut restored = 0usize;
    let message = match &candidate.params {
        Params::ServerDisable {
            item_kind,
            id: item_id,
            source,
            ..
        } => {
            match sweep::restore_item(state, item_kind, item_id, source.as_deref()) {
                Ok(true) => restored += 1,
                Ok(false) => failures.push(UndoFailure {
                    item: item_id.clone(),
                    reason: "Piggy has no record of switching it off any more".to_string(),
                }),
                Err(e) => failures.push(UndoFailure {
                    item: item_id.clone(),
                    reason: format!("{e:#}"),
                }),
            }
            state.save()?;
            format!("put the {item_id} {item_kind} back")
        }
        Params::ServerScope { server, .. } => {
            match restore_scope_move(state, id) {
                Ok(()) => restored += 1,
                Err(e) => failures.push(UndoFailure {
                    item: server.clone(),
                    reason: format!("{e:#}"),
                }),
            }
            format!("moved {server} back to user scope")
        }
        Params::Claudemd { path } => {
            let outcome = undo_file_snapshots(state, id)?;
            restored += outcome.restored;
            failures.extend(outcome.failures.into_iter().map(|f| UndoFailure {
                item: f.path,
                reason: f.reason,
            }));
            format!("restored {path} from its backup")
        }
        Params::SaverMix { saver, .. } => {
            let back_on = restore_ref
                .strip_prefix(REF_SAVER)
                .and_then(|rest| rest.rsplit(':').next())
                .map(|s| s == "on")
                .ok_or_else(|| anyhow!("advice '{id}' has an unreadable restore reference"))?;
            match engine::set_enabled(catalog, saver, back_on) {
                Ok(_) => restored += 1,
                Err(e) => failures.push(UndoFailure {
                    item: saver.clone(),
                    reason: format!("{e:#}"),
                }),
            }
            *state = PiggyState::load()?;
            format!("turned {saver} back {}", if back_on { "on" } else { "off" })
        }
    };

    // Only a clean undo returns the row to `open`. Leaving a partly-restored row
    // `applied` keeps its restore reference, which is the only handle on what is
    // still outstanding.
    if failures.is_empty() {
        store.set_advice_status(id, advice_status::OPEN, None, None, None)?;
    }
    Ok(Undone {
        id: id.to_string(),
        kind: candidate.kind,
        restored,
        failures,
        message,
    })
}

/// Put both ends of a scope move back exactly as they were, dropping the record
/// on success. Shared with [`crate::engine::restore_defaults`], which has to
/// reverse these too or the panic button leaves a server somewhere the user
/// never put it.
pub(crate) fn restore_scope_move(state: &mut PiggyState, advice_id: &str) -> Result<()> {
    let Some(pos) = state
        .scope_moves
        .iter()
        .rposition(|m| m.advice_id == advice_id)
    else {
        bail!("Piggy has no record of moving that server");
    };
    let record = state.scope_moves[pos].clone();
    let path = config::claude_json_path();
    sweep::edit_json_atomic(&path, |root| {
        // Projects first, then user scope, mirroring apply in reverse.
        for (project, before) in &record.before_projects {
            let Some(servers) = sweep::mcp_servers_mut(root, Some(project), false) else {
                continue; // the project entry is gone; nothing of ours is left in it
            };
            match before {
                Value::Null => {
                    servers.remove(&record.server);
                }
                value => {
                    servers.insert(record.server.clone(), value.clone());
                }
            }
        }
        if !record.before_user.is_null() {
            let servers = sweep::mcp_servers_mut(root, None, true).ok_or_else(|| {
                anyhow!(
                    "{} has no top-level `mcpServers` map to put '{}' back in",
                    path.display(),
                    record.server
                )
            })?;
            servers.insert(record.server.clone(), record.before_user.clone());
        }

        // Take the scaffolding away, innermost first, and only where it is still
        // empty: anything the user has since put in one of these is theirs.
        for project in &record.created.maps {
            if sweep::mcp_servers_mut(root, Some(project), false).is_some_and(|m| m.is_empty()) {
                if let Some(entry) = project_entry_mut(root, project) {
                    entry.remove("mcpServers");
                }
            }
        }
        for project in &record.created.entries {
            if project_entry_mut(root, project).is_some_and(|e| e.is_empty()) {
                if let Some(projects) = root.get_mut("projects").and_then(Value::as_object_mut) {
                    projects.remove(project);
                }
            }
        }
        if record.created.projects_root
            && root
                .get("projects")
                .and_then(Value::as_object)
                .is_some_and(Map::is_empty)
        {
            if let Some(obj) = root.as_object_mut() {
                obj.remove("projects");
            }
        }
        Ok(())
    })?;
    state.scope_moves.remove(pos);
    state.save()?;
    Ok(())
}

/// Restore every file snapshot belonging to one advice row, dropping the records
/// that came back and keeping the ones that did not.
fn undo_file_snapshots(
    state: &mut PiggyState,
    advice_id: &str,
) -> Result<snapshots::RestoreOutcome> {
    let mine: Vec<FileSnapshot> = state
        .file_snapshots
        .iter()
        .filter(|s| s.advice_id.as_deref() == Some(advice_id))
        .cloned()
        .collect();
    if mine.is_empty() {
        bail!("Piggy has no backup recorded for that edit");
    }
    let outcome = snapshots::restore(&mine);
    prune_restored(&mut state.file_snapshots, &mine, &outcome);
    state.save()?;
    Ok(outcome)
}

/// Drop the records in `attempted` that came back, keeping every one that did
/// not: the backup is the only copy of the original bytes, so a record whose
/// restore failed must survive to be retried.
pub(crate) fn prune_restored(
    records: &mut Vec<FileSnapshot>,
    attempted: &[FileSnapshot],
    outcome: &snapshots::RestoreOutcome,
) {
    let failed: BTreeSet<&str> = outcome.failures.iter().map(|f| f.path.as_str()).collect();
    let restored: BTreeSet<(&str, &str)> = attempted
        .iter()
        .filter(|r| !failed.contains(r.path.as_str()))
        .map(|r| (r.path.as_str(), r.backup.as_str()))
        .collect();
    records.retain(|r| !restored.contains(&(r.path.as_str(), r.backup.as_str())));
}

// ---------------------------------------------------------------------------
// Dismiss
// ---------------------------------------------------------------------------

/// Mark a suggestion "not for me", recording what it was worth at the time.
///
/// The baseline is what makes the suppression honest: the same target comes back
/// only once it costs [`REOPEN_MULTIPLIER`] times what the user waved away, so a
/// no stays a no while the evidence stands still.
pub fn dismiss(store: &mut Store, id: &str, note: Option<&str>) -> Result<bool> {
    let Some(row) = store.advice(id)? else {
        return Ok(false);
    };
    let recorded = serde_json::to_string(&DismissNote {
        note: note.map(str::to_string),
        est_tokens_month: Some(row.est_tokens_month),
    })?;
    store.set_advice_status(id, advice_status::DISMISSED, None, None, Some(&recorded))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Assemble a candidate and hash its id. One constructor so no generator can
/// forget a field or hash a different set of inputs than it shows.
#[allow(clippy::too_many_arguments)]
fn build(
    kind: ActionKind,
    target: String,
    title: String,
    evidence: Vec<EvidenceRow>,
    est_tokens_month: i64,
    risk_tier: u8,
    prerequisites: Vec<Prerequisite>,
    fingerprint: String,
    params: Params,
    new_content: Option<String>,
) -> Candidate {
    let id = candidate_id(kind, &target, &fingerprint, &evidence);
    Candidate {
        id,
        kind,
        target,
        title,
        evidence,
        est_tokens_month,
        risk_tier,
        prerequisites,
        fingerprint,
        params,
        new_content,
        status: advice_status::OPEN.to_string(),
    }
}

/// The stable id: kind, target, fingerprint and every evidence row, in order.
///
/// Evidence is in because a card whose numbers moved is a different suggestion -
/// the user has not seen this one. The fingerprint is in because two files can
/// carry identical evidence and different bytes. Field separators are NUL bytes,
/// which none of the inputs can contain, so no two different inputs can hash the
/// same string.
fn candidate_id(
    kind: ActionKind,
    target: &str,
    fingerprint: &str,
    evidence: &[EvidenceRow],
) -> String {
    let mut h = Sha256::new();
    h.update(ID_DOMAIN);
    h.update(kind.as_str().as_bytes());
    h.update([0]);
    h.update(target.as_bytes());
    h.update([0]);
    h.update(fingerprint.as_bytes());
    h.update([0]);
    for row in evidence {
        h.update(row.label.as_bytes());
        h.update([0]);
        h.update(row.value.as_bytes());
        h.update([0]);
        h.update(row.basis.as_bytes());
        h.update([0]);
    }
    let hex = format!("{:x}", h.finalize());
    format!("{}-{}", kind.as_str(), &hex[..ID_HEX_LEN])
}

/// One `projects.<path>` object in a parsed `~/.claude.json`.
fn project_entry_mut<'a>(root: &'a mut Value, project: &str) -> Option<&'a mut Map<String, Value>> {
    root.get_mut("projects")?.get_mut(project)?.as_object_mut()
}

/// The [`ConfiguredServer`] behind a sweep row, when there is one.
fn configured_server<'a>(
    inputs: &'a Inputs,
    key: &str,
    source: Option<&str>,
) -> Option<&'a ConfiguredServer> {
    inputs
        .servers
        .iter()
        .find(|s| s.key == key && s.project.as_deref() == source)
}

/// A server's display target: the key plus where it is configured.
fn server_target(key: &str, source: Option<&str>) -> String {
    format!("{key} ({})", scope_label(source))
}

fn scope_label(source: Option<&str>) -> String {
    source
        .map(str::to_string)
        .unwrap_or_else(|| "user scope".to_string())
}

/// A project path's last segment, which is what a person calls it.
fn short_project(project: &str) -> String {
    Path::new(project)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| project.to_string())
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// How a CLAUDE.md file is named in a title. Every project's is called
/// `CLAUDE.md`, so the file name alone leaves two suggestions reading the same.
fn claudemd_label(path: &str, project: Option<&str>) -> String {
    let name = file_name(path);
    match project {
        Some(project) => format!("{}'s {name}", short_project(project)),
        None => format!("your global {name}"),
    }
}

/// `"1 dead reference"` / `"3 dead references"`.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Most items spelled out in an evidence value before the rest becomes a count.
/// Past this the row stops being evidence and starts being the file.
const MAX_LISTED: usize = 6;

/// A comma list, capped: the whole point of an evidence row is that a person
/// reads it.
fn list_briefly(items: &[String]) -> String {
    if items.len() <= MAX_LISTED {
        return items.join(", ");
    }
    format!(
        "{}, and {} more",
        items[..MAX_LISTED].join(", "),
        items.len() - MAX_LISTED
    )
}

/// `text` split into lines, each keeping its own terminator.
///
/// Rebuilding is a plain concatenation, so a file with CRLF (or mixed) endings
/// comes back byte-identical apart from the lines that were dropped - rejoining
/// `str::lines()` with `\n` would quietly rewrite every line ending in the file.
/// Index `i` here is the same line `i` that `str::lines()` yields, which is what
/// the detectors report positions in.
fn lines_with_endings(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, _) in text.match_indices('\n') {
        out.push(&text[start..=i]);
        start = i + 1;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}
