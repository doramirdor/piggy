//! Piggy — the Tauri v2 menu bar application.
//!
//! Tray-only (no dock icon): a 360×600 undecorated, transparent, popover-vibrancy
//! popover anchored under the menu-bar pig glyph. The Rust side links
//! `piggy-core` directly and exposes the [`commands`] surface to the React UI.
//! A background [`piggy_core::SessionWatcher`] snapshot-tags new sessions,
//! incrementally re-indexes on change, steps the rotation scheduler when a
//! session goes idle, and emits `piggy://stats-updated` so the panel and
//! menu-bar stay live.

mod backend;
mod commands;
mod tray;

use std::time::Duration;

use tauri::{Emitter, Manager, WindowEvent};

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

    // Autostart (launch-at-login) — desktop only.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder.plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ));
    }

    builder
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::environment,
            commands::stats_overview,
            commands::savers_list,
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
        ])
        .on_window_event(|window, event| {
            // Popover behaviour: hide when focus is lost (click-away), like a
            // native NSPanel menu extra.
            if let WindowEvent::Focused(false) = event {
                let _ = window.hide();
            }
        })
        .setup(|app| {
            // Tray-only: no dock icon on macOS.
            #[cfg(target_os = "macos")]
            {
                use tauri::ActivationPolicy;
                app.set_activation_policy(ActivationPolicy::Accessory);
            }

            // Popover vibrancy + rounded corners on the panel window. Popover
            // (unlike HudWindow, which is always dark) follows the system
            // light/dark appearance, matching the CSS prefers-color-scheme split.
            #[cfg(target_os = "macos")]
            {
                use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
                if let Some(win) = app.get_webview_window("panel") {
                    let _ = apply_vibrancy(&win, NSVisualEffectMaterial::Popover, None, Some(14.0));
                }
            }

            tray::setup(app)?;

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

    // Initial pass: index history, anchor the pre-install baseline, paint once.
    let _ = backend::reindex();
    backend::anchor_baseline();
    // Set up the first assignment if we're already idle (covers a restart mid-gap).
    let _ = backend::rotation_tick_if_enabled();
    let _ = handle.emit(STATS_UPDATED, ());

    let projects = config::claude_projects_dir();
    // Don't create ~/.claude/projects if Claude Code isn't installed — without a
    // history dir there is nothing to watch, so idle out rather than materialise it.
    if !projects.exists() {
        return;
    }
    let home = config::piggy_home();
    let pricing = Pricing::load(&home);
    let mut watcher = match SessionWatcher::new(projects, &home) {
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
