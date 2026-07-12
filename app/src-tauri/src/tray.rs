//! Menu-bar tray icon: a monochrome template pig glyph whose left-click toggles
//! the popover panel, anchored under the tray icon via `tauri-plugin-positioner`.

use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, Manager};
use tauri_plugin_positioner::{Position, WindowExt};

/// The monochrome tray template PNG (22×22@1x), tinted by macOS for light/dark
/// menu bars. Embedded at build time so there is no runtime path to resolve.
const TRAY_ICON_PNG: &[u8] = include_bytes!("../assets/tray-icon.png");

/// Show/hide the panel, re-anchoring it under the tray icon each time it opens.
pub fn toggle_panel(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("panel") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
        } else {
            let _ = win.move_window(Position::TrayCenter);
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
}

/// Build the tray icon and wire its left-click to [`toggle_panel`].
pub fn setup(app: &App) -> tauri::Result<()> {
    let icon = Image::from_bytes(TRAY_ICON_PNG)?;
    TrayIconBuilder::with_id("piggy-tray")
        .icon(icon)
        .icon_as_template(true)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            // Keep the positioner's cached tray rect fresh so TrayCenter is exact.
            tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_panel(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}
