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
//! ### What changed upstream, and what did not (checked for M5.4)
//!
//! Half of the warning above is now out of date and half of it is still exactly
//! right, which is why it stays. The pinned `llama-cpp-2 0.1.153` does expose
//! the grammar API (`LlamaSampler::grammar`, `json_schema_to_grammar`, both
//! under the `common` feature, which is on by default), and two of the three
//! failure modes are fixed: a mid-generation rejection now **throws** instead of
//! aborting (`llama-grammar.cpp:1503-1508`), and the sys crate catches it and
//! surfaces it as `Err` from `LlamaSampler::try_accept`.
//!
//! The third is not fixed, and it cannot be worked around through the API this
//! file uses. `llama_sampler_sample` (`llama.cpp/src/llama-sampler.cpp:870`)
//! calls `llama_sampler_accept` **directly in C++**, inside the same function
//! that picks the token, and that internal accept is not the try/catch-wrapped
//! entry point the sys crate exposes. So with a grammar in the chain the token
//! is accepted before Rust regains control, the end-of-generation check below is
//! one frame too late to help, and `llama-grammar.cpp:1428-1435` is still a
//! `GGML_ABORT` when an end-of-generation token is accepted into a grammar that
//! is not in an accepting state. `GGML_ABORT` is not a C++ exception.
//!
//! Using a grammar safely would mean abandoning [`LlamaSampler::sample`] for a
//! manual `token_data_array` plus `apply_sampler` plus greedy pick plus our own
//! end-of-generation check plus `try_accept` loop: a rewrite of the single code
//! path that must never crash, to gain a constraint on shape that
//! [`super::guard`] already enforces on shape *and* on truth. The grammar text
//! and that protocol are kept in [`super::prompts`] behind `GRAMMAR`, off.
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

use super::draft;
use super::facts::Facts;
use super::guard::{self, Annotation, Suggestion};
use super::prompts;
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

/// Which of a model's two windows a call runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// [`AdvisorModel::ctx`]: the cheap window the popover passes answer in.
    Popover,
    /// [`AdvisorModel::advice_ctx`]: 16,384, which is what the advice sheet and
    /// a whole CLAUDE.md need.
    Advice,
}

/// How many cores a call may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadShare {
    /// Everything but two. Today's behaviour, and the popover passes keep it
    /// exactly: someone is watching that spinner.
    LeaveTwo,
    /// Half the machine, capped. The advice pass runs in the background while
    /// the user is working, and a background pass that fights their editor for
    /// CPU is worse than one that takes twice as long.
    Half,
}

impl ThreadShare {
    fn count(self) -> i32 {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        match self {
            ThreadShare::LeaveTwo => cores.saturating_sub(2).clamp(1, 8) as i32,
            ThreadShare::Half => (cores / 2).clamp(1, 4) as i32,
        }
    }
}

/// What one call is allowed to spend.
#[derive(Debug, Clone, Copy)]
struct Budget {
    window: Window,
    max_tokens: usize,
    deadline: Duration,
    threads: ThreadShare,
}

/// The two M4 popover passes. Unchanged, deliberately: they answer while someone
/// waits, and nothing in M5.4 is a reason to make them slower or greedier.
const ANNOTATE: Budget = Budget {
    window: Window::Popover,
    max_tokens: MAX_TOKENS,
    deadline: DEADLINE,
    threads: ThreadShare::LeaveTwo,
};

/// The rank pass.
///
/// 1,024 generated tokens is the spec's figure and it is the right one here:
/// eight picks at 280 characters plus the JSON around them is comfortably under
/// it. The deadline is not a latency budget - this runs on a background worker
/// whose result lands in a cache, so abandoning at 20 seconds would mean a
/// machine that never once produces advice. It is a guard against a wedged
/// generation, and 90 seconds is generous on purpose.
const SUGGEST: Budget = Budget {
    window: Window::Advice,
    max_tokens: 1_024,
    deadline: Duration::from_secs(90),
    threads: ThreadShare::Half,
};

/// Input tokens one drafting call sends.
///
/// docs/m5-spec.md says 12,000, which predates the arithmetic: a rewrite is the
/// same file again, so a 12,000-token input needs nearly 12,000 tokens of output
/// and 24,000 does not fit a 16,384-token window at all. 6,000 is close to the
/// largest cap that leaves room to write the answer back, and a file over it is
/// split on its `##` headings, which is the mechanism the spec already provides
/// for exactly this.
pub const DRAFT_INPUT_CAP_TOKENS: usize = 6_000;

/// The drafting pass. Same window, same deadline, same share as the rank pass;
/// only the token ceiling differs, and it is computed per call from the length
/// of the file being rewritten.
const DRAFT: Budget = SUGGEST;

/// The one line attempt two adds.
const RETRY_NOTE: &str = "Your previous answer was not valid JSON. Return only \
the JSON object, starting with { and ending with }, and nothing before or after \
it.";

/// When a generation stops, apart from end-of-generation, the token ceiling and
/// the deadline.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// The top-level bracket depth returned to zero: the JSON closed. Small
    /// models happily keep writing commentary after it, and every token of that
    /// is latency.
    Json,
    /// The emitted text contains this marker. The bracket tracker is useless for
    /// a draft: a markdown file full of code fences would trip it on the first
    /// one.
    Sentinel(&'static str),
}

/// Slack over the source's own token count for one drafting call's ceiling.
///
/// A draft has to come out at least a tenth smaller than its source, so the
/// source's length is the ceiling and this is only room for the closing marker
/// and for the draft tokenizing slightly worse than the original did.
const DRAFT_TOKEN_MARGIN: usize = 256;

/// llama.cpp's backend is process-global and must be initialised exactly once.
static BACKEND: OnceLock<std::result::Result<LlamaBackend, String>> = OnceLock::new();

pub(crate) fn backend() -> Result<&'static LlamaBackend> {
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

/// Whether a prompt of `prompt_tokens` leaves room to answer in a `ctx_size`
/// window.
///
/// The reserve is generation's real ceiling, not a token or two of slack. The KV
/// cache holds exactly `ctx_size` entries and [`Advisor::generate`] walks `pos`
/// forward once per generated token, so a sheet that clears a smaller reserve
/// passes the pre-flight and then dies inside the loop: `ctx.decode` runs out of
/// slots, `generate` returns `Err`, and every annotation already written is
/// discarded with it. Rejecting up front is what makes the bail report the real
/// cause instead of "generating".
fn fits_context(prompt_tokens: usize, ctx_size: u32, max_tokens: usize) -> bool {
    prompt_tokens + max_tokens < ctx_size as usize
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
        self.generate(
            &self.prompt(&facts.prompt_json(), SYSTEM, USER_PREAMBLE)?,
            ANNOTATE,
            Stop::Json,
        )
    }

    /// Turn the per-saver measurements into advice.
    ///
    /// A different job from [`Self::annotate`], and so a different prompt: the
    /// ledger pass explains *why* a cost exists by naming a configuration item,
    /// while this one says what a saver's measured result means for this setup
    /// and what to do about it. Same guard, same allow-list, same failure mode:
    /// anything unquotable, hedged, or about the wrong saver is dropped and the
    /// deterministic summary renders alone.
    pub fn explain_savers(&self, facts: &Facts) -> Result<Vec<Annotation>> {
        if facts.insight_ids.is_empty() {
            return Ok(Vec::new());
        }
        guard::accept_savers(&self.explain_savers_raw(facts)?, facts)
    }

    /// The saver pass's unfiltered output. Diagnostics only, exactly as
    /// [`Self::annotate_raw`]: never rendered.
    pub fn explain_savers_raw(&self, facts: &Facts) -> Result<String> {
        self.generate(
            &self.prompt(&facts.prompt_json(), SAVER_SYSTEM, &saver_preamble())?,
            ANNOTATE,
            Stop::Json,
        )
    }

    /// Rank and explain the candidates in `facts`.
    ///
    /// Returns only what survived [`guard::accept_suggestion`]. An empty
    /// [`Suggestion`] is a normal outcome and means the UI renders the
    /// deterministic order with house copy, which is the fallback the spec
    /// requires.
    ///
    /// One bounded retry, and only when the response did not parse as an object
    /// at all. Never a retry because picks were dropped: a dropped pick is the
    /// guard working, and asking again invites a second answer aimed at getting
    /// past it.
    pub fn suggest(&self, facts: &Facts) -> Result<Suggestion> {
        if facts.candidate_ids.is_empty() {
            return Ok(Suggestion::default());
        }
        let first = match guard::accept_suggestion(&self.suggest_raw(facts)?, facts) {
            Ok(s) => return Ok(s),
            Err(e) => e,
        };
        eprintln!("piggy: the advisor's ranking did not parse, asking once more: {first}");

        // Attempt two differs from attempt one by one line, which is the point:
        // same sheet, same greedy sampler, one more instruction about the shape.
        let preamble = format!("{}\n\n{RETRY_NOTE}", prompts::suggest_preamble());
        let retry = self.prompt(&facts.prompt_json(), prompts::SUGGEST_SYSTEM, &preamble)?;
        match guard::accept_suggestion(&self.generate(&retry, SUGGEST, Stop::Json)?, facts) {
            Ok(s) => Ok(s),
            Err(e) => {
                // Two failures is not an error the user sees. The deterministic
                // order with house copy is a complete product.
                eprintln!("piggy: the advisor's ranking did not parse twice, using the deterministic order: {e}");
                Ok(Suggestion::default())
            }
        }
    }

    /// The rank pass's unfiltered output. Diagnostics only, exactly as
    /// [`Self::annotate_raw`]: never rendered.
    pub fn suggest_raw(&self, facts: &Facts) -> Result<String> {
        self.generate(
            &self.prompt(&facts.prompt_json(), prompts::SUGGEST_SYSTEM, &prompts::suggest_preamble())?,
            SUGGEST,
            Stop::Json,
        )
    }

    /// Draft a shorter replacement for one CLAUDE.md.
    ///
    /// `label` is the file's display name (`"Stacked's CLAUDE.md"`). It goes in
    /// the prompt so the model knows what it is editing and is never trusted
    /// back: nothing the model returns is read as a path.
    ///
    /// `original` is [`crate::claudemd::FileText::text`], BOM already stripped.
    ///
    /// `Ok(None)` means nothing survived [`draft::accept_draft`], which demotes
    /// the candidate to deterministic presentation rather than failing the pass.
    /// The reject is logged, because a silently absent draft and a rejected one
    /// are indistinguishable from the outside.
    pub fn draft(&self, label: &str, original: &str) -> Result<Option<String>> {
        let joined = match self.draft_body(label, original)? {
            Some(j) => j,
            None => return Ok(None),
        };
        match draft::accept_joined(original, &joined) {
            Ok(text) => Ok(Some(text)),
            Err(e) => {
                eprintln!("piggy: the drafted rewrite of {label} was refused: {}", e.reason());
                Ok(None)
            }
        }
    }

    /// The draft before the whole-file checks: one call, or one per section.
    fn draft_body(&self, label: &str, original: &str) -> Result<Option<String>> {
        let tokens = |s: &str| self.token_count(s);
        if tokens(original) <= DRAFT_INPUT_CAP_TOKENS {
            return Ok(Some(self.draft_once(label, original)?));
        }

        let sections = draft::split_sections(original, DRAFT_INPUT_CAP_TOKENS, &tokens);
        // One section and it is over the cap: there is nothing to split on, so
        // there is no draft to make.
        if sections.len() < 2 {
            eprintln!("piggy: {label} is over the drafting cap and has no `##` sections to split on");
            return Ok(None);
        }

        let mut out = String::new();
        for section in &sections {
            // A section that is enormous on its own is passed through verbatim.
            // Failing the whole file for it would throw away the shrink
            // available everywhere else, and a pass-through shows in the diff as
            // exactly what it is.
            if section.too_large {
                out.push_str(&section.text);
                continue;
            }
            let named = match &section.heading {
                Some(h) => format!("{label}, section: {h}"),
                None => label.to_string(),
            };
            let raw = self.draft_once(&named, &section.text)?;
            // A section that fails its own checks is replaced by its source, not
            // dropped: the user's guidance is not ours to delete because a model
            // wrote something we would not print.
            match draft::check_draft_content(&section.text, &raw) {
                Ok(text) => out.push_str(&text),
                Err(e) => {
                    eprintln!("piggy: a section of {label} was refused: {}", e.reason());
                    out.push_str(&section.text);
                }
            }
        }
        Ok(Some(out))
    }

    /// One drafting call, returning the raw text between (and including) the
    /// markers.
    fn draft_once(&self, label: &str, source: &str) -> Result<String> {
        let lines = source.lines().count();
        let preamble = prompts::draft_preamble(label, lines, prompts::draft_target_lines(lines));
        let prompt = self.prompt(source, prompts::DRAFT_SYSTEM, &preamble)?;
        // The ceiling is the source's own length: a draft that has to come out
        // smaller has no business being longer, and a fixed ceiling would either
        // cut off a large file or reserve a window a small one never uses.
        let budget = Budget {
            max_tokens: self.token_count(source) + DRAFT_TOKEN_MARGIN,
            ..DRAFT
        };
        self.generate(&prompt, budget, Stop::Sentinel(prompts::DRAFT_CLOSE))
    }

    /// The drafting call's unfiltered output. Diagnostics only.
    pub fn draft_raw(&self, label: &str, source: &str) -> Result<String> {
        self.draft_once(label, source)
    }

    /// How many tokens `text` is to this model. Falls back to a byte estimate
    /// only for input `str_to_token` refuses, which serialized text never is.
    fn token_count(&self, text: &str) -> usize {
        self.model
            .str_to_token(text, AddBos::Never)
            .map(|t| t.len())
            .unwrap_or_else(|_| text.len() / 3)
    }

    /// Build the prompt through the model's own chat template.
    ///
    /// Falling back to a bare concatenation is deliberate: a GGUF without an
    /// embedded template still produces usable output, and refusing to run would
    /// be a worse trade than a slightly off prompt format.
    fn prompt(&self, data: &str, system: &str, preamble: &str) -> Result<String> {
        // Instructions AFTER the data. The fact sheet is a couple of thousand
        // tokens of dense JSON, and a small model that read the task first has
        // effectively forgotten it by the time it reaches the end.
        let user = format!("{data}\n\n{preamble}");
        let plain = format!("{system}\n\n{user}\n\n");

        let Ok(tmpl) = self.model.chat_template(None) else {
            return Ok(plain);
        };
        let msgs = vec![
            LlamaChatMessage::new("system".into(), system.into())?,
            LlamaChatMessage::new("user".into(), user)?,
        ];
        Ok(self
            .model
            .apply_chat_template(&tmpl, &msgs, true)
            .unwrap_or(plain))
    }

    /// Greedy generation with a token and time ceiling.
    fn generate(&self, prompt: &str, budget: Budget, stop: Stop) -> Result<String> {
        let backend = backend()?;
        let ctx_size = match budget.window {
            Window::Popover => self.spec.ctx,
            Window::Advice => self.spec.advice_ctx,
        };
        let threads = budget.threads.count();

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
        if !fits_context(tokens.len(), ctx_size, budget.max_tokens) {
            anyhow::bail!(
                "the prompt is {} tokens and the answer needs {}, too large for \
                 this model's {ctx_size}-token window",
                tokens.len(),
                budget.max_tokens
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

        for _ in 0..budget.max_tokens {
            if started.elapsed() > budget.deadline {
                let d = budget.deadline;
                anyhow::bail!("the local model did not finish within {d:?}");
            }
            let token = sampler.sample(&ctx, -1);

            // End-of-generation before anything else. `sample()` has already
            // accepted this token inside C++ (llama-sampler.cpp:870), which is
            // harmless for a greedy sampler and is exactly why a grammar cannot
            // be driven through it: see the module doc.
            if self.model.is_eog_token(token) {
                break;
            }
            let piece = self.model.token_to_piece(token, &mut decoder, false, None)?;
            out.push_str(&piece);

            match stop {
                Stop::Json => {
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
                }
                // A marker can straddle a token boundary, so the test is on the
                // whole emitted text and not on the piece.
                Stop::Sentinel(marker) => {
                    // Only the tail can hold a marker that just completed, and
                    // `out` grows to the size of a whole file: rescanning all of
                    // it once per token is quadratic for nothing.
                    let mut at = out.len().saturating_sub(marker.len() + piece.len());
                    while at > 0 && !out.is_char_boundary(at) {
                        at -= 1;
                    }
                    if out[at..].contains(marker) {
                        break;
                    }
                }
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

Every number in the report is already measured and every finding is already \
written, with its own title, detail and recommended action. The reader has \
read them. Restating one in different words is worth nothing to them.

Your only job: connect a finding to the specific item in the user's \
`configuration` that produced it. That link is the one thing the report cannot \
compute for itself, and it is the only reason you are here.

Hard rules:
- Annotate a finding ONLY if you can name an item from `configuration` that \
explains it. No named item, no annotation. An empty array is a correct answer.
- Never repeat what a finding's own title, detail or current_action already \
says, in any wording.
- Never state a number, percentage, or multiplier that does not appear verbatim \
in the data you were given. If you want to quote a figure, copy it exactly.
- Never perform arithmetic. Do not add, average, or compare figures to produce a \
new one. Every total you might need has already been computed for you.
- Never speculate about savings. The data says what things cost, not what \
turning them off would achieve.
- Never guess at a cause. If you write \"likely\", \"probably\", \"suggests\", \
\"appears to\" or \"may be\", you are guessing: leave that finding out instead. \
Only say what the data shows.";

/// The saver pass's system prompt.
///
/// Deliberately narrow in the same way [`SYSTEM`] is. The measurements are done
/// and the one-line finding is already written; what a reader cannot get from
/// the row is what the result means for the setup they actually have, and
/// whether it calls for an action. Everything else the model might want to add
/// is either already on screen or a number it is not allowed to invent.
const SAVER_SYSTEM: &str = "\
You write one line of advice per token-saving add-on for a developer tool \
called Piggy.

Each saver in `savers` has already been measured against the user's own \
sessions, with it switched on and with it switched off. `finding` is the \
verdict from that comparison. It is correct, it is already printed on the \
screen your line appears on, and the reader has read it.

So restating it is worth nothing. Your line has to say something the verdict \
does not: what it means for this setup, or what to do now.

Hard rules:
- Never restate a `finding`, in any wording. If all you have is the verdict in \
different words, leave that saver out.
- Never contradict a `finding`.
- `reduced_by_pct` is what the saver took OFF a stream; `increased_by_pct` is \
what it ADDED. A stream carrying `increased_by_pct` got worse with the saver on. \
Never read one of those two as the other.
- A stream with neither figure was NOT measurable. Never call it unchanged, \
unaffected, safe, or free: `result` says why it could not be read, and that is \
all that is known about it.
- Never state a number, percentage, or multiplier that does not appear verbatim \
in the data you were given. If you want to quote a figure, copy it exactly.
- Never perform arithmetic. Every figure you might want has been computed.
- Never guess at a cause. If you write \"likely\", \"probably\", \"suggests\", \
\"appears to\" or \"may be\", you are guessing: leave that saver out instead.
- Never recommend an action the measurement does not support.";

/// The saver pass's task instructions, appended after the sheet.
///
/// The steering matters more here than in the ledger pass. Left to itself, a 4B
/// picks the three savers with the prettiest percentages and writes the verdict
/// back out with "use it consistently" bolted on. The savers actually worth a
/// line are the ones whose measurement says something the percentage does not.
///
/// Built rather than written out because the rejected example below is also the
/// needle [`guard::accept_savers`] matches a pasted-back answer against. Held as
/// two literals they drift, and a needle that appears nowhere in the prompt is a
/// check that runs on every response and can never fire.
fn saver_preamble() -> String {
    format!(
        "\
Those are the measurements. Write about at most three savers, and only ones \
where you have something to add. For each, return an object:
  insight_id  the saver's id, copied exactly from `savers`
  headline    under 120 characters, naming the saver
  why         one or two short sentences: what the reader should do, or what \
they would get wrong about this saver without you

Some savers carry a `caveat`: the part of the measurement that is NOT on the \
reader's screen. Those are the savers to write about, and the caveat is what to \
write. Put it in your own words, name the saver, and say what it means for \
them.

Where there is no caveat, the only line worth writing is about a saver whose \
`finding` says nothing changed: it has been shown to do nothing on THIS \
workload, over the session counts on the sheet, so switching it off would cost \
nothing measurable.

A saver whose streams were all too noisy or too small to compare supports NO \
advice. Leave it out. So does one whose only story is the percentage already in \
its `finding`.

A line that would be REJECTED, because it only says the verdict again:
  {{\"insight_id\":\"saver:example\",\"headline\":\"{headline}\",\
\"why\":\"{why}\"}}

Two sharp lines beat five, and none at all beats one that repeats the verdict. \
Return a JSON array of at most three objects, and nothing else.",
        headline = guard::EXAMPLE_HEADLINE,
        why = guard::EXAMPLE_WHY,
    )
}

const USER_PREAMBLE: &str = "\
That was the report. Annotate at most three findings, and only the ones where a \
named item in `configuration` explains what the finding measured. For each one, \
return an object with:
  insight_id  the finding's id, copied exactly from `findings`
  headline    under 120 characters, naming the configuration item responsible
  why         one or two short sentences: which item it is, what `configuration` \
records about its use, and how it reaches this finding

Every item in `configuration` is an add-on the logs show going unused. That is \
the whole point of naming one: the reader is paying for it and getting nothing.

Each item carries `inflates`: the part of the session floor it is loaded as \
part of. That field is the only way to match an item to a finding. A finding \
whose id ends in the same name is the one that item explains, and if no item's \
`inflates` matches, no item explains that finding.

Findings about session counts, per-project churn, or how long sessions run have \
no cause in `configuration`. Leave them alone. Leave a finding out by not \
writing an object for it: never write one that says nothing explains it.

Two sharp annotations beat five, and none at all beats one that only repeats the \
finding. Do not quote figures unless you are copying one character-for-character \
from the report.

Return a JSON array of at most three objects, and nothing else.";

#[cfg(test)]
mod tests {
    use super::*;

    /// The needle [`guard::accept_savers`] matches a pasted-back answer against
    /// has to be a sentence the model was actually shown.
    ///
    /// Splicing the two constants into the prompt is what makes that true today,
    /// but nothing about splicing them is load-bearing: rewrite the rejected
    /// example around them and the needle goes back to matching text no model has
    /// ever seen. That failure is silent by construction. The check runs on every
    /// response and simply never fires, so the first symptom is a 4B's pasted
    /// example printed as advice.
    ///
    /// Asserted against the built preamble rather than the source, and here
    /// rather than in `tests/`, because building it needs no weights: this is the
    /// half of the prompt that exists before a model is loaded.
    #[test]
    fn the_saver_prompt_shows_the_example_the_guard_matches() {
        let prompt = saver_preamble();
        assert!(
            prompt.contains(guard::EXAMPLE_HEADLINE),
            "guard's needle headline is not in the prompt: {prompt}"
        );
        assert!(
            prompt.contains(guard::EXAMPLE_WHY),
            "guard's needle `why` is not in the prompt: {prompt}"
        );
    }

    /// The pre-flight has to reserve what generation actually spends.
    ///
    /// A sheet that fits the window but not the answer used to clear a 64-token
    /// reserve and then die at `ctx.decode` a few dozen tokens in, throwing away
    /// the annotations written up to that point and reporting "generating" in
    /// place of the size that caused it. Asserted here rather than in `tests/`
    /// because reaching the check through [`Advisor::generate`] needs weights.
    #[test]
    fn the_preflight_reserves_the_whole_generation_budget() {
        // The catalog's smallest window, at the popover budget.
        let ctx = 4096u32;
        // What the old 64-token reserve let through.
        assert!(!fits_context(ctx as usize - 65, ctx, MAX_TOKENS));
        assert!(!fits_context(ctx as usize - MAX_TOKENS, ctx, MAX_TOKENS));
        assert!(fits_context(ctx as usize - MAX_TOKENS - 1, ctx, MAX_TOKENS));

        // And the advice window at its own ceiling, so both budgets are pinned
        // and neither can drift into the other's reserve.
        let ctx = super::super::ADVICE_CTX;
        let max = SUGGEST.max_tokens;
        assert!(!fits_context(ctx as usize - max, ctx, max));
        assert!(fits_context(ctx as usize - max - 1, ctx, max));
    }

    /// A drafting call has to be able to write the file back.
    ///
    /// The cap and the window are two halves of one arithmetic: a rewrite is the
    /// same file again, so the call spends its input twice. This is the check
    /// that catches someone raising the cap to the spec's 12,000 without raising
    /// the window, which would make every draft over ~4k tokens unrunnable.
    #[test]
    fn a_draft_of_a_capped_file_fits_the_advice_window() {
        let ctx = super::super::ADVICE_CTX;
        let cap = DRAFT_INPUT_CAP_TOKENS;
        // Room for the system prompt and the preamble on top of the file.
        let prompt = cap + 512;
        let answer = cap + DRAFT_TOKEN_MARGIN;
        assert!(
            fits_context(prompt, ctx, answer),
            "a {cap}-token file needs {prompt} in and {answer} out, over the {ctx} window"
        );
    }
}
