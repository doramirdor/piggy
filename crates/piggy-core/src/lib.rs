//! `piggy-core` — the ground-truth measurement core for Piggy.
//!
//! It parses Claude Code session logs (`~/.claude/projects/**/*.jsonl`),
//! aggregates deduplicated per-model token usage, prices it with an embedded
//! (user-overridable) table, and persists per-session aggregates into a local
//! SQLite database for incremental re-indexing and querying.
//!
//! The crate is UI-agnostic: the `piggy` CLI and (later) the Tauri app both
//! link against it.

pub mod attribution;
pub mod codex;
pub mod config;
pub mod discovery;
pub mod engine;
pub mod index;
pub mod parser;
pub mod pricing;
pub mod registry;
pub mod rng;
pub mod rotation;
pub mod saver_config;
pub mod settings;
pub mod sources;
pub mod state;
pub mod stats;
pub mod store;
pub mod sweep;
pub mod tagging;
pub mod watcher;

pub use attribution::{
    attribute, headline, Badge, Headline, HeadlineBaseline, SaverAttribution, Stream, StreamStat,
};
pub use codex::parse_codex_file;
pub use discovery::{DiscoveredRepo, DiscoveryCache};
pub use engine::{ActionReport, HealthReport};
pub use index::{default_roots, run_index, run_index_roots, IndexReport, SourceRoot};
pub use parser::{parse_file, ModelTokens, SessionParse};
pub use pricing::{ModelPrice, Pricing};
pub use registry::{Catalog, Entry};
pub use rotation::{RotationOutcome, RotationPlan};
pub use sources::{Interface, SourceKind};
pub use state::PiggyState;
pub use stats::{GroupRow, Period, SourceRow, Totals};
pub use store::{SaverTag, Store};
pub use sweep::{SweepItem, SweepReport};
pub use watcher::{SessionWatcher, WatchEvent};
