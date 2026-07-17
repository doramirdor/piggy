//! Piggy — the Tauri v2 desktop application.
//!
//! A regular windowed macOS app (940×660, resizable, overlay title bar) with a
//! Dock icon and a companion menu-bar tray icon that re-opens the window.
//! Closing the window hides it and keeps the background daemon running. The Rust
//! side links `piggy-core` directly and exposes the [`commands`] surface to the
//! React UI.
//! A background [`piggy_core::SessionWatcher`] snapshot-tags new sessions,
//! incrementally re-indexes on change, steps the rotation scheduler when a
//! session goes idle, and emits `piggy://stats-updated` so the panel and
//! menu-bar stay live.

mod backend;
mod commands;
mod tray;

use std::time::Duration;

use tauri::{Emitter, WindowEvent};

/// Event emitted whenever the token index changes (new/updated sessions) or a
/// rotation step lands.
const STATS_UPDATED: &str = "piggy://stats-updated";

/// How long each watcher tick blocks waiting for filesystem activity before it
/// loops (also the effective idle-poll cadence when nothing is happening).
const WATCH_TICK: Duration = Duration::from_secs(2);

/// Backoff after a watcher error before retrying, so a transient failure can't
/// spin the loop hot.
const WATCH_RETRY: Duration = Duration::from_secs(5);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();

    // Autostart (launch-at-login) and the updater — desktop only.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder
            .plugin(tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                None,
            ))
            .plugin(tauri_plugin_updater::Builder::new().build());
    }

    builder
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::environment,
            commands::stats_overview,
            commands::sources_overview,
            commands::usage_series,
            commands::savers_list,
            commands::saver_config_get,
            commands::saver_config_set,
            commands::saver_toggle,
            commands::master_toggle,
            commands::sweep_report,
            commands::sweep_apply,
            commands::sweep_restore,
            commands::discovered_list,
            commands::refresh_discovered,
            commands::share_card_data,
            commands::save_share_card,
            commands::settings_get,
            commands::settings_set,
            commands::restore_defaults,
            commands::doctor,
            commands::reindex,
            commands::open_external,
            commands::check_for_update,
            commands::install_update,
        ])
        .on_window_event(|window, event| {
            // Desktop-window behaviour: closing the window keeps Piggy running
            // (background measurement + tray) rather than quitting — the tray
            // icon reopens it. Standard macOS menu-utility pattern.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            // Regular activation: Piggy is a normal windowed app with a Dock icon.
            #[cfg(target_os = "macos")]
            {
                use tauri::ActivationPolicy;
                app.set_activation_policy(ActivationPolicy::Regular);
            }

            tray::setup(app)?;

            // Re-point an opted-in `piggy` CLI link at this build's sidecar, so
            // it survives the user moving or replacing Piggy.app. No-op unless
            // they turned the CLI on, so launching never edits a shell profile
            // uninvited.
            backend::refresh_cli_link();

            // Background daemon: initial index + anchor, then a live filesystem
            // watcher that snapshot-tags new sessions, re-indexes on change, and
            // steps rotation once each session goes idle.
            let handle = app.handle().clone();
            std::thread::spawn(move || background_loop(handle));

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Piggy application");
}

/// The background measurement daemon.
///
/// Runs an initial index + baseline anchor, then drives a [`SessionWatcher`]:
/// each tick snapshot-tags any brand-new session and incrementally re-indexes
/// touched files. When a session's writes stop (the watcher goes quiet after a
/// burst), we step the rotation scheduler once — `rotation::tick_now` self-gates
/// on the 10-minute idle window, so the next session picks up the next planned
/// saver set without ever perturbing a live one. A stats-updated event fires on
/// any change so the panel and menu-bar refresh.
fn background_loop(handle: tauri::AppHandle) {
    use piggy_core::{config, Pricing, SessionWatcher};

    // Initial pass: index history (Claude Code + Codex), anchor the pre-install
    // baseline, paint once.
    let _ = backend::reindex();
    backend::anchor_baseline();
    // Set up the first assignment if we're already idle (covers a restart mid-gap).
    let _ = backend::rotation_tick_if_enabled();
    let _ = handle.emit(STATS_UPDATED, ());

    // Watch every session-log root that exists: Claude Code's projects dir plus
    // Codex's sessions dirs. Nothing is created if a tool isn't installed —
    // without a history dir there is nothing to watch, so idle out rather than
    // materialise one.
    let roots = piggy_core::default_roots();
    if roots.is_empty() {
        return;
    }
    let home = config::piggy_home();
    let pricing = Pricing::load(&home);
    let mut watcher =
        match SessionWatcher::with_roots(roots, &home, piggy_core::WatchBackend::Native) {
            Ok(w) => w,
            Err(_) => return,
        };

    // Edge-triggered rotation: a session wrote (`pending_rotation`), so once the
    // dir falls idle we apply exactly one rotation step — never re-ticking during
    // the idle gap, which would churn the saver set and settings.json.
    let mut pending_rotation = false;
    loop {
        match watcher.tick(WATCH_TICK, &pricing) {
            Ok(events) => {
                if !events.is_empty() {
                    // The tick incrementally re-indexed the touched files, so the
                    // cached attribution bundle is now stale. Invalidate it before
                    // telling the UI to refresh — otherwise the dashboard keeps
                    // reading the frozen startup bundle (headline multiplier and
                    // saver badges never move until a rotation tick or restart).
                    backend::bump_attr_version();
                    pending_rotation = true;
                    let _ = handle.emit(STATS_UPDATED, ());
                } else if pending_rotation && backend::rotation_tick_if_enabled() {
                    pending_rotation = false;
                    let _ = handle.emit(STATS_UPDATED, ());
                }
            }
            Err(_) => std::thread::sleep(WATCH_RETRY),
        }
    }
}
