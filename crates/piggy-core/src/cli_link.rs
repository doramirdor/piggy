//! The managed `piggy` command-line link.
//!
//! The app bundle ships the `piggy` CLI as a sidecar next to its own
//! executable. Command-line users opt in from Settings, which points
//! `<piggy_home>/bin/piggy` at that sidecar and puts the directory on `PATH`.
//! That is the same directory, and the same delimited `PATH` block, that savers
//! like rtk already use, so a user who turns on both still gets exactly one
//! managed line in their shell profile.
//!
//! The link is a symlink rather than a copy so it can never drift from the app
//! it shipped with. [`install`] re-points it unconditionally, which self-heals
//! the link after the user moves or replaces Piggy.app.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config;
use crate::engine;
use crate::state::PiggyState;

/// Path of the managed CLI symlink: `<piggy_home>/bin/piggy`.
pub fn link_path() -> PathBuf {
    config::piggy_bin_dir().join("piggy")
}

/// Whether the managed CLI link is present.
///
/// Deliberately uses `symlink_metadata`, so a *dangling* link (Piggy.app moved
/// or deleted) still counts as present: it is ours to repair or remove, and the
/// managed `PATH` block has to outlive a saver uninstall either way.
pub fn exists() -> bool {
    std::fs::symlink_metadata(link_path()).is_ok()
}

/// What [`install`] actually changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReport {
    /// The managed link itself (`<piggy_home>/bin/piggy`).
    pub link: PathBuf,
    /// The resolved binary the link points at.
    pub target: PathBuf,
    /// The link was created or re-pointed (`false` = it was already correct).
    pub linked: bool,
    /// A managed `PATH` block was appended to the shell profile.
    pub path_added: bool,
    /// The shell profile considered.
    pub profile: PathBuf,
}

/// Point `<piggy_home>/bin/piggy` at `target` and ensure that directory is on
/// `PATH`.
///
/// Idempotent: re-running with the same target reports `linked: false` and
/// leaves the profile untouched.
pub fn install(target: &Path) -> Result<LinkReport> {
    let target = target
        .canonicalize()
        .with_context(|| format!("resolving the piggy CLI at {}", target.display()))?;
    let bin = config::piggy_bin_dir();
    std::fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    let link = link_path();

    let linked = if std::fs::read_link(&link).ok().as_deref() == Some(target.as_path()) {
        false
    } else {
        // Replace whatever is there: a stale link from a previous app location,
        // or a plain copy left by an older build.
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link)
                .with_context(|| format!("replacing {}", link.display()))?;
        }
        std::os::unix::fs::symlink(&target, &link)
            .with_context(|| format!("linking {} to {}", link.display(), target.display()))?;
        true
    };

    let profile = config::shell_profile_path();
    let path_added = engine::ensure_path_block(&profile, &bin.to_string_lossy())
        .with_context(|| format!("adding {} to PATH via {}", bin.display(), profile.display()))?;

    Ok(LinkReport {
        link,
        target,
        linked,
        path_added,
        profile,
    })
}

/// Remove the CLI link.
///
/// The managed `PATH` block goes with it, unless an installed saver still keeps
/// a binary in `<piggy_home>/bin` (removing it then would break that saver).
/// Returns `true` if a link was actually removed.
pub fn uninstall() -> Result<bool> {
    let link = link_path();
    let existed = std::fs::symlink_metadata(&link).is_ok();
    if existed {
        std::fs::remove_file(&link).with_context(|| format!("removing {}", link.display()))?;
    }

    // A missing/unreadable state file means no savers are installed, so the
    // PATH line is ours alone to drop.
    let savers_need_bin = PiggyState::load()
        .map(|state| engine::any_saver_uses_bin_dir(&state, None))
        .unwrap_or(false);
    if !savers_need_bin {
        let profile = config::shell_profile_path();
        engine::remove_path_block(&profile)
            .with_context(|| format!("removing Piggy PATH block from {}", profile.display()))?;
    }
    Ok(existed)
}
