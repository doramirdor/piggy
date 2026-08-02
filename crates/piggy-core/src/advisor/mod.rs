//! The local advisor: an optional language model that *annotates* findings the
//! deterministic layer already computed.
//!
//! Piggy ships without this. [`crate::insights`] is the product; the advisor is
//! an opt-in layer on top, and every path through this module degrades to
//! "render the deterministic insights unchanged" rather than to an error.
//!
//! The rules that keep it honest, and why they are enforced in code rather than
//! in a prompt:
//!
//! * **The model never produces a number.** It receives facts that were already
//!   computed and may only re-word them. [`guard`] rejects any output containing
//!   a figure that is not in the input, so a hallucinated percentage cannot
//!   reach the UI even if the prompt fails to prevent it.
//! * **The model never invents a finding.** It annotates by `insight.id`;
//!   an id the ledger did not produce is dropped.
//! * **Weights are data, never code.** We download a `.gguf` and verify it
//!   against a sha256 pinned in [`CATALOG`]. We never download an executable:
//!   inference is linked into this binary (see [`llama`]), so the code that runs
//!   is the code that was signed and notarized.
//!
//! The other half of honesty is not offering a model that will wreck the
//! machine. [`AdvisorModel::peak_bytes`] computes real memory cost including the
//! KV cache, which for a 4B model at 8k context is larger than most people
//! expect, and [`fits`] refuses anything the host cannot hold.

pub mod download;
pub mod facts;
pub mod guard;

#[cfg(feature = "local-llm")]
pub mod llama;

use std::path::PathBuf;

use crate::config::piggy_home;

/// KV cache geometry and download coordinates for one downloadable model.
///
/// Sizes and digests are pinned from the Hugging Face API at catalog-authoring
/// time (`lfs.sha256`), not fetched at runtime: a digest we look up from the
/// same server that serves the file verifies nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvisorModel {
    /// Stable id used in settings and IPC. Never derived from the filename.
    pub id: &'static str,
    /// Shown in the picker.
    pub name: &'static str,
    /// One line on what the tradeoff is, for the picker.
    pub blurb: &'static str,
    pub repo: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
    /// Exact byte length, so a truncated download is caught before hashing.
    pub bytes: u64,

    // --- KV cache geometry, from the model's own config.json ---
    pub layers: u32,
    pub kv_heads: u32,
    pub head_dim: u32,
    /// `(window, pattern)` for sliding-window attention: one layer in every
    /// `pattern` attends globally, the rest cap their cache at `window` tokens.
    /// `None` means every layer is global.
    pub sliding: Option<(u32, u32)>,

    /// Context we actually run at. This is a **budget decision, not a model
    /// limit**: the facts payload is bounded (see [`facts`]), so paying for
    /// 262k of KV cache we will never fill would be pure waste.
    pub ctx: u32,
}

/// Bytes per cached element with a `q8_0` KV cache: 32 values in a 32-byte
/// block plus a 2-byte scale. We always quantize the KV cache, because at f16 a
/// 4B model's cache is larger than its weights at any useful context.
const KV_BYTES_PER_ELEM: f64 = 34.0 / 32.0;

/// llama.cpp's compute buffers and graph overhead, which scale with model size
/// rather than context. Measured slack, deliberately generous: being wrong
/// downward here means swapping a user's 8GB machine.
const COMPUTE_FLOOR: u64 = 256 * 1024 * 1024;
const COMPUTE_SHARE: f64 = 0.05;

impl AdvisorModel {
    /// Bytes of KV cache at [`Self::ctx`], accounting for sliding-window
    /// attention.
    ///
    /// This is the term people forget. Qwen3-4B is 36 fully-global layers at 8
    /// KV heads by 128 dims, which is 144 KiB of cache *per token*: 1.2 GB at 8k
    /// context, on top of 2.5 GB of weights. Gemma 3 4B is the same weight class
    /// but caps 29 of its 34 layers at a 1024-token window, so the same context
    /// costs about a quarter as much.
    pub fn kv_bytes(&self) -> u64 {
        // Both K and V, per layer, per token.
        let per_layer_token =
            2.0 * self.kv_heads as f64 * self.head_dim as f64 * KV_BYTES_PER_ELEM;

        let layer_tokens = match self.sliding {
            None => self.layers as u64 * self.ctx as u64,
            Some((window, pattern)) => {
                // Layer `i` is global when `(i + 1) % pattern == 0`, so a
                // 34-layer model with pattern 6 has 5 global layers.
                let global = (self.layers / pattern.max(1)) as u64;
                let local = self.layers as u64 - global;
                global * self.ctx as u64 + local * self.ctx.min(window) as u64
            }
        };
        (layer_tokens as f64 * per_layer_token) as u64
    }

    /// Resident bytes this model needs while answering: weights, KV cache, and
    /// llama.cpp's compute buffers.
    pub fn peak_bytes(&self) -> u64 {
        let compute = COMPUTE_FLOOR.max((self.bytes as f64 * COMPUTE_SHARE) as u64);
        self.bytes + self.kv_bytes() + compute
    }

    /// Where the verified weights live once downloaded.
    pub fn path(&self) -> PathBuf {
        models_dir().join(self.file)
    }

    /// Whether verified weights are already on disk. Length only: a full re-hash
    /// of 2.5 GB on every status poll would stall the UI, and
    /// [`download::verify`] is what actually gates the file into this location.
    pub fn present(&self) -> bool {
        std::fs::metadata(self.path())
            .map(|m| m.len() == self.bytes)
            .unwrap_or(false)
    }
}

/// Where downloaded weights live: `<piggy_home>/models`.
///
/// Under `PIGGY_HOME`, so tests and the install engine share one override
/// discipline and no test can touch a real download.
pub fn models_dir() -> PathBuf {
    piggy_home().join("models")
}

/// The models Piggy will download.
///
/// Deliberately short. A picker with twelve quantizations is a research tool;
/// this needs to answer "will it run on my laptop" and nothing else.
///
/// Two hard rules for anything added here, both learned the expensive way:
///
/// * **Non-reasoning instruct builds only.** Plain `Qwen3-*` is a hybrid
///   thinking model: its chat template opens a `<think>` block, so the first
///   useful token arrives seconds late. Wrong for a menu bar popover, and it was
///   what exposed the grammar abort described in [`llama`].
/// * **4B or better.** A 3B was here and was removed after a live run against a
///   real ledger produced "the harness reuses one session across iterations,
///   which pays the full startup cost", the exact inverse of the finding's own
///   advice. [`guard`] can enforce that a number is real; nothing can enforce
///   that a causal claim is. The only control for that is model quality, so the
///   floor is set here rather than patched downstream.
pub const CATALOG: &[AdvisorModel] = &[
    AdvisorModel {
        id: "qwen3-4b-instruct-2507",
        name: "Qwen3 4B Instruct",
        blurb: "Best at reading your config. The default when it fits.",
        repo: "unsloth/Qwen3-4B-Instruct-2507-GGUF",
        file: "Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
        sha256: "3605803b982cb64aead44f6c1b2ae36e3acdb41d8e46c8a94c6533bc4c67e597",
        bytes: 2_497_281_120,
        layers: 36,
        kv_heads: 8,
        head_dim: 128,
        sliding: None,
        // The `-Instruct-2507` build is the non-thinking variant. A reasoning
        // model would spend seconds of local generation before its first
        // visible token, which is wrong for a menu bar popover.
        ctx: 4096,
    },
    AdvisorModel {
        id: "gemma-3-4b-it",
        name: "Gemma 3 4B",
        blurb: "Same size as Qwen 4B but far cheaper at long context.",
        repo: "unsloth/gemma-3-4b-it-GGUF",
        file: "gemma-3-4b-it-Q4_K_M.gguf",
        sha256: "04a43a22e8d2003deda5acc262f68ec1005fa76c735a9962a8c77042a74a7d19",
        bytes: 2_489_894_016,
        layers: 34,
        kv_heads: 4,
        head_dim: 256,
        sliding: Some((1024, 6)),
        // Sliding-window attention makes context nearly free here, so this is
        // the model to grow if follow-up questions over the whole ledger land.
        ctx: 8192,
    },
];

/// Look a model up by [`AdvisorModel::id`].
pub fn model(id: &str) -> Option<&'static AdvisorModel> {
    CATALOG.iter().find(|m| m.id == id)
}

/// Memory we refuse to plan around, whatever the machine: the OS, the user's
/// editor, and the browser they are about to open.
const RESERVE_FLOOR: u64 = 3 * 1024 * 1024 * 1024;
/// On a larger machine the absolute floor is too generous, so also reserve a
/// share of total. A 64GB host should not be told it can spend 61GB on weights.
const RESERVE_SHARE: f64 = 0.4;

/// Bytes a model is allowed to occupy on a host with `total` bytes of RAM.
pub fn budget(total: u64) -> u64 {
    let reserve = RESERVE_FLOOR.max((total as f64 * RESERVE_SHARE) as u64);
    total.saturating_sub(reserve)
}

/// Whether `m` can run on a host with `total` bytes of RAM.
pub fn fits(m: &AdvisorModel, total: u64) -> bool {
    m.peak_bytes() <= budget(total)
}

/// Every catalog model that fits, largest (best) first.
///
/// The UI shows only these. Offering a download that will swap the machine and
/// letting the user find out after 2.5 GB is not a choice, it is a trap.
pub fn available(total: u64) -> Vec<&'static AdvisorModel> {
    let mut out: Vec<_> = CATALOG.iter().filter(|m| fits(m, total)).collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    out
}

/// The model to preselect: the largest that fits.
pub fn recommended(total: u64) -> Option<&'static AdvisorModel> {
    available(total).into_iter().next()
}

/// Physical RAM in bytes, or `None` when we cannot tell.
///
/// `None` is load-bearing: an unknown host budget means we offer nothing rather
/// than guess, because guessing high is the failure that hurts.
pub fn host_ram() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        return String::from_utf8_lossy(&out.stdout).trim().parse().ok();
    }
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        return None;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// What the UI needs to render the advisor section without further calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisorState {
    /// Built without the `local-llm` feature, or no model fits this host.
    Unsupported,
    /// Supported, but the user has not opted in.
    Off,
    /// Opted in, weights not on disk yet.
    NeedsDownload,
    /// Weights present and verified.
    Ready,
}

/// Whether this build can run a model at all. Without the feature the whole
/// advisor is inert, and the UI says so instead of offering a 2.5 GB download
/// that could never be used.
pub const fn compiled_in() -> bool {
    cfg!(feature = "local-llm")
}
