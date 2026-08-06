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

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(any(feature = "local-llm", test))]
use std::sync::{MutexGuard, TryLockError};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use piggy_core::advice::{self, Candidate, Prerequisite};
use piggy_core::advisor::{self, cache, download, facts, facts::Facts, guard};
use piggy_core::attribution::SaverAttribution;
use piggy_core::{config, probe, sweep, Pricing, Store};

use crate::backend::ApiError;

/// Event channel for download progress. One channel, because only one download
/// can be in flight (see [`CANCEL`]).
pub const DOWNLOAD_EVENT: &str = "advisor://download";

/// Emitted when an advice pass has landed in the cache, and only then.
///
/// Its own channel rather than `stats-updated`, which fires on the watcher's
/// 400ms debounce and pulls five other readings with it. A pass lands once every
/// minute or two at most, and the only thing that needs to know is the advice
/// sheet, which is showing the deterministic order and a "no rewrite yet" note
/// until it hears this.
pub const ADVICE_EVENT: &str = "advice://updated";

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
            let pct = (received * 100).checked_div(total).unwrap_or(0);
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
        // Deliberately the rolling `cutoff`, not `day_cutoff`: this ledger is
        // model input (a fact sheet), not a number rendered beside a chart, so
        // it doesn't need to reconcile with drawn bars and the rolling window
        // is the fresher summary of recent usage.
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

/// The single inference slot. Weights are roughly three gigabytes resident and
/// [`advisor::fits`] sizes the host for exactly one copy, so runs are serialised
/// process wide rather than left to the frontend: `annotatedPeriod` and
/// `saverNotesFor` are independent guards, so a Ledger annotation and a Proof
/// saver note can otherwise land on two `spawn_blocking` threads at once.
///
/// Compiled without the feature too, so the slot keeps its test in the default
/// build that ships.
#[cfg(any(feature = "local-llm", test))]
static INFERENCE: OnceLock<Mutex<()>> = OnceLock::new();

/// Take the inference slot, or `None` when another run already holds it.
///
/// Poisoning is deliberately ignored: the guarded value is `()`, so a run that
/// panicked left nothing to corrupt, and switching the advisor off for the rest
/// of the process would be a worse answer than letting the next call try.
#[cfg(any(feature = "local-llm", test))]
fn claim_inference() -> Option<MutexGuard<'static, ()>> {
    match INFERENCE.get_or_init(|| Mutex::new(())).try_lock() {
        Ok(slot) => Some(slot),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[cfg(feature = "local-llm")]
fn run_model(
    spec: &'static piggy_core::AdvisorModel,
    facts: &Facts,
) -> Result<Vec<AnnotationDto>, ApiError> {
    use piggy_core::advisor::llama::Advisor;

    // Refused rather than queued: a second load would double the resident
    // weights, and an answer that arrives after a queued twenty second run is
    // about a period the user has already left. Refused as an ERROR, not an
    // empty Ok: the store claims its guard key before awaiting, and records an
    // Ok as answered, so an empty success would suppress the prose until the
    // reading moves. The catch path re-arms silently, which is the retry.
    //
    // Claimed before the model is loaded, so drop order frees the weights first
    // and the slot only afterwards: the next run never overlaps this one.
    let Some(_slot) = claim_inference() else {
        return Err(ApiError::new(
            "The local advisor is busy",
            "another annotation pass holds the inference slot; the caller re-arms and retries",
            false,
        ));
    };

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

    // One run at a time, and busy is an error rather than an empty answer, for
    // the reasons spelled out in `run_model`.
    let Some(_slot) = claim_inference() else {
        return Err(ApiError::new(
            "The local advisor is busy",
            "another annotation pass holds the inference slot; the caller re-arms and retries",
            false,
        ));
    };

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

// ---------------------------------------------------------------------------
// The advice pass
// ---------------------------------------------------------------------------

/// One ranked candidate, as the sheet renders it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvicePickDto {
    /// A candidate id from the list this overlay came back with.
    pub id: String,
    /// The model's sentence about it, already length-capped and already checked
    /// against the numbers allow-list.
    pub why: String,
}

/// Picks the model grouped, so the sheet can offer them as one apply.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceBundleDto {
    pub project: String,
    pub ids: Vec<String>,
}

/// What the model added on top of the deterministic advice list.
///
/// Every field degrades to "nothing": no advisor, no model, a busy slot or a
/// refused answer all produce an overlay whose `picks` are empty, and the sheet
/// renders the deterministic order with house copy. That is the fallback the
/// spec requires, not an error state.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdviceOverlayDto {
    /// The fact sheet this describes. Not rendered; it is the handle a log line
    /// or a test needs to tell one pass from another.
    pub facts_hash: String,
    /// A pass is running or has just been started. Render the deterministic
    /// order and ask again on the next `piggy://stats-updated`.
    pub pending: bool,
    /// Which model wrote these sentences. The UI shows it: locally generated
    /// prose must never look like it came from the same place as the receipt.
    pub model: Option<String>,
    /// Candidate ids in the model's order, best first. Ids the model did not
    /// rank are not here; they keep their deterministic order behind these.
    pub picks: Vec<AdvicePickDto>,
    pub bundles: Vec<AdviceBundleDto>,
    /// Candidate ids that have a drafted rewrite waiting in memory. Applying one
    /// goes through [`attach_cached_drafts`].
    pub drafted: Vec<String>,
}

/// Where a candidate's drafted rewrite has got to.
///
/// One string cannot be right in all of these, and shipping one was a live
/// honesty defect: a user who had already switched the advisor on, and whose
/// draft the guard then refused, was told to switch the advisor on. The burden
/// figure on the card is the insight and it is honest in every state; only this
/// sentence changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DraftState {
    /// This build cannot run a model, no model is selected, or its weights are
    /// not on disk. The only state "turn on the local advisor" is true in.
    Unavailable,
    /// The advisor can run and has not answered for this file yet. Either a
    /// pass is in flight or the next UI read starts one.
    Pending,
    /// The advisor ran this file and nothing it wrote cleared
    /// [`piggy_core::advisor::draft::accept_draft`]. Blocked, and not something
    /// the user can act on: the shrink threshold is deliberate, and lowering a
    /// guard so a demo succeeds is the failure mode M5 spent itself fighting.
    Refused,
    /// A validated rewrite is attached, the diff opens, and Apply writes it.
    Ready,
}

impl DraftState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DraftState::Unavailable => "unavailable",
            DraftState::Pending => "pending",
            DraftState::Refused => "refused",
            DraftState::Ready => "ready",
        }
    }
}

/// What the advisor can say about drafts right now, read once per report.
///
/// Reads the cache and never starts a pass, so every command that returns an
/// advice report can label its cards without paying for a fact sheet.
pub struct DraftStatus {
    /// This build can run a model, one is selected, and its weights are present.
    advisor_ready: bool,
    /// The selected model's id: half of a [`cache::draft_key`].
    model_id: Option<&'static str>,
    /// Every draft key the cached pass tried, drafted or refused.
    attempted: BTreeSet<String>,
}

impl DraftStatus {
    /// Read the advisor's state and whatever pass is cached.
    pub fn read() -> DraftStatus {
        // A build with no inference linked in has nothing to look up: no cache
        // can hold a pass it cannot run, and reading the state file to learn
        // that would be work for an answer that is already known.
        if !advisor::compiled_in() {
            return DraftStatus {
                advisor_ready: false,
                model_id: None,
                attempted: BTreeSet::new(),
            };
        }
        let spec = selected().as_deref().and_then(advisor::model);
        let advisor_ready =
            advisor::compiled_in() && spec.map(|m| m.present()).unwrap_or(false);
        let attempted = advice_cache()
            .lock()
            .ok()
            .and_then(|c| c.current().map(|o| o.attempted.clone()))
            .unwrap_or_default();
        DraftStatus {
            advisor_ready,
            model_id: spec.map(|m| m.id),
            attempted,
        }
    }

    /// A status with no advisor behind it.
    ///
    /// The honest answer for a build with the feature off, and what a test that
    /// is not exercising the advisor wants: it reads no state file, no cache and
    /// no home directory.
    #[cfg(test)]
    pub fn none() -> DraftStatus {
        DraftStatus {
            advisor_ready: false,
            model_id: None,
            attempted: BTreeSet::new(),
        }
    }

    /// A status as it looks after a pass that tried `attempted`.
    #[cfg(test)]
    pub fn after_pass(model_id: &'static str, attempted: BTreeSet<String>) -> DraftStatus {
        DraftStatus {
            advisor_ready: true,
            model_id: Some(model_id),
            attempted,
        }
    }

    /// The state a UI may claim for `candidate`, or `None` when this kind never
    /// needed a draft in the first place.
    pub fn of(&self, candidate: &Candidate) -> Option<DraftState> {
        if !candidate
            .prerequisites
            .iter()
            .any(|p| matches!(p, Prerequisite::NeedsAdvisor))
        {
            return None;
        }
        // Attached, so a pass produced one and the guard accepted it. Checked
        // first because it is the one state that is true of the candidate in
        // hand rather than of the machine around it.
        if candidate.new_content.is_some() {
            return Some(DraftState::Ready);
        }
        if !self.advisor_ready {
            return Some(DraftState::Unavailable);
        }
        // Keyed by the file's hash, not by the fact sheet: a pass that ran over
        // an older ledger still tried this exact file, and a pass that has not
        // seen this file yet is pending however old the overlay is.
        let attempted = match (self.model_id, candidate.source_hash()) {
            (Some(model), Some(hash)) => self
                .attempted
                .contains(&cache::draft_key(model, &candidate.id, hash)),
            _ => false,
        };
        Some(if attempted {
            DraftState::Refused
        } else {
            DraftState::Pending
        })
    }
}

/// The whole advice sheet: the deterministic candidates, in the model's order
/// when there is one, plus the overlay that explains them.
///
/// **This is the call the app should make**, rather than
/// [`piggy_core::advice::generate`] followed by a second pass for the overlay:
/// the fact sheet is built from the same [`piggy_core::advice::Inputs`] the
/// generators ran over, and that load is the app's heaviest read.
///
/// The model never reorders anything here by itself: `apply_llm_order` moves
/// rows the guard already accepted, and a candidate the model ignored keeps its
/// place among the other unranked ones.
pub fn advice_sheet(app: AppHandle) -> Result<(Vec<Candidate>, AdviceOverlayDto), ApiError> {
    let home = config::piggy_home();
    let (mut store, mut candidates, facts) = (|| -> anyhow::Result<(Store, Vec<Candidate>, Facts)> {
        let mut store = Store::open(&home)?;
        let pricing = Pricing::load(&home);
        let state = piggy_core::PiggyState::load()?;
        let catalog = piggy_core::Catalog::embedded();
        let opts = advice::GenerateOptions::new(&catalog, &pricing, &state);

        let inputs = advice::load_inputs(&mut store, &opts)?;
        let candidates = advice::reconcile(&mut store, advice::generate_from(&inputs))?;
        let facts = build_facts(&store, &pricing, &inputs, &candidates)?;
        Ok((store, candidates, facts))
    })()
    .map_err(|e| ApiError::new("Could not assemble the advice", e.to_string(), true))?;

    let overlay = advice_overlay(app, &facts, &candidates);

    // Provenance, and the one field the generator could not fill: it does not
    // know what the advisor was shown. Stamped only once a model has actually
    // read this sheet, because a hash written when no model ran would claim a
    // provenance that never happened. A hash of the payload is not the payload,
    // so nothing about "contents never enter the DB" is bent here.
    if overlay.model.is_some() && !overlay.pending {
        let hash = facts.hash();
        for id in &facts.candidate_ids {
            let _ = store.set_advice_facts_hash(id, &hash);
        }
    }

    if let Some(picks) = overlay_picks(&overlay) {
        advice::apply_llm_order(&mut candidates, &picks);
    }
    Ok((candidates, overlay))
}

/// Attach every cached draft to the candidates that have one, returning how many
/// landed.
///
/// Apply has to run through this first: a draft lives in this process only (it
/// is derived from a CLAUDE.md's contents, which never enter the database), and
/// [`piggy_core::advice::Candidate::new_content`] is what clears
/// [`piggy_core::advice::Candidate::blocked`]. A candidate with no cached draft
/// stays blocked, which is the deterministic presentation.
pub fn attach_cached_drafts(candidates: &mut [Candidate]) -> usize {
    let Some(spec) = selected().as_deref().and_then(advisor::model) else {
        return 0;
    };
    let Ok(cache) = advice_cache().lock() else {
        return 0;
    };
    let Some(overlay) = cache.current() else {
        return 0;
    };
    let mut n = 0;
    for c in candidates.iter_mut() {
        let Some(source) = c.source_hash() else {
            continue;
        };
        let key = cache::draft_key(spec.id, &c.id, source);
        let Some(draft) = overlay.drafts.get(&key) else {
            continue;
        };
        if advice::attach_draft(c, &draft.text, draft.had_bom).is_ok() {
            n += 1;
        }
    }
    n
}

/// Forget the cached overlay, so the next read re-runs the pass.
///
/// The whole of "Refresh advice": the candidates are regenerated every read
/// anyway, and `advice::generate` already handles the lifecycle transitions.
pub fn refresh_advice() {
    if let Ok(mut cache) = advice_cache().lock() {
        cache.clear();
    }
}

/// The fact sheet for one advice pass.
fn build_facts(
    store: &Store,
    pricing: &Pricing,
    inputs: &advice::Inputs,
    candidates: &[Candidate],
) -> anyhow::Result<Facts> {
    let month = piggy_core::Period::Month.cutoff();
    let ledger = store.ledger(month.as_deref(), pricing)?;
    let found = piggy_core::insights(&ledger);

    // Two adjacent windows for the floor trend, so a recent change is not
    // damped by being inside the window it is compared against.
    let recent_from = days_ago(facts::TREND_RECENT_DAYS);
    let prior_from = days_ago(facts::TREND_RECENT_DAYS + facts::TREND_PRIOR_DAYS);
    let recent = store.ledger_between(Some(&recent_from), None, pricing)?;
    let prior = store.ledger_between(Some(&prior_from), Some(&recent_from), pricing)?;

    // Catalog order, so the sheet's cap takes the savers the user sees first.
    // The bundle's map is a `HashMap`, and iterating one into a payload whose
    // hash is a cache key would move the key on every run.
    let catalog = piggy_core::Catalog::embedded();
    let attrs = crate::backend::attribution_bundle().ok();
    let savers: Vec<(&piggy_core::registry::Entry, bool, &SaverAttribution)> = catalog
        .entries
        .iter()
        .filter_map(|e| {
            let saver = inputs.savers.iter().find(|s| s.entry.id == e.id)?;
            Some((e, saver.enabled, &saver.attribution))
        })
        .collect();

    Ok(Facts::advice(&facts::AdviceInput {
        ledger: &ledger,
        trend: Some(facts::FloorTrend {
            recent: &recent,
            prior: &prior,
        }),
        insights: &found,
        sweep: Some(&inputs.sweep),
        manifests: &inputs.manifests,
        server_usage: &inputs.server_usage,
        claudemd: &inputs.claudemd,
        project_mcp: &inputs.project_mcp,
        savers: &savers,
        headline: attrs.as_ref().map(|b| &b.headline),
        candidates,
    }))
}

/// An RFC3339 instant `days` in the past, in the form the sessions table stores.
fn days_ago(days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339()
}

/// The overlay's picks in the shape [`advice::apply_llm_order`] wants.
fn overlay_picks(overlay: &AdviceOverlayDto) -> Option<Vec<guard::Pick>> {
    if overlay.picks.is_empty() {
        return None;
    }
    Some(
        overlay
            .picks
            .iter()
            .map(|p| guard::Pick {
                id: p.id.clone(),
                rationale: p.why.clone(),
            })
            .collect(),
    )
}

/// The in-process overlay cache. Memory only, and deliberately so: a draft is
/// derived from a CLAUDE.md's contents, which the spec says never enter the
/// database.
static ADVICE_CACHE: OnceLock<Mutex<cache::AdviceCache>> = OnceLock::new();
/// Set while a pass is in flight, so a burst of UI reads starts one worker.
/// Only the inference build ever has a pass to be in flight.
#[cfg_attr(not(feature = "local-llm"), allow(dead_code))]
static ADVICE_RUNNING: AtomicBool = AtomicBool::new(false);

fn advice_cache() -> &'static Mutex<cache::AdviceCache> {
    ADVICE_CACHE.get_or_init(|| Mutex::new(cache::AdviceCache::default()))
}

/// Look the overlay up, and start a pass when there is none.
///
/// Pull, not push (docs/m5-spec.md: "no tray badge, no notifications, no
/// auto-refresh nags"): the worker is started by a UI read and never by a timer,
/// and a read that finds nothing renders the deterministic order rather than
/// waiting.
fn advice_overlay(app: AppHandle, facts: &Facts, candidates: &[Candidate]) -> AdviceOverlayDto {
    let hash = facts.hash();
    // A build with no inference linked in has no pass to wait for. Without this
    // it would answer `pending` for ever, and a card would say a rewrite was on
    // its way from a model this binary cannot load.
    if !advisor::compiled_in() {
        return AdviceOverlayDto {
            facts_hash: hash,
            ..Default::default()
        };
    }
    let Some(spec) = selected().as_deref().and_then(advisor::model) else {
        return AdviceOverlayDto {
            facts_hash: hash,
            ..Default::default()
        };
    };
    if !spec.present() {
        return AdviceOverlayDto {
            facts_hash: hash,
            ..Default::default()
        };
    }

    let key = cache::advice_key(spec.id, &hash);
    if let Ok(cache) = advice_cache().lock() {
        if let Some(overlay) = cache.get(&key) {
            return dto_from_overlay(spec, overlay);
        }
    }

    // Nothing cached. Start a pass only once the watcher has been quiet for a
    // whole tick: a background inference pass that starts while Claude Code is
    // writing a session is a pass competing with the indexer.
    let idle = crate::backend::index_is_idle();
    if idle {
        start_advice_pass(app, spec, facts.clone(), draft_jobs(candidates));
    }
    AdviceOverlayDto {
        facts_hash: hash,
        // Pending either way: idle spawned a worker, and not-idle will spawn one
        // on the next read. Both mean "ask again", and neither is an error.
        pending: true,
        ..Default::default()
    }
}

fn dto_from_overlay(
    spec: &'static piggy_core::AdvisorModel,
    overlay: &cache::AdviceOverlay,
) -> AdviceOverlayDto {
    AdviceOverlayDto {
        facts_hash: overlay.facts_hash.clone(),
        pending: false,
        model: Some(spec.name.to_string()),
        picks: overlay
            .suggestion
            .picks
            .iter()
            .map(|p| AdvicePickDto {
                id: p.id.clone(),
                why: p.rationale.clone(),
            })
            .collect(),
        bundles: overlay
            .suggestion
            .bundles
            .iter()
            .map(|b| AdviceBundleDto {
                project: b.project.clone(),
                ids: b.ids.clone(),
            })
            .collect(),
        drafted: overlay.drafted_candidates(),
    }
}

/// One file a drafting call would rewrite.
///
/// Assembled before the worker starts so the thread carries plain data. The
/// contents are read inside the worker, at call time, and never stored.
///
/// Assembled in every build so the two differ by inference alone; only the
/// inference build has a worker to read the fields.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "local-llm"), allow(dead_code))]
struct DraftJob {
    candidate_id: String,
    /// The file's display name, for the prompt.
    label: String,
    path: String,
    /// The file's hash as the candidate was computed against it.
    source_hash: String,
}

/// Start one background pass, or do nothing when one is already running.
///
/// Detached and fire-and-forget: the result lands in the cache and the UI is
/// told to re-ask. Never queued - a second pass over the same facts would
/// produce the same answer, and a pass over newer facts is what the next read
/// starts.
#[cfg(feature = "local-llm")]
fn start_advice_pass(
    app: AppHandle,
    spec: &'static piggy_core::AdvisorModel,
    facts: Facts,
    jobs: Vec<DraftJob>,
) {
    // `swap` rather than load-then-store: two reads in the same millisecond must
    // not both win.
    if ADVICE_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        lower_thread_priority();
        let mut landed = false;
        if let Some(overlay) = run_advice_pass(spec, &facts, &jobs) {
            if let Ok(mut cache) = advice_cache().lock() {
                cache.put(overlay);
                landed = true;
            }
        }
        ADVICE_RUNNING.store(false, Ordering::SeqCst);
        // The sheet is open and showing the deterministic order; this is what
        // tells it to ask again.
        //
        // Only when something landed. A pass that produced nothing has cached
        // nothing, so telling the UI to re-read would have it miss the cache,
        // start another pass, and be told again: a model that cannot load would
        // spin on its own failure for as long as the sheet was open.
        if landed {
            let _ = app.emit(ADVICE_EVENT, ());
        }
    });
}

#[cfg(not(feature = "local-llm"))]
fn start_advice_pass(
    _app: AppHandle,
    _spec: &'static piggy_core::AdvisorModel,
    _facts: Facts,
    _jobs: Vec<DraftJob>,
) {
}

/// Rank, then draft, in one model load.
///
/// One load for the whole pass: two loads would be two cold starts and, if they
/// ever overlapped, two copies of three gigabytes on a machine
/// [`advisor::fits`] sized for one. The slot is claimed **before** the load, so
/// drop order frees the weights first and the slot only afterwards.
#[cfg(feature = "local-llm")]
fn run_advice_pass(
    spec: &'static piggy_core::AdvisorModel,
    facts: &Facts,
    jobs: &[DraftJob],
) -> Option<cache::AdviceOverlay> {
    use piggy_core::advisor::llama::Advisor;

    let Some(_slot) = claim_inference() else {
        // Another pass holds the slot. Never queue: the next UI read tries
        // again, and by then the facts may have moved anyway.
        return None;
    };
    // Loaded per run and dropped at the end of it, for the reason spelled out in
    // `run_model`: a model cached in a `static` outlives ggml's teardown and
    // aborts the app on quit.
    let advisor = match Advisor::load(spec) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("piggy: could not load the local model for the advice pass: {e}");
            return None;
        }
    };

    let suggestion = match advisor.suggest(facts) {
        Ok(s) => s,
        Err(e) => {
            // A failed rank pass costs the user nothing: the deterministic order
            // renders either way.
            eprintln!("piggy: the advice pass produced nothing usable: {e}");
            return None;
        }
    };

    let mut drafts = std::collections::BTreeMap::new();
    let mut attempted = BTreeSet::new();
    for job in jobs {
        let key = cache::draft_key(spec.id, &job.candidate_id, &job.source_hash);
        // Contents at call time, never stored. A file that has changed since the
        // candidate was computed is skipped rather than drafted against bytes
        // that are gone.
        let text = match piggy_core::claudemd::read_file_text(std::path::Path::new(&job.path), None)
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("piggy: could not read {} to draft it: {e:#}", job.path);
                // Counted as attempted: the file is not going to become
                // readable because the sheet was opened again, and a card that
                // says "still working" forever is the same lie in a politer
                // tense.
                attempted.insert(key);
                continue;
            }
        };
        // Not counted as attempted: the file moved, so this candidate is about
        // to be regenerated against the new bytes under a new id and a new key.
        if text.hash != job.source_hash {
            continue;
        }
        attempted.insert(key);
        match advisor.draft(&job.label, &text.text) {
            Ok(Some(drafted)) => {
                drafts.insert(
                    cache::draft_key(spec.id, &job.candidate_id, &job.source_hash),
                    cache::Draft {
                        candidate_id: job.candidate_id.clone(),
                        text: drafted,
                        had_bom: text.had_bom,
                    },
                );
            }
            // Refused by the guard: the candidate stays blocked, which is the
            // deterministic presentation the spec asks for.
            Ok(None) => {}
            Err(e) => eprintln!("piggy: drafting {} failed: {e}", job.label),
        }
    }

    Some(cache::AdviceOverlay {
        facts_hash: facts.hash(),
        model_id: spec.id.to_string(),
        suggestion,
        drafts,
        attempted,
    })
}

/// Drop this thread to the utility quality-of-service class.
///
/// The idle gate decides *when* the pass runs; this decides what it costs while
/// it does. They solve different halves: a pass that starts in a quiet moment
/// still runs for a minute or two, and the user is very likely typing again
/// before it finishes. `pthread_set_qos_class_self_np` is the OS-supported
/// mechanism for that, and halving the core count is only a proxy for it.
#[cfg(all(feature = "local-llm", target_os = "macos"))]
fn lower_thread_priority() {
    // SAFETY: an FFI call with no pointer arguments, on the calling thread,
    // documented to be callable from any thread that is not a dispatch worker.
    // A failure returns non-zero and changes nothing, which is why the result is
    // ignored: a pass at default priority is worse than one at utility, and
    // better than no pass.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}

#[cfg(all(feature = "local-llm", not(target_os = "macos")))]
fn lower_thread_priority() {}

/// Run `f` with the best schema tokenizer this build and this machine can
/// offer.
///
/// The seam [`piggy_core::probe::SchemaTokenizer`] exists for: with weights on
/// disk a manifest is measured by the advisor's own vocabulary, and without them
/// it falls back to the shipped bytes estimate, which labels itself as one. The
/// caller does not know or care which it got - the label travels with the count
/// into the `mcp_manifests` row.
///
/// A closure rather than a returned value because the tokenizer borrows a loaded
/// vocabulary, and its lifetime is the probe run.
pub fn with_schema_tokenizer<T>(f: impl FnOnce(&dyn probe::SchemaTokenizer) -> T) -> T {
    #[cfg(feature = "local-llm")]
    {
        use piggy_core::advisor::tokenizer::ModelTokenizer;
        if let Some(spec) = selected().as_deref().and_then(advisor::model) {
            if spec.present() {
                match ModelTokenizer::load(spec) {
                    Ok(t) => return f(&t),
                    Err(e) => eprintln!("piggy: could not load the advisor's tokenizer: {e:#}"),
                }
            }
        }
    }
    f(&probe::BytesEstimate)
}

fn draft_jobs(candidates: &[Candidate]) -> Vec<DraftJob> {
    candidates
        .iter()
        .filter(|c| c.kind == advice::ActionKind::ClaudemdTrim)
        .filter_map(|c| {
            Some(DraftJob {
                candidate_id: c.id.clone(),
                // The candidate's title without its verb, which is already the
                // file's plain name: "Trim Stacked's CLAUDE.md" names the file
                // "Stacked's CLAUDE.md".
                label: c.title.strip_prefix("Trim ").unwrap_or(&c.title).to_string(),
                path: match &c.params {
                    advice::Params::Claudemd { path } => path.clone(),
                    _ => return None,
                },
                source_hash: c.source_hash()?.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::claim_inference;

    /// A file in the repo, from this crate's manifest dir.
    fn repo_file(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    }

    /// Acceptance criterion 1, the automatable half: the shipped bundle
    /// compiles the advisor in, and the path everything else runs on does not.
    ///
    /// A repo invariant rather than a unit test, and deliberately so. The whole
    /// of M5 rests on a build flag in one YAML line: drop it and every shipped
    /// Settings screen reports the advisor as unsupported, with nothing else
    /// failing anywhere. Nothing else in the tree notices, so this does.
    ///
    /// The fresh-Mac half of the criterion (download the .dmg, opt into a
    /// model, watch it work offline-tolerant) is a person with a Mac. It is a
    /// checklist item in `docs/releasing.md`, not a pretend assertion here.
    #[test]
    fn the_shipped_bundle_compiles_the_advisor_in_and_the_test_path_does_not() {
        let workflow = repo_file(".github/workflows/release.yml");

        // The step that produces the .dmg passes the feature.
        let bundle = workflow
            .lines()
            .find(|l| l.contains("--target universal-apple-darwin"))
            .expect("no bundle build step in release.yml");
        assert!(
            bundle.contains("--features local-llm"),
            "the release bundle would ship with the advisor dark: {bundle}"
        );

        // And the gate that runs the tests does not, so what CI tests is the
        // default feature set: linking llama.cpp would put cmake and a C++
        // toolchain on the critical path of every run.
        let gate = workflow
            .lines()
            .find(|l| l.contains("cargo test"))
            .expect("no test gate in release.yml");
        assert!(
            !gate.contains("--features"),
            "the test gate stopped exercising the default build: {gate}"
        );

        // The feature exists here and is not on by default: `compiled_in` is
        // `cfg!(feature = "local-llm")`, so a default line would make the flag
        // above meaningless and quietly link llama.cpp into every build.
        let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
            .expect("this crate's own manifest");
        assert!(manifest.contains("\nlocal-llm = ["), "the feature is gone: {manifest}");
        for line in manifest.lines().filter(|l| l.starts_with("default = ")) {
            assert!(!line.contains("local-llm"), "the advisor is on by default: {line}");
        }

        // The manual release path documents the same flag. CI is the usual
        // route and this is the fallback, so the two drifting apart ships a
        // dark advisor by hand.
        assert!(
            repo_file("docs/releasing.md").contains("--features local-llm"),
            "docs/releasing.md no longer builds the advisor in"
        );
    }

    /// The other half of the same invariant, from inside the compiler: this is
    /// a default build, so nothing here can run a model and every call degrades
    /// to the deterministic product. [`super::status`] turns this into the
    /// `unsupported` the Settings screen reports; it is not called here because
    /// it reads the real `state.json`, and a test that touches a developer's
    /// home is the thing the CLI harness exists to prevent.
    #[test]
    #[cfg(not(feature = "local-llm"))]
    fn a_default_build_cannot_run_a_model() {
        assert!(!piggy_core::advisor::compiled_in());
        assert_eq!(
            super::DraftStatus::read().of(&drafting_candidate()),
            Some(super::DraftState::Unavailable),
            "with no advisor there is only one honest thing to say about a draft"
        );
    }

    /// A `ClaudemdTrim`, the one kind whose action is a piece of writing.
    #[cfg(not(feature = "local-llm"))]
    fn drafting_candidate() -> piggy_core::advice::Candidate {
        use piggy_core::advice::{ActionKind, Params, Prerequisite, RISK_CONTENT_EDIT};
        piggy_core::advice::Candidate {
            id: "claudemd-trim-1".into(),
            kind: ActionKind::ClaudemdTrim,
            target: "~/.claude/CLAUDE.md".into(),
            title: "Trim your global CLAUDE.md".into(),
            evidence: Vec::new(),
            est_tokens_month: 135_000,
            risk_tier: RISK_CONTENT_EDIT,
            prerequisites: vec![Prerequisite::NeedsAdvisor],
            fingerprint: "deadbeef".into(),
            params: Params::Claudemd {
                path: "/tmp/CLAUDE.md".into(),
            },
            new_content: None,
            status: "open".into(),
        }
    }

    #[test]
    fn only_one_model_run_holds_the_slot() {
        // A Ledger annotation is in flight.
        let held = claim_inference().expect("the slot starts free");
        // Proof asks for saver notes on its own blocking thread: refused, so a
        // second copy of the weights is never loaded beside the first.
        assert!(
            claim_inference().is_none(),
            "a second run must not load its own weights"
        );
        // The first run finishes and the next visit can ask again.
        drop(held);
        assert!(
            claim_inference().is_some(),
            "the slot is free once the run drops it"
        );
    }
}
