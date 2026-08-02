//! In-process inference, compiled only under the `local-llm` feature.
//!
//! The advisor links llama.cpp rather than shelling out to a downloaded
//! `llama-server`, so the code that executes is the code that was signed and
//! notarized. The only thing fetched at runtime is weights, and weights are
//! inert data (see [`super::download`]).
//!
//! Constraint comes from two things:
//!
//! 1. A **greedy sampler**, so the same ledger yields the same annotation. Text
//!    next to a receipt that reworded itself on every poll would read as
//!    instability in the numbers.
//! 2. [`super::guard`], which rejects any figure that is not already a fact and
//!    any id the ledger did not produce. This is the whole guarantee.
//!
//! ## Why there is no GBNF grammar here
//!
//! There was one. It pinned the JSON shape and enumerated the valid
//! `insight_id`s, which was a genuinely nice belt-and-braces on top of the
//! guard. It is gone because it can **kill the app**.
//!
//! When llama.cpp's grammar sampler is handed a token the grammar cannot
//! accept, it trips `GGML_ASSERT(!stacks.empty())` in `llama-grammar.cpp`, and
//! `GGML_ASSERT` calls `abort()`. That is not a catchable error in Rust:
//! `LlamaSampler::accept` routes through `try_accept` under the `common`
//! feature, but the assert fires in C++ before any status can be returned, so
//! `catch_unwind` and `Result` are both useless. In a menu bar app the process
//! is the product, and the whole thing would simply vanish.
//!
//! Two separate live runs hit it: once by accepting an end-of-generation token
//! into a completed grammar, and once because a hybrid reasoning model opened
//! with a `<think>` block the grammar forbade at token zero. Both were fixable,
//! but hitting the same abort twice from unrelated causes is the signal: the
//! grammar was redundant with [`super::guard`] and was the only component that
//! could take the process down. Redundancy is not worth a crash.
//!
//! What replaces it: [`super::guard::accept`] already tolerates a preamble and
//! rejects anything malformed, unknown, or numerically invented. A model that
//! returns prose instead of JSON now yields zero annotations instead of a
//! SIGABRT, and zero annotations is a state the UI already handles.
//!
//! Every failure path returns `Err`, and every caller renders the deterministic
//! findings regardless. The advisor is never load-bearing.

use std::num::NonZeroU32;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use llama_cpp_2::context::params::{KvCacheType, LlamaContextParams};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

use super::facts::Facts;
use super::guard::{self, Annotation};
use super::AdvisorModel;

/// Hard ceiling on generated tokens.
///
/// 512 was too tight: a 3B writes much longer `why` strings than it is asked to
/// and was running out mid-object, which cost the complete items ahead of it
/// until [`super::guard`] learned to salvage a truncated array. This leaves room
/// for three verbose annotations.
const MAX_TOKENS: usize = 768;
/// Wall-clock ceiling. A menu bar popover that hangs is worse than one with no
/// annotations, so generation is abandoned rather than awaited.
const DEADLINE: Duration = Duration::from_secs(20);

/// llama.cpp's backend is process-global and must be initialised exactly once.
static BACKEND: OnceLock<std::result::Result<LlamaBackend, String>> = OnceLock::new();

fn backend() -> Result<&'static LlamaBackend> {
    BACKEND
        .get_or_init(|| {
            // Quiet: llama.cpp logs load progress to stderr by default, which
            // would end up interleaved with the app's own diagnostics.
            llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default().with_logs_enabled(false));
            LlamaBackend::init().map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("could not start the local model backend: {e}"))
}

/// A loaded model, kept alive across queries.
///
/// Loading is seconds of mmap and Metal warm-up for a 2.5 GB file, so the app
/// holds one of these rather than paying it per annotation.
pub struct Advisor {
    model: LlamaModel,
    spec: &'static AdvisorModel,
}

impl Advisor {
    /// Load verified weights from disk.
    ///
    /// Verification is the caller's job via [`super::download::verify`]; this
    /// only checks presence, because re-hashing gigabytes on every load would
    /// dominate startup.
    pub fn load(spec: &'static AdvisorModel) -> Result<Self> {
        let backend = backend()?;
        let path = spec.path();
        if !path.exists() {
            anyhow::bail!("{} is not downloaded", spec.file);
        }

        let params = LlamaModelParams::default().with_n_gpu_layers(if cfg!(feature = "metal") {
            // Offload everything. These models are chosen to fit, so a partial
            // offload would only mean a slower answer for no memory saved.
            u32::MAX
        } else {
            0
        });

        let model = LlamaModel::load_from_file(backend, &path, &params)
            .with_context(|| format!("loading {}", path.display()))?;
        Ok(Advisor { model, spec })
    }

    /// Annotate the findings in `facts`.
    ///
    /// Returns only annotations that survived [`guard::accept`]. An empty vector
    /// is a normal outcome and means the UI shows the deterministic findings
    /// alone.
    pub fn annotate(&self, facts: &Facts) -> Result<Vec<Annotation>> {
        if facts.insight_ids.is_empty() {
            return Ok(Vec::new());
        }
        guard::accept(&self.annotate_raw(facts)?, facts)
    }

    /// The model's unfiltered output, before [`guard::accept`] sees it.
    ///
    /// Exposed for diagnostics. When an annotation is dropped the UI shows
    /// nothing, which is the right behaviour but a terrible debugging signal:
    /// "the model wrote a number that was not a fact" and "the model returned
    /// prose instead of JSON" are indistinguishable from an empty list. Never
    /// render this. It is the text that has not been checked yet.
    pub fn annotate_raw(&self, facts: &Facts) -> Result<String> {
        self.generate(&self.prompt(facts)?)
    }

    /// Build the prompt through the model's own chat template.
    ///
    /// Falling back to a bare concatenation is deliberate: a GGUF without an
    /// embedded template still produces usable output, and refusing to run would
    /// be a worse trade than a slightly off prompt format.
    fn prompt(&self, facts: &Facts) -> Result<String> {
        // Instructions AFTER the data. The fact sheet is a couple of thousand
        // tokens of dense JSON, and a small model that read the task first has
        // effectively forgotten it by the time it reaches the end.
        let user = format!("{}\n\n{USER_PREAMBLE}", facts.prompt_json());
        let plain = format!("{SYSTEM}\n\n{user}\n\n");

        let Ok(tmpl) = self.model.chat_template(None) else {
            return Ok(plain);
        };
        let msgs = vec![
            LlamaChatMessage::new("system".into(), SYSTEM.into())?,
            LlamaChatMessage::new("user".into(), user)?,
        ];
        Ok(self
            .model
            .apply_chat_template(&tmpl, &msgs, true)
            .unwrap_or(plain))
    }

    /// Greedy generation with a token and time ceiling.
    fn generate(&self, prompt: &str) -> Result<String> {
        let backend = backend()?;
        let ctx_size = self.spec.ctx;

        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            // Leave the machine responsive. This runs behind a menu bar popover,
            // not on a batch box.
            .saturating_sub(2)
            .clamp(1, 8) as i32;

        let params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(ctx_size))
            .with_n_batch(ctx_size)
            .with_n_threads(threads)
            .with_n_threads_batch(threads)
            // Quantized KV. The catalog's peak-memory math assumes q8_0, so
            // these two lines are what make the RAM gate honest.
            .with_type_k(KvCacheType::Q8_0)
            .with_type_v(KvCacheType::Q8_0);

        let mut ctx = self
            .model
            .new_context(backend, params)
            .context("creating the inference context")?;

        let tokens = self
            .model
            .str_to_token(prompt, AddBos::Always)
            .context("tokenizing the fact sheet")?;
        if tokens.len() + 64 >= ctx_size as usize {
            anyhow::bail!(
                "the fact sheet is {} tokens, too large for this model's {ctx_size}-token window",
                tokens.len()
            );
        }

        let mut batch = LlamaBatch::new(ctx_size as usize, 1);
        let last = tokens.len() - 1;
        for (i, t) in tokens.iter().enumerate() {
            batch.add(*t, i as i32, &[0], i == last)?;
        }
        ctx.decode(&mut batch).context("evaluating the prompt")?;

        let mut sampler = LlamaSampler::greedy();

        let started = Instant::now();
        let mut out = String::new();
        let mut pos = tokens.len() as i32;
        // ONE decoder for the whole generation. A token boundary can fall in the
        // middle of a multi-byte character, so a per-token decoder turns any
        // non-ASCII the model emits (curly quotes, for one) into replacement
        // characters.
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        // Bracket depth over the emitted text, so generation stops the moment the
        // array closes rather than waiting for the model to volunteer an EOG
        // token. Small models happily keep writing commentary after the JSON,
        // and every token of it is latency in a popover.
        let mut depth = 0i32;
        let mut opened = false;
        let mut in_string = false;
        let mut escaped = false;

        for _ in 0..MAX_TOKENS {
            if started.elapsed() > DEADLINE {
                anyhow::bail!("the local model did not finish within {DEADLINE:?}");
            }
            let token = sampler.sample(&ctx, -1);

            // EOG before accept. Harmless for a greedy sampler, but kept as the
            // correct order: it is what a stateful sampler requires, and the
            // module docs record what accepting EOG into one cost us.
            if self.model.is_eog_token(token) {
                break;
            }
            sampler.accept(token);
            let piece = self.model.token_to_piece(token, &mut decoder, false, None)?;
            out.push_str(&piece);

            // Track depth outside string literals, so a bracket inside a
            // headline cannot end generation early.
            for c in piece.chars() {
                if in_string {
                    match c {
                        _ if escaped => escaped = false,
                        '\\' => escaped = true,
                        '"' => in_string = false,
                        _ => {}
                    }
                    continue;
                }
                match c {
                    '"' => in_string = true,
                    '[' | '{' => {
                        depth += 1;
                        opened = true;
                    }
                    ']' | '}' => depth -= 1,
                    _ => {}
                }
            }
            if opened && depth <= 0 {
                break;
            }

            batch.clear();
            batch.add(token, pos, &[0], true)?;
            pos += 1;
            ctx.decode(&mut batch).context("generating")?;
        }
        Ok(out)
    }
}

const SYSTEM: &str = "\
You annotate a token-usage report for a developer tool called Piggy.

The report's numbers are exact measurements that have already been computed. \
Your job is to explain WHY each finding is happening, using the user's own \
configuration, and to make the recommended action specific.

Hard rules:
- Never state a number, percentage, or multiplier that does not appear verbatim \
in the data you were given. If you want to quote a figure, copy it exactly.
- Never perform arithmetic. Do not add, average, or compare figures to produce a \
new one. Every total you might need has already been computed for you.
- Never speculate about savings. The data says what things cost, not what \
turning them off would achieve.
- Never guess at a cause. If you write \"likely\", \"probably\", \"suggests\", \
\"appears to\" or \"may be\", you are guessing: leave that finding out instead. \
Only say what the data shows.
- Prefer a couple of sharp annotations over one for every finding. Annotate the \
ones where the configuration actually explains what you see.";

const USER_PREAMBLE: &str = "\
That was the report. Now annotate the two or three most important findings \
above. For each one, return an object with:
  insight_id  the finding's id, copied exactly from `findings`
  headline    one short sentence under 120 characters, naming the CAUSE
  why         one or two short sentences, grounded in `configuration`

The headline must not restate the finding's own title. Say why it is happening.

Name specific items from `configuration` where they explain a finding. Do not \
quote figures at all unless you are copying one character-for-character from \
the report: never add, compare, or estimate one. Prefer sentences with no \
numbers in them.

Return a JSON array of two or three objects, and nothing else.";
