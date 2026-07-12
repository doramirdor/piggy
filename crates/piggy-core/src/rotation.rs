//! The rotation scheduler (M3).
//!
//! Per `docs/measurement.md`, we cannot change a session's config after it
//! starts, so A/B assignment happens **between** sessions. When the projects
//! directory is idle (no `.jsonl` written in the last [`IDLE_WINDOW_SECS`]), the
//! scheduler applies the next planned saver set so the *next* session picks it
//! up.
//!
//! The plan is a repeating block over installed, rotation-controlled savers:
//!
//! * one **holdout** slot (all savers off) — ~`holdout_fraction` of sessions,
//! * one **single-off** slot per saver (everything on except that saver),
//! * the **remainder** full-on.
//!
//! A saver the user toggled manually is *paused*: rotation never touches it
//! (`last_toggle_source == "manual"`), respecting the explicit choice. Rotation
//! applies via [`crate::engine::set_enabled_src`] with a non-manual source.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use walkdir::WalkDir;

use crate::engine;
use crate::registry::Catalog;
use crate::state::PiggyState;
use crate::store::{source, Store};

/// A session counts as "active" if any `.jsonl` was written within this window.
pub const IDLE_WINDOW_SECS: u64 = 10 * 60;

/// What a single rotation slot assigns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotKind {
    /// All savers off (the measurement holdout).
    Holdout,
    /// Everything on except this one saver.
    SingleOff(String),
    /// Everything on.
    FullOn,
}

/// A concrete assignment for one block position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub kind: SlotKind,
    /// Desired enabled state per rotation-controlled saver id.
    pub set: BTreeMap<String, bool>,
    /// The `session_savers.source` this assignment implies (`holdout`/`rotation`).
    pub source: &'static str,
}

/// A deterministic rotation plan over a fixed saver set.
#[derive(Debug, Clone)]
pub struct RotationPlan {
    /// Rotation-controlled savers, in catalog order.
    pub savers: Vec<String>,
    pub holdout_enabled: bool,
    pub holdout_fraction: f64,
    block_len: usize,
}

impl RotationPlan {
    /// Build a plan for `savers` (already ordered) with the given holdout policy.
    pub fn new(savers: Vec<String>, holdout_fraction: f64, holdout_enabled: bool) -> Self {
        let block_len = block_len(savers.len(), holdout_fraction, holdout_enabled);
        RotationPlan {
            savers,
            holdout_enabled,
            holdout_fraction,
            block_len,
        }
    }

    /// The number of session slots before the pattern repeats.
    pub fn block_len(&self) -> usize {
        self.block_len
    }

    /// The assignment for block position `pos` (wraps modulo the block length).
    /// Deterministic: identical inputs always yield identical output.
    pub fn assignment_at(&self, pos: usize) -> Assignment {
        let n = self.savers.len();
        let pos = pos % self.block_len.max(1);
        let holdout_slots = usize::from(self.holdout_enabled);

        if self.holdout_enabled && pos == 0 {
            return Assignment {
                kind: SlotKind::Holdout,
                set: self.savers.iter().map(|s| (s.clone(), false)).collect(),
                source: source::HOLDOUT,
            };
        }
        let idx = pos - holdout_slots; // 0-based slot after the holdout
        if idx < n {
            let off = &self.savers[idx];
            Assignment {
                kind: SlotKind::SingleOff(off.clone()),
                set: self.savers.iter().map(|s| (s.clone(), s != off)).collect(),
                source: source::ROTATION,
            }
        } else {
            Assignment {
                kind: SlotKind::FullOn,
                set: self.savers.iter().map(|s| (s.clone(), true)).collect(),
                source: source::ROTATION,
            }
        }
    }
}

/// Block length: at least enough for one holdout + one single-off per saver, and
/// large enough that the holdout is ~`holdout_fraction` of sessions.
fn block_len(n_savers: usize, holdout_fraction: f64, holdout_enabled: bool) -> usize {
    let target = if holdout_fraction > 0.0 {
        (1.0 / holdout_fraction).round() as usize
    } else {
        0
    };
    let min_needed = n_savers + usize::from(holdout_enabled);
    target.max(min_needed).max(1)
}

/// The rotation-controlled savers for `state`, in catalog order: installed
/// savers whose last toggle was **not** manual (manual savers are paused).
pub fn controlled_savers(catalog: &Catalog, state: &PiggyState) -> Vec<String> {
    let mut ids: Vec<String> = state
        .savers
        .iter()
        .filter(|(_, s)| s.last_toggle_source.as_deref() != Some(source::MANUAL))
        .map(|(id, _)| id.clone())
        .collect();
    // Order by catalog `ordering` (fall back to id) for a stable plan.
    ids.sort_by(|a, b| {
        let oa = catalog.get(a).map(|e| e.ordering).unwrap_or(i64::MAX);
        let ob = catalog.get(b).map(|e| e.ordering).unwrap_or(i64::MAX);
        oa.cmp(&ob).then_with(|| a.cmp(b))
    });
    ids
}

/// Wall-clock now as whole Unix seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether any `.jsonl` under `projects_dir` was modified within `window_secs`
/// of `now_secs` — i.e. a session is (probably) live and must not be perturbed.
pub fn is_session_active(projects_dir: &Path, now_secs: u64, window_secs: u64) -> bool {
    for entry in WalkDir::new(projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl")
        {
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(dur) = mtime.duration_since(UNIX_EPOCH) {
                        let m = dur.as_secs();
                        // Active if the file was touched at or after the window start.
                        if m + window_secs >= now_secs {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// The result of a scheduler tick.
#[derive(Debug, Clone)]
pub enum RotationOutcome {
    /// A session is live — nothing was changed.
    SkippedActive,
    /// No rotation-controlled savers — nothing to do.
    NothingToRotate,
    /// Applied `assignment` (at the recorded block position). `changed` lists the
    /// savers whose state was actually flipped.
    Applied {
        assignment: Assignment,
        changed: Vec<String>,
    },
}

/// Run one scheduler tick against the live environment.
///
/// Applies the next planned set only when the projects dir is idle. `now`/`window`
/// are injectable for tests; production passes [`wall_now`] and
/// [`IDLE_WINDOW_SECS`].
pub fn tick(
    catalog: &Catalog,
    store: &mut Store,
    projects_dir: &Path,
    now: u64,
    window_secs: u64,
) -> Result<RotationOutcome> {
    if is_session_active(projects_dir, now, window_secs) {
        return Ok(RotationOutcome::SkippedActive);
    }
    let state = PiggyState::load()?;
    let controlled = controlled_savers(catalog, &state);
    if controlled.is_empty() {
        return Ok(RotationOutcome::NothingToRotate);
    }
    let plan = RotationPlan::new(
        controlled,
        state.settings.holdout_fraction,
        state.settings.holdout_enabled,
    );
    let (block_pos, _) = store.rotation_state()?;
    let block_pos = block_pos.max(0) as usize;
    let assignment = plan.assignment_at(block_pos);

    // Apply: flip only savers whose current state differs. Manual savers were
    // already excluded from `controlled`, so this never overrides a user choice.
    let mut changed = Vec::new();
    for (id, &want) in &assignment.set {
        let cur = PiggyState::load()?
            .savers
            .get(id)
            .map(|s| s.enabled)
            .unwrap_or(false);
        if cur != want {
            engine::set_enabled_src(catalog, id, want, assignment.source)?;
            changed.push(id.clone());
        }
    }

    // Advance the cursor and preview the next set.
    let next_pos = (block_pos + 1) as i64;
    let next = plan.assignment_at(block_pos + 1);
    let planned_json = serde_json::to_string(&next.set).ok();
    store.set_rotation_state(next_pos, planned_json.as_deref())?;

    Ok(RotationOutcome::Applied {
        assignment,
        changed,
    })
}

/// Production entry: a tick against wall-clock now with the standard idle window.
pub fn tick_now(
    catalog: &Catalog,
    store: &mut Store,
    projects_dir: &Path,
) -> Result<RotationOutcome> {
    tick(catalog, store, projects_dir, now_secs(), IDLE_WINDOW_SECS)
}
