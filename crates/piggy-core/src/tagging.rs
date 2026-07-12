//! Session tagging: snapshot the enabled-saver set for new sessions, and mark
//! pre-Piggy sessions as the observational baseline.
//!
//! The measurement doc requires the saver set to be captured **at session
//! start** (we cannot change a session's config after it begins). The watcher
//! calls [`snapshot_new_session`] the moment a new `.jsonl` appears; indexing
//! calls [`tag_pre_install_baseline`] to backfill sessions that predate Piggy.

use anyhow::Result;

use crate::registry::Catalog;
use crate::state::{PiggyState, SaverState};
use crate::store::{source, SaverTag, Store};

/// The `session_savers.source` label for a saver, derived from who last toggled
/// it. A never-toggled (as-installed) saver counts as `rotation` (Piggy-managed).
fn source_for(s: &SaverState) -> &'static str {
    match s.last_toggle_source.as_deref() {
        Some(x) if x == source::MANUAL => source::MANUAL,
        Some(x) if x == source::HOLDOUT => source::HOLDOUT,
        _ => source::ROTATION,
    }
}

/// Snapshot the currently-installed saver set into `session_id`, unless it is
/// already tagged. Returns `true` if a snapshot was written.
///
/// Each installed saver contributes one `(saver_id, enabled, source)` row. A
/// session with no installed savers is still marked (an empty snapshot would be
/// indistinguishable from "untagged"), so we record nothing and report `false` —
/// there is genuinely nothing to attribute.
pub fn snapshot_new_session(
    store: &mut Store,
    state: &PiggyState,
    session_id: &str,
) -> Result<bool> {
    if store.has_session_savers(session_id)? {
        return Ok(false);
    }
    let tags: Vec<SaverTag> = state
        .savers
        .iter()
        .map(|(id, s)| SaverTag::new(id.clone(), s.enabled, source_for(s)))
        .collect();
    if tags.is_empty() {
        return Ok(false);
    }
    store.set_session_savers(session_id, &tags)?;
    Ok(true)
}

/// Backfill the `pre_install` baseline: every untagged session that started
/// before `state.created_at` is marked all-off across the catalog's savers.
///
/// Returns the number of sessions tagged. A `None` install time (Piggy has never
/// stamped one) means we cannot prove any session predates Piggy, so nothing is
/// tagged.
pub fn tag_pre_install_baseline(
    store: &mut Store,
    state: &PiggyState,
    catalog: &Catalog,
) -> Result<usize> {
    let Some(cutoff) = state.install_time() else {
        return Ok(0);
    };
    let saver_ids: Vec<String> = catalog.entries.iter().map(|e| e.id.clone()).collect();
    store.tag_pre_install(cutoff, &saver_ids)
}
