//! Menu-bar tray icon: a monochrome template pig glyph whose left-click shows or
//! hides the Piggy window. A convenience entry point for a windowed app that can
//! be closed (hidden) but keeps running in the background. Right-clicking opens a
//! menu to reopen the window or quit Piggy entirely.

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
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

/// Build the tray icon: left-click toggles the window, right-click opens a menu
/// with "Open Piggy" and "Quit Piggy". Quit exits the process (and its background
/// measurement daemon) rather than merely hiding the window.
pub fn setup(app: &App) -> tauri::Result<()> {
    let icon = Image::from_bytes(TRAY_ICON_PNG)?;

    let open = MenuItem::with_id(app, "open", "Open Piggy", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Piggy", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &sep, &quit])?;

    TrayIconBuilder::with_id("piggy-tray")
        .icon(icon)
        .icon_as_template(true)
        .menu(&menu)
        // Left-click toggles the window; the menu opens on right-click only.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => {
                if let Some(win) = app.get_webview_window("panel") {
                    let _ = win.show();
                    let _ = win.set_focus();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
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
