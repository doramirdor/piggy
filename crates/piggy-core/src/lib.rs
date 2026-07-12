//! `piggy-core` — the ground-truth measurement core for Piggy.
//!
//! It parses Claude Code session logs (`~/.claude/projects/**/*.jsonl`),
//! aggregates deduplicated per-model token usage, prices it with an embedded
//! (user-overridable) table, and persists per-session aggregates into a local
//! SQLite database for incremental re-indexing and querying.
//!
//! The crate is UI-agnostic: the `piggy` CLI and (later) the Tauri app both
//! link against it.

pub mod config;
pub mod index;
pub mod parser;
pub mod pricing;
pub mod stats;
pub mod store;

pub use index::{run_index, IndexReport};
pub use parser::{parse_file, ModelTokens, SessionParse};
pub use pricing::{ModelPrice, Pricing};
pub use stats::{GroupRow, Period, Totals};
pub use store::Store;
