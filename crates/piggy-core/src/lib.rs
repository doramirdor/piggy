//! `piggy-core` — the ground-truth measurement core for Piggy.
//!
//! It parses Claude Code session logs (`~/.claude/projects/**/*.jsonl`),
//! aggregates deduplicated per-model token usage, prices it with an embedded
//! (user-overridable) table, and persists per-session aggregates into a local
//! SQLite database for incremental re-indexing and querying.
//!
//! The crate is UI-agnostic: the `piggy` CLI and (later) the Tauri app both
//! link against it.

pub mod advice;
pub mod advisor;
pub mod attribution;
pub mod claudemd;
pub mod cli_link;
pub mod codex;
pub mod config;
pub mod discovery;
pub mod engine;
pub mod index;
pub mod insights;
pub mod ledger;
pub mod parser;
pub mod pricing;
pub mod probe;
pub mod registry;
pub mod rng;
pub mod rotation;
pub mod saver_config;
pub mod settings;
pub mod snapshots;
pub mod sources;
pub mod state;
pub mod stats;
pub mod store;
pub mod tasks;
pub mod sweep;
pub mod tagging;
pub mod watcher;

pub use advice::{ActionKind, Applied, Candidate, EvidenceRow, Prerequisite, Undone};
pub use advisor::{
    facts::Facts, guard::Annotation, available, model, recommended, AdvisorModel, AdvisorState,
    CATALOG,
};
pub use attribution::{
    attribute, headline, Badge, Headline, HeadlineBaseline, SaverAttribution, Stream, StreamStat,
};
pub use claudemd::{ClaudemdReport, Finding, FindingKind, ProjectMcpServers, ScannedFile};
pub use cli_link::LinkReport;
pub use codex::parse_codex_file;
pub use discovery::{DiscoveredRepo, DiscoveryCache};
pub use engine::{ActionReport, HealthReport};
pub use index::{default_roots, run_index, run_index_roots, IndexReport, SourceRoot};
pub use insights::{insights, Insight, Severity};
pub use ledger::{Ledger, LedgerRow, ProjectRow};
pub use parser::{parse_file, ContextTokens, ModelTokens, SessionParse, CTX_CONVERSATION, CTX_FLOOR, CTX_FLOOR_PREFIX};
pub use pricing::{ModelPrice, Pricing};
pub use probe::{ConfiguredServer, Measurement, MeasurementStatus, ProbeError, ProbeOptions};
pub use registry::{Catalog, Entry};
pub use rotation::{RotationOutcome, RotationPlan};
pub use snapshots::{FileBackup, FileSnapshot};
pub use sources::{Interface, SourceKind};
pub use state::PiggyState;
pub use stats::{GroupRow, Period, SourceRow, Totals};
pub use tasks::TaskRow;
pub use store::{AdviceRow, ClaudemdFile, McpManifest, SaverTag, Store};
pub use sweep::{SweepItem, SweepReport};
pub use watcher::{SessionWatcher, WatchBackend, WatchEvent};
