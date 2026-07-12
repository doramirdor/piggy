//! Tauri command surface (per `docs/m4-spec.md`). Every command is a thin async
//! wrapper that runs the `piggy-core`-touching work on a blocking task — the
//! engine mutates `settings.json` and must never run on the UI/main thread.

use serde::Deserialize;
use tauri::AppHandle;

use crate::backend::{
    self, ApiError, AppPrefs, DiscoverDto, DoctorDto, Environment, ReindexDto, RestoreDto,
    SaversState, ShareCardData, StatsOverview, SweepReportDto,
};

/// Run blocking `piggy-core` work off the main thread, flattening the join error.
async fn run<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(f).await {
        Ok(r) => r,
        Err(e) => Err(ApiError::new("Something went wrong", e.to_string(), false)),
    }
}

#[tauri::command]
pub async fn environment() -> Result<Environment, ApiError> {
    run(|| Ok(backend::environment())).await
}

#[tauri::command]
pub async fn stats_overview(period: String) -> Result<StatsOverview, ApiError> {
    run(move || backend::stats_overview(period)).await
}

#[tauri::command]
pub async fn savers_list() -> Result<SaversState, ApiError> {
    run(backend::savers_list).await
}

#[tauri::command]
pub async fn saver_toggle(id: String, on: bool) -> Result<SaversState, ApiError> {
    run(move || backend::saver_toggle(id, on)).await
}

#[tauri::command]
pub async fn master_toggle(on: bool) -> Result<SaversState, ApiError> {
    run(move || backend::master_toggle(on)).await
}

#[tauri::command]
pub async fn sweep_report() -> Result<SweepReportDto, ApiError> {
    run(backend::sweep_report).await
}

#[tauri::command]
pub async fn sweep_apply(item_ids: Vec<String>) -> Result<SweepReportDto, ApiError> {
    run(move || backend::sweep_apply(item_ids)).await
}

#[tauri::command]
pub async fn sweep_restore(item_ids: Vec<String>) -> Result<SweepReportDto, ApiError> {
    run(move || backend::sweep_restore(item_ids)).await
}

#[tauri::command]
pub async fn discovered_list() -> Result<DiscoverDto, ApiError> {
    run(|| Ok(backend::discovered_list())).await
}

/// Manual "check now": force a live GitHub search past the once-a-day cache.
#[tauri::command]
pub async fn refresh_discovered() -> Result<DiscoverDto, ApiError> {
    run(|| Ok(backend::refresh_discovered())).await
}

#[tauri::command]
pub async fn share_card_data(period: String) -> Result<ShareCardData, ApiError> {
    run(move || backend::share_card_data(period)).await
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub path: String,
}

#[tauri::command]
pub async fn save_share_card(app: AppHandle, png_base64: String) -> Result<SaveResult, ApiError> {
    use tauri_plugin_opener::OpenerExt;
    let path = run(move || backend::save_share_card(png_base64)).await?;
    // Reveal in Finder; a failure to reveal is non-fatal (the file is saved).
    let _ = app.opener().reveal_item_in_dir(&path);
    Ok(SaveResult {
        path: path.to_string_lossy().into_owned(),
    })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub holdout_fraction: f64,
    pub rotation_enabled: bool,
    pub launch_at_login: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsInput {
    pub holdout_fraction: f64,
    pub rotation_enabled: bool,
    pub launch_at_login: bool,
}

fn launch_at_login_enabled(app: &AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
pub async fn settings_get(app: AppHandle) -> Result<Settings, ApiError> {
    let prefs = run(|| Ok(backend::load_prefs())).await?;
    Ok(Settings {
        holdout_fraction: prefs.holdout_fraction,
        rotation_enabled: prefs.rotation_enabled,
        launch_at_login: launch_at_login_enabled(&app),
    })
}

#[tauri::command]
pub async fn settings_set(app: AppHandle, settings: SettingsInput) -> Result<Settings, ApiError> {
    use tauri_plugin_autostart::ManagerExt;
    let prefs = AppPrefs {
        holdout_fraction: settings.holdout_fraction,
        rotation_enabled: settings.rotation_enabled,
    };
    run(move || backend::save_prefs(&prefs)).await?;

    let al = app.autolaunch();
    let _ = if settings.launch_at_login {
        al.enable()
    } else {
        al.disable()
    };

    let saved = run(|| Ok(backend::load_prefs())).await?;
    Ok(Settings {
        holdout_fraction: saved.holdout_fraction,
        rotation_enabled: saved.rotation_enabled,
        launch_at_login: launch_at_login_enabled(&app),
    })
}

#[tauri::command]
pub async fn restore_defaults() -> Result<RestoreDto, ApiError> {
    run(backend::restore_defaults).await
}

#[tauri::command]
pub async fn doctor() -> Result<DoctorDto, ApiError> {
    run(|| Ok(backend::doctor())).await
}

#[tauri::command]
pub async fn reindex() -> Result<ReindexDto, ApiError> {
    run(backend::reindex).await
}

/// Open an external URL in the user's browser (used for "View on GitHub").
#[tauri::command]
pub async fn open_external(app: AppHandle, url: String) -> Result<(), ApiError> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| ApiError::new("Couldn't open the link", e.to_string(), false))
}
