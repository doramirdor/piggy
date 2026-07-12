//! Piggy — the Tauri v2 menu bar application.
//!
//! Tray-only (no dock icon): a 360×600 undecorated, transparent, HUD-vibrancy
//! popover anchored under the menu-bar pig glyph. The Rust side links
//! `piggy-core` directly and exposes the [`commands`] surface to the React UI.
//! A lightweight background poller re-indexes recent sessions and emits
//! `piggy://stats-updated` so the panel and menu-bar stay live.

mod backend;
mod commands;
mod tray;

use std::time::Duration;

use tauri::{Emitter, Manager, WindowEvent};

/// Event emitted whenever the token index changes (new/updated sessions).
const STATS_UPDATED: &str = "piggy://stats-updated";

/// How often the background poller re-indexes recent sessions.
///
/// `notify` is not in this milestone's approved dependency set, so instead of a
/// filesystem watcher Piggy uses a debounced poll: cheap incremental re-index
/// (unchanged files are skipped by size+mtime) plus a re-index on window-show.
const POLL_INTERVAL: Duration = Duration::from_secs(15);

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

            // HUD vibrancy + rounded corners on the panel window.
            #[cfg(target_os = "macos")]
            {
                use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
                if let Some(win) = app.get_webview_window("panel") {
                    let _ =
                        apply_vibrancy(&win, NSVisualEffectMaterial::HudWindow, None, Some(14.0));
                }
            }

            tray::setup(app)?;

            // Kick an initial index + a background poller so stats stay live.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                // Initial pass on launch.
                let _ = backend::reindex();
                let _ = handle.emit(STATS_UPDATED, ());
                loop {
                    std::thread::sleep(POLL_INTERVAL);
                    if let Ok(rep) = backend::reindex() {
                        if rep.updated > 0 {
                            let _ = handle.emit(STATS_UPDATED, ());
                        }
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Piggy application");
}
