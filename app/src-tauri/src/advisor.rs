//! App-side wiring for the local advisor.
//!
//! Piggy ships with this switched off. Everything here is reachable only after
//! the user opts in, and every call degrades to "no annotations" rather than to
//! an error banner, because the deterministic findings in
//! [`piggy_core::insights`] are the product and the advisor is decoration on top.
//!
//! Two things live here that do not belong in the core crate: the download is a
//! long-running job that reports progress through Tauri events, and the loaded
//! model is process state that has to outlive a single command (a 2.5 GB load is
//! seconds, and paying it per annotation would be absurd).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use piggy_core::advisor::{self, download, facts::Facts};
use piggy_core::{config, sweep, Pricing, Store};

use crate::backend::ApiError;

/// Event channel for download progress. One channel, because only one download
/// can be in flight (see [`CANCEL`]).
pub const DOWNLOAD_EVENT: &str = "advisor://download";

/// Set to stop the in-flight download. Also acts as the "a download is running"
/// flag, so a second request cannot start a competing transfer onto the same
/// partial file.
static CANCEL: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static RUNNING: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn cancel_flag() -> &'static Arc<AtomicBool> {
    CANCEL.get_or_init(|| Arc::new(AtomicBool::new(false)))
}
fn running_flag() -> &'static Arc<AtomicBool> {
    RUNNING.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// One catalog entry as the picker needs it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvisorModelDto {
    pub id: String,
    pub name: String,
    pub blurb: String,
    /// Download size.
    pub bytes: u64,
    /// What it actually costs to run: weights plus KV cache plus compute
    /// buffers. The picker shows this, not the download size, because this is
    /// the number that decides whether the machine copes.
    pub peak_bytes: u64,
    pub context: u32,
    pub downloaded: bool,
}

/// Everything the Settings screen needs in one call.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvisorStatusDto {
    /// Whether this build can run a model at all.
    pub compiled_in: bool,
    pub host_ram_bytes: Option<u64>,
    /// What we are willing to let a model occupy on this host.
    pub budget_bytes: Option<u64>,
    /// `unsupported` | `off` | `needsDownload` | `ready`.
    pub state: String,
    /// Only models that fit. An entry that cannot run is not a choice.
    pub models: Vec<AdvisorModelDto>,
    pub selected_id: Option<String>,
    pub recommended_id: Option<String>,
}

/// An accepted annotation on its way to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationDto {
    pub insight_id: String,
    pub headline: String,
    pub why: String,
    /// Which model wrote it. The UI shows this: text generated locally by a 4B
    /// model must never look like it came from the same place as the receipt.
    pub model: String,
}

/// Download progress, emitted on [`DOWNLOAD_EVENT`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgressDto {
    pub model_id: String,
    pub received: u64,
    pub total: u64,
    pub done: bool,
    /// Set when the download failed or was cancelled. The UI shows it verbatim,
    /// which is why [`download::fetch`] writes errors in plain language.
    pub error: Option<String>,
}

fn selected() -> Option<String> {
    piggy_core::PiggyState::load()
        .ok()
        .and_then(|s| s.settings.advisor_model)
}

pub fn status() -> Result<AdvisorStatusDto, ApiError> {
    let ram = advisor::host_ram();
    let fitting = ram.map(advisor::available).unwrap_or_default();
    let sel = selected();

    let models: Vec<AdvisorModelDto> = fitting
        .iter()
        .map(|m| AdvisorModelDto {
            id: m.id.to_string(),
            name: m.name.to_string(),
            blurb: m.blurb.to_string(),
            bytes: m.bytes,
            peak_bytes: m.peak_bytes(),
            context: m.ctx,
            downloaded: m.present(),
        })
        .collect();

    // An unknown host budget is treated as "no", not as "probably fine": we
    // would rather withhold the feature than swap someone's machine.
    let state = if !advisor::compiled_in() || models.is_empty() {
        "unsupported"
    } else {
        match sel.as_deref().and_then(advisor::model) {
            None => "off",
            Some(m) if m.present() => "ready",
            Some(_) => "needsDownload",
        }
    };

    Ok(AdvisorStatusDto {
        compiled_in: advisor::compiled_in(),
        host_ram_bytes: ram,
        budget_bytes: ram.map(advisor::budget),
        state: state.to_string(),
        models,
        selected_id: sel,
        recommended_id: ram
            .and_then(advisor::recommended)
            .map(|m| m.id.to_string()),
    })
}

/// Opt in to `model_id`, or pass `None` to switch the advisor off.
///
/// Switching off deliberately leaves the weights on disk. Re-downloading 2.5 GB
/// because someone toggled a switch to see what it did is a bad trade; the
/// Settings screen offers an explicit delete for reclaiming the space.
pub fn select(model_id: Option<String>) -> Result<AdvisorStatusDto, ApiError> {
    if let Some(id) = &model_id {
        let m = advisor::model(id)
            .ok_or_else(|| ApiError::new("Unknown model", format!("no model named {id}"), false))?;
        let ram = advisor::host_ram();
        if !ram.map(|r| advisor::fits(m, r)).unwrap_or(false) {
            return Err(ApiError::new(
                "That model will not fit on this machine",
                format!(
                    "{} needs about {} GB to run and this machine has too little to spare.",
                    m.name,
                    m.peak_bytes() / 1_000_000_000
                ),
                false,
            ));
        }
    }
    (|| -> anyhow::Result<()> {
        let mut state = piggy_core::PiggyState::load()?;
        state.settings.advisor_model = model_id;
        state.save()?;
        Ok(())
    })()
    .map_err(|e| ApiError::new("Could not save your choice", e.to_string(), true))?;
    status()
}

/// Start downloading weights, reporting progress on [`DOWNLOAD_EVENT`].
///
/// Returns as soon as the transfer is running. The UI follows the events rather
/// than awaiting a multi-minute command.
pub fn start_download(app: AppHandle, model_id: String) -> Result<(), ApiError> {
    let m = advisor::model(&model_id).ok_or_else(|| {
        ApiError::new("Unknown model", format!("no model named {model_id}"), false)
    })?;

    // `swap` rather than load-then-store: two rapid clicks must not both win.
    if running_flag().swap(true, Ordering::SeqCst) {
        return Err(ApiError::new(
            "A download is already running",
            "Cancel the current download before starting another.".to_string(),
            false,
        ));
    }
    cancel_flag().store(false, Ordering::SeqCst);

    let cancel = Arc::clone(cancel_flag());
    let running = Arc::clone(running_flag());
    std::thread::spawn(move || {
        let emit = |received: u64, total: u64, done: bool, error: Option<String>| {
            let _ = app.emit(
                DOWNLOAD_EVENT,
                DownloadProgressDto {
                    model_id: m.id.to_string(),
                    received,
                    total,
                    done,
                    error,
                },
            );
        };

        // Throttled to whole percent: a per-megabyte event on a 2.5 GB file is
        // 2,500 IPC round trips the UI cannot use.
        let mut last_pct = u64::MAX;
        let result = download::fetch(m, &cancel, |received, total| {
            let pct = if total == 0 { 0 } else { received * 100 / total };
            if pct != last_pct {
                last_pct = pct;
                emit(received, total, false, None);
            }
        });

        match result {
            Ok(()) => emit(m.bytes, m.bytes, true, None),
            Err(e) => emit(0, m.bytes, true, Some(e.to_string())),
        }
        running.store(false, Ordering::SeqCst);
    });
    Ok(())
}

pub fn cancel_download() {
    cancel_flag().store(true, Ordering::SeqCst);
}

/// Delete a model's weights.
pub fn remove(model_id: String) -> Result<AdvisorStatusDto, ApiError> {
    let m = advisor::model(&model_id).ok_or_else(|| {
        ApiError::new("Unknown model", format!("no model named {model_id}"), false)
    })?;
    download::remove(m)
        .map_err(|e| ApiError::new("Could not delete the model", e.to_string(), true))?;
    // Deleting the weights of the selected model leaves the selection dangling,
    // which would render as "ready" with nothing to load.
    if selected().as_deref() == Some(model_id.as_str()) {
        return select(None);
    }
    status()
}

// ---------------------------------------------------------------------------
// annotation
// ---------------------------------------------------------------------------

/// Annotate the findings for `period`.
///
/// Returns an empty vector whenever the advisor cannot or should not run: not
/// compiled in, not opted into, not downloaded, or the model produced nothing
/// that survived the guard. The caller renders the deterministic findings in
/// every one of those cases, so none of them is an error.
pub fn annotate(period_s: String) -> Result<Vec<AnnotationDto>, ApiError> {
    let Some(spec) = selected().as_deref().and_then(advisor::model) else {
        return Ok(Vec::new());
    };
    if !spec.present() {
        return Ok(Vec::new());
    }

    let built = (|| -> anyhow::Result<Facts> {
        let home = config::piggy_home();
        let store = Store::open(&home)?;
        let period = crate::backend::period_from(&period_s);
        let cutoff = period.cutoff();
        let pricing = Pricing::load(&home);
        let ledger = store.ledger(cutoff.as_deref(), &pricing)?;
        let found = piggy_core::insights(&ledger);
        // The sweep is what upgrades advice from "trim your hooks" to naming the
        // skill that has not been invoked in 200 sessions. Its absence is not
        // fatal, so a failure here still produces a usable fact sheet.
        let swept = sweep::scan(&store, 200).ok();
        Ok(Facts::build(&ledger, &found, swept.as_ref()))
    })()
    .map_err(|e| ApiError::new("Could not assemble the report", e.to_string(), true))?;

    run_model(spec, &built)
}

/// Per-saver advice, keyed by `saver:<id>`.
///
/// Same contract as [`annotate`]: an empty vector whenever the advisor cannot or
/// should not run, because the deterministic per-saver summary on the row is the
/// product and this is prose on top of it.
pub fn explain_savers() -> Result<Vec<AnnotationDto>, ApiError> {
    let Some(spec) = selected().as_deref().and_then(advisor::model) else {
        return Ok(Vec::new());
    };
    if !spec.present() {
        return Ok(Vec::new());
    }

    let catalog = piggy_core::Catalog::embedded();
    let bundle = crate::backend::attribution_bundle()
        .map_err(|e| ApiError::new("Could not read the measurements", e.to_string(), true))?;
    // Catalog order, so the sheet's cap takes the savers the user sees first.
    let rows: Vec<_> = catalog
        .entries
        .iter()
        .filter_map(|e| Some((e, bundle.per_saver.get(&e.id)?)))
        .collect();
    let facts = Facts::savers(&rows);

    run_saver_model(spec, &facts)
}

#[cfg(feature = "local-llm")]
fn run_model(
    spec: &'static piggy_core::AdvisorModel,
    facts: &Facts,
) -> Result<Vec<AnnotationDto>, ApiError> {
    use piggy_core::advisor::llama::Advisor;

    // Loaded per call and dropped at the end of it. An earlier version cached the
    // model in a `static OnceLock` to save the ~1.6s load, and that **aborted the
    // app on quit**: Rust statics are never dropped, so the `LlamaModel` and its
    // Metal resource sets outlived ggml's own teardown, and Cmd+Q went
    //
    //   NSApplication terminate: -> exit() -> C++ static destructors
    //   -> ~vector<ggml_metal_device> -> GGML_ASSERT([rsets->data count] == 0)
    //
    // straight into `ggml_abort`. There is no catching that (see `llama.rs`), and
    // a crash report on every quit is not a fair price for a warm cache.
    //
    // Reloading costs about a second and a half. It also hands back roughly three
    // gigabytes between annotations, which on the 8GB machines this feature is
    // sized for is a benefit rather than a cost.
    let advisor = Advisor::load(spec)
        .map_err(|e| ApiError::new("Could not load the local model", e.to_string(), true))?;

    // A model that fails or times out costs the user nothing: the deterministic
    // findings render either way, so this is logged and swallowed.
    match advisor.annotate(facts) {
        Ok(list) => Ok(list
            .into_iter()
            .map(|a| AnnotationDto {
                insight_id: a.insight_id,
                headline: a.headline,
                why: a.why,
                model: spec.name.to_string(),
            })
            .collect()),
        Err(e) => {
            eprintln!("piggy: local advisor produced nothing usable: {e}");
            Ok(Vec::new())
        }
    }
}

#[cfg(feature = "local-llm")]
fn run_saver_model(
    spec: &'static piggy_core::AdvisorModel,
    facts: &Facts,
) -> Result<Vec<AnnotationDto>, ApiError> {
    use piggy_core::advisor::llama::Advisor;

    // Loaded and dropped per call, for the reason spelled out in `run_model`.
    let advisor = Advisor::load(spec)
        .map_err(|e| ApiError::new("Could not load the local model", e.to_string(), true))?;
    match advisor.explain_savers(facts) {
        Ok(list) => Ok(list
            .into_iter()
            .map(|a| AnnotationDto {
                insight_id: a.insight_id,
                headline: a.headline,
                why: a.why,
                model: spec.name.to_string(),
            })
            .collect()),
        Err(e) => {
            eprintln!("piggy: local advisor produced nothing usable for savers: {e}");
            Ok(Vec::new())
        }
    }
}

#[cfg(not(feature = "local-llm"))]
fn run_saver_model(
    _spec: &'static piggy_core::AdvisorModel,
    _facts: &Facts,
) -> Result<Vec<AnnotationDto>, ApiError> {
    Ok(Vec::new())
}

#[cfg(not(feature = "local-llm"))]
fn run_model(
    _spec: &'static piggy_core::AdvisorModel,
    _facts: &Facts,
) -> Result<Vec<AnnotationDto>, ApiError> {
    Ok(Vec::new())
}
