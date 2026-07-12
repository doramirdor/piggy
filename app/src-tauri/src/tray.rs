//! Menu-bar tray icon: a monochrome template pig glyph whose left-click shows or
//! hides the Piggy window. A convenience entry point for a windowed app that can
//! be closed (hidden) but keeps running in the background.

use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, Manager};

/// The monochrome tray template PNG (22×22@1x), tinted by macOS for light/dark
/// menu bars. Embedded at build time so there is no runtime path to resolve.
const TRAY_ICON_PNG: &[u8] = include_bytes!("../assets/tray-icon.png");

/// Toggle the main window's visibility, focusing it when shown.
pub fn toggle_panel(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("panel") {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
        } else {
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
