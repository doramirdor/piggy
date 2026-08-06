//! Every string the advice pass sends to a model, and the two constants the
//! guard matches a pasted-back answer against.
//!
//! Deliberately **not** feature-gated. The M4 prompts live inside
//! [`super::llama`], which only compiles under `local-llm`, so the coupling test
//! that keeps the guard's needle and the prompt's example in step runs in a
//! build almost nobody makes. Moving the M5 prompts here means their coupling
//! test runs in the default build that CI runs.
//!
//! ## Output shape, and why it is not a grammar
//!
//! The rank call asks for JSON and gets it checked by a strict parser plus one
//! bounded retry. It is **not** constrained by a GBNF grammar, and the reason is
//! in [`super::llama`]'s module doc: `llama_sampler_sample` accepts the sampled
//! token inside C++ before Rust regains control, so the end-of-generation check
//! that would make a grammar safe cannot be placed anywhere useful, and the
//! abort it guards against takes the whole app down. [`SUGGEST_GBNF`] and the
//! protocol that would be required are kept below behind [`GRAMMAR`], off.
//!
//! The drafting call sends no JSON at all. A markdown file inside a JSON string
//! means every newline is `\n` and every quote is `\"`, and a 4B gets that wrong
//! often enough that escaping becomes the failure mode rather than the writing.
//! Two sentinels remove the problem outright, and they give the generation loop
//! a stop condition that a file full of braces cannot trip.

/// The rank call's system prompt.
///
/// The register follows [`super::llama`]'s two M4 system prompts, re-pointed at
/// a different job: there the model explains a finding, here it orders a menu of
/// actions pure code already computed and says why the first one is first.
pub const SUGGEST_SYSTEM: &str = "\
You order a list of suggested actions for a developer tool called Piggy.

Every candidate was computed by pure code from the user's own logs, and every \
number on it is already measured and already rounded. You rank the candidates \
and explain them. You never add one, never remove one, never change a target, \
and never change a number.

Hard rules:
- Order `picks` by what this user should do first. The biggest figure is the \
default, but a smaller action that is reversible and low risk can come first: \
when it does, say so in the rationale.
- A candidate whose `est_is` is \"burden\" has NOT been shown to save that \
much. The figure is what the target costs today and the ceiling on what a \
rewrite could give back. Never call it a saving.
- Never state a number, percentage or multiplier that does not appear verbatim \
in the data you were given. If you want to quote a figure, copy it exactly.
- Never perform arithmetic. Do not add, average or compare figures to produce a \
new one. Every total you might need has already been computed for you.
- Never guess at a cause. If you write \"likely\", \"probably\", \"suggests\", \
\"appears to\" or \"may be\", you are guessing: leave that candidate out instead.
- A stream carrying neither `reduced_by_pct` nor `increased_by_pct` was NOT \
measurable. Never call it unchanged, unaffected, safe or free.
- Name what each pick is about. A rationale that names none of the candidate's \
`about` entries explains nothing.
- Fewer, sharper picks beat more. An empty `picks` array is a correct answer.";

/// The rank call's task instructions, appended **after** the sheet.
///
/// After, for the reason recorded in [`super::llama::Advisor`]'s prompt builder:
/// a small model that reads the task before two thousand tokens of dense JSON
/// has forgotten it by the end. The advice sheet is several times larger than
/// the M4 ones, so that matters more here, not less.
///
/// Built rather than written out because the rejected example below is also the
/// needle [`super::guard::accept_suggestion`] matches a pasted-back answer
/// against. Held as two separate literals they drift, and a needle that appears
/// nowhere in the prompt is a check that runs on every response and can never
/// fire.
pub fn suggest_preamble() -> String {
    format!(
        "\
That was everything Piggy knows. Rank the `candidates` and return ONE JSON \
object, and nothing else:

  {{\"picks\":[{{\"id\":\"...\",\"why\":\"...\"}}],\
\"bundles\":[{{\"project\":\"...\",\"ids\":[\"...\",\"...\"]}}]}}

  id       a candidate id, copied exactly from `candidates`
  why      one or two short sentences, under {max} characters: what this action \
does for this user, and why it is where you put it. Name the candidate's \
subject. Do not repeat its `title`.

`bundles` is optional. Add one only when two or more of your picks are about \
the same project, and use the project name exactly as `projects` spells it. \
Leave the whole key out when nothing groups.

Pick at most {picks}. A pick you cannot explain without guessing is a pick to \
leave out.

A pick that would be REJECTED, because it says only what the card already says:
  {{\"id\":\"{id}\",\"why\":\"{why}\"}}

Return only the JSON object, starting with {{ and ending with }}.",
        max = super::guard::MAX_RATIONALE,
        picks = super::guard::MAX_PICKS,
        id = SUGGEST_EXAMPLE_ID,
        why = SUGGEST_EXAMPLE_WHY,
    )
}

/// The worked example in the rank prompt, which is **also** the needle
/// [`super::guard::accept_suggestion`] matches a pasted-back answer against.
///
/// These two constants are the only copy. The prompt splices them in. An earlier
/// version of the saver pass kept a second copy inside the guard, the prompt was
/// rewritten, and the check spent that whole time matching a sentence no model
/// had ever been shown. That failure is silent by construction: the check runs
/// on every response and can never fire.
///
/// The id is a real-looking `server-disable` id that no generator can produce
/// (sixteen zeroes), so a model copying the example wholesale is rejected by the
/// id check as well as by this one.
pub const SUGGEST_EXAMPLE_ID: &str = "server-disable-0000000000000000";
pub const SUGGEST_EXAMPLE_WHY: &str =
    "This server is unused and turning it off saves tokens in every session.";

/// The drafting call's system prompt.
///
/// A different job from ranking, and it says so. The one thing this prompt has
/// to get across is that shortening is not improving: the file is the user's own
/// guidance, and v1 is explicitly "trims and fixes only" (docs/m5-spec.md).
pub const DRAFT_SYSTEM: &str = "\
You shorten a file the user wrote. You are not rewriting it and you are not \
improving it.

Remove what repeats, what says the same thing twice in different words, and \
what no longer applies. Keep every instruction the user still relies on, in \
their words.

Hard rules:
- Never add guidance. Not a rule, not a suggestion, not a note about what you \
changed.
- Never introduce a file path, a URL, a heading or a number that is not already \
in the file you were given.
- Never reword an instruction into something narrower or broader than it was.
- Headings may be merged or dropped. A heading that was not there may not appear.
- Output the whole file, from its first line to its last, between the two \
markers and with nothing before or after them.";

/// The drafting call's task instructions, appended after the file.
///
/// Names the file so the model knows what it is editing. The label is never
/// trusted back: nothing the model returns is read as a path.
pub fn draft_preamble(label: &str) -> String {
    format!(
        "\
That was {label}, in full.

Write the shortened version. It has to be shorter by at least a tenth, and \
preferably more. Everything the user still relies on stays; everything that \
repeats goes.

Put the whole file between these two markers, exactly:

{DRAFT_OPEN}
(the shortened file)
{DRAFT_CLOSE}

Nothing before the first marker and nothing after the second."
    )
}

/// The markers a draft comes back between.
///
/// Unlikely in a real CLAUDE.md and cheap to scan for, which is what makes them
/// usable as the generation loop's stop condition.
pub const DRAFT_OPEN: &str = "<<<PIGGY_DRAFT>>>";
pub const DRAFT_CLOSE: &str = "<<<END_PIGGY_DRAFT>>>";

// ---------------------------------------------------------------------------
// The grammar, kept and switched off
// ---------------------------------------------------------------------------

/// Whether the rank call constrains sampling with [`SUGGEST_GBNF`].
///
/// **Off, and flipping it needs the manual sampling loop below.** The pinned
/// `llama-cpp-2 0.1.153` does expose the grammar API, and two of its three
/// historical failure modes are fixed upstream: a mid-generation rejection now
/// throws instead of aborting, and the sys crate catches it. The third is not
/// fixed and cannot be worked around through the API this codebase uses:
/// `llama_sampler_sample` (`llama.cpp/src/llama-sampler.cpp:870`) calls
/// `llama_sampler_accept` directly in C++, inside the same function that picks
/// the token, so the token is already accepted before Rust regains control. That
/// internal accept is not the try/catch-wrapped `llama_rs_sampler_accept` the
/// sys crate exposes, and `llama-grammar.cpp:1428-1435` is still a `GGML_ABORT`
/// when an end-of-generation token is accepted into a grammar that is not in an
/// accepting state. `GGML_ABORT` is not a C++ exception, so nothing catches it,
/// and in a menu bar app the process is the product.
///
/// Kept rather than deleted because the option is real, the text below is
/// verified against the vendored parser, and the work to make it safe is
/// written down rather than rediscovered.
///
/// Flipping this means abandoning `LlamaSampler::sample()` for a manual loop,
/// which is the whole cost of the trade:
///
/// ```text
/// let mut grammar = LlamaSampler::grammar(&self.model, SUGGEST_GBNF, "root")?;
/// loop {
///     let mut arr = ctx.token_data_array_ith(-1);
///     arr.apply_sampler(&grammar);            // masks invalid tokens
///     let token = arr.sample_token_greedy();  // selects, does NOT accept
///     if self.model.is_eog_token(token) { break; }   // BEFORE any accept
///     if grammar.try_accept(token).is_err() {
///         // The stacks are already empty (llama-grammar.cpp:1503 runs before
///         // the throw at :1506), so one more apply would hit the assert at
///         // :940. End the generation and drop the sampler; never continue.
///         break;
///     }
///     // ... decode, as today
/// }
/// ```
///
/// And a live test that drives the grammar into a rejection and asserts the
/// process is still alive afterwards is the sign-off gate for the flip, not a
/// nicety: without it the abort is discovered by a user.
pub const GRAMMAR: bool = false;

/// The rank call's grammar. UNUSED unless [`GRAMMAR`] is flipped on.
///
/// Static by design: enumerating candidate ids here is what would make it change
/// per call, and a static grammar is one a single live test can prove
/// non-degenerate at init.
///
/// Verified against the vendored parser: `\x` with two hex digits, `\"`, `\\`,
/// `\[`, `\]`, `\t`, `\r` and `\n` are the supported escapes
/// (`llama-grammar.cpp:162-183`), and `/`, `b` and `f` inside a character class
/// are ordinary characters that need no escape.
pub const SUGGEST_GBNF: &str = r#"
root    ::= "{" ws "\"picks\"" ws ":" ws picks (ws "," ws "\"bundles\"" ws ":" ws bundles)? ws "}"
picks   ::= "[" ws (pick (ws "," ws pick)*)? ws "]"
pick    ::= "{" ws "\"id\"" ws ":" ws string ws "," ws "\"why\"" ws ":" ws string ws "}"
bundles ::= "[" ws (bundle (ws "," ws bundle)*)? ws "]"
bundle  ::= "{" ws "\"project\"" ws ":" ws string ws "," ws "\"ids\"" ws ":" ws ids ws "}"
ids     ::= "[" ws (string (ws "," ws string)*)? ws "]"
string  ::= "\"" char* "\""
char    ::= [^\"\\\x00-\x1F] | "\\" esc
esc     ::= [\"\\/bfnrt] | "u" hex hex hex hex
hex     ::= [0-9a-fA-F]
ws      ::= [ \t\n]*
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The needle the guard matches a pasted-back answer against has to be a
    /// sentence the model was actually shown.
    #[test]
    fn the_suggest_prompt_shows_the_example_the_guard_matches() {
        let prompt = suggest_preamble();
        assert!(prompt.contains(SUGGEST_EXAMPLE_ID), "{prompt}");
        assert!(prompt.contains(SUGGEST_EXAMPLE_WHY), "{prompt}");
    }

    /// The parser looks for the markers the prompt asked for.
    #[test]
    fn the_draft_prompt_shows_the_sentinels_the_parser_looks_for() {
        let prompt = draft_preamble("your global CLAUDE.md");
        assert!(prompt.contains(DRAFT_OPEN), "{prompt}");
        assert!(prompt.contains(DRAFT_CLOSE), "{prompt}");
        assert!(prompt.contains("your global CLAUDE.md"), "{prompt}");
    }
}
