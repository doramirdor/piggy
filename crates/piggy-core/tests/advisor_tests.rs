//! Tests for the local advisor.
//!
//! Two things are worth testing here and they are not the inference. First, the
//! memory arithmetic that decides whether a user is offered a download at all:
//! getting it wrong the generous way means a 2.5 GB download that swaps an 8GB
//! laptop. Second, the guard, which is the only thing standing between a small
//! model and a fabricated number printed next to a receipt.

use piggy_core::advisor::facts::Facts;
use piggy_core::advisor::guard::{accept, Allowlist};
use piggy_core::advisor::{available, budget, fits, model, recommended, AdvisorModel, CATALOG};
use piggy_core::insights::{Insight, Severity};
use piggy_core::ledger::{Ledger, LedgerRow, ProjectRow};
use piggy_core::parser::{CTX_CONVERSATION, CTX_FLOOR};
use piggy_core::sweep::{SweepItem, SweepReport};

const GB: u64 = 1024 * 1024 * 1024;

fn qwen4b() -> &'static AdvisorModel {
    model("qwen3-4b-instruct-2507").expect("catalog has the 4B")
}

// --- memory arithmetic -----------------------------------------------------

#[test]
fn kv_cache_matches_the_hand_calculation() {
    let m = qwen4b();
    // 2 (K+V) * 8 kv heads * 128 dims = 2048 elements per layer per token,
    // at 34/32 bytes for a q8_0 cache = 2176 bytes, * 36 layers * 4096 ctx.
    let expected = 2176u64 * 36 * 4096;
    assert_eq!(m.kv_bytes(), expected);
    // Sanity against the number that motivated quantizing the cache at all:
    // the same geometry at f16 is 144 KiB per token.
    assert_eq!(2 * 8 * 128 * 2 * 36, 147_456);
}

#[test]
fn sliding_window_attention_is_much_cheaper_at_context() {
    let gemma = model("gemma-3-4b-it").unwrap();
    let qwen = qwen4b();
    // Gemma runs at 2x the context of the Qwen build and still caches less,
    // because only 5 of its 34 layers attend globally.
    assert!(gemma.ctx > qwen.ctx);
    assert!(
        gemma.kv_bytes() < qwen.kv_bytes(),
        "gemma {} should cache less than qwen {} despite 2x context",
        gemma.kv_bytes(),
        qwen.kv_bytes()
    );
}

#[test]
fn sliding_layers_stop_growing_with_context() {
    // A model whose local layers are already saturated pays only for its global
    // layers as context grows. This is the property that makes Gemma the one to
    // grow if follow-up questions ever land.
    let base = AdvisorModel {
        ctx: 8192,
        ..*model("gemma-3-4b-it").unwrap()
    };
    let doubled = AdvisorModel { ctx: 16384, ..base };
    let growth = doubled.kv_bytes() as f64 / base.kv_bytes() as f64;
    assert!(
        growth < 1.8,
        "doubling context should cost far less than 2x, got {growth:.2}x"
    );
}

#[test]
fn peak_includes_weights_kv_and_compute() {
    let m = qwen4b();
    assert!(m.peak_bytes() > m.bytes + m.kv_bytes());
    // The claim that motivated the 4k context choice: this fits in ~3.1 GB.
    assert!(
        m.peak_bytes() < 3_300_000_000,
        "4B peak was {} bytes",
        m.peak_bytes()
    );
}

#[test]
fn every_catalog_model_runs_on_8gb() {
    // The whole point of the catalog. If this fails we are shipping a picker
    // entry that cannot run on the machine it was chosen for.
    for m in CATALOG {
        assert!(
            fits(m, 8 * GB),
            "{} needs {} bytes, over the 8GB budget of {}",
            m.id,
            m.peak_bytes(),
            budget(8 * GB)
        );
    }
}

#[test]
fn budget_reserves_room_for_the_rest_of_the_machine() {
    // Never plan around more than 60% of RAM, and never less than 3 GB reserved.
    assert_eq!(budget(8 * GB), 8 * GB - (8 * GB as u64) * 2 / 5);
    assert!(budget(4 * GB) < 2 * GB);
    assert!(budget(64 * GB) < 40 * GB);
}

#[test]
fn oversized_models_are_never_offered() {
    let huge = AdvisorModel {
        id: "huge",
        bytes: 40 * GB,
        ..*qwen4b()
    };
    assert!(!fits(&huge, 8 * GB));
    assert!(!fits(&huge, 16 * GB));
}

#[test]
fn a_tiny_host_is_offered_nothing_rather_than_the_smallest() {
    // 4 GB leaves under a gigabyte of budget. Offering the 1.7B anyway would be
    // the trap the gate exists to prevent.
    assert!(available(4 * GB).is_empty());
    assert!(recommended(4 * GB).is_none());
}

#[test]
fn recommendation_is_the_largest_that_fits() {
    let pick = recommended(8 * GB).expect("something fits 8GB");
    for m in available(8 * GB) {
        assert!(pick.bytes >= m.bytes);
    }
}

#[test]
fn catalog_ids_and_digests_are_well_formed() {
    for m in CATALOG {
        assert_eq!(m.sha256.len(), 64, "{} digest is not a sha256", m.id);
        assert!(m.sha256.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(m.bytes > 0);
        assert!(m.file.ends_with(".gguf"));
        assert_eq!(CATALOG.iter().filter(|o| o.id == m.id).count(), 1);
    }
}

// --- fact sheet ------------------------------------------------------------

fn ledger() -> Ledger {
    Ledger {
        rows: vec![
            LedgerRow { kind: CTX_FLOOR.into(), tokens: 700_000, n: 100 },
            LedgerRow { kind: CTX_CONVERSATION.into(), tokens: 250_000, n: 100 },
            LedgerRow { kind: "hook_success".into(), tokens: 50_000, n: 80 },
        ],
        projects: vec![ProjectRow {
            project: "/Users/dor/Documents/code/Stacked".into(),
            sessions: 40,
            msgs: 90,
            floor_tokens: 700_000,
            work_tokens: 300_000,
        }],
        cost_units: 1_400_000.0,
        write_weight: 1.4,
    }
}

fn findings() -> Vec<Insight> {
    vec![Insight {
        id: "floor-dominates".into(),
        severity: Severity::High,
        title: "70% of your tokens went to starting sessions".into(),
        detail: "700,000 of 1,000,000 cache-write tokens were the session floor.".into(),
        tokens: 700_000,
        action: "Fewer, longer sessions pay this once.".into(),
    }]
}

#[test]
fn facts_carry_the_ids_the_model_may_annotate() {
    let f = Facts::build(&ledger(), &findings(), None);
    assert_eq!(f.insight_ids, vec!["floor-dominates".to_string()]);
    assert_eq!(f.value["totals"]["startup_pct"], 70);
}

#[test]
fn facts_use_basenames_not_home_directory_paths() {
    let f = Facts::build(&ledger(), &findings(), None);
    let s = f.value.to_string();
    assert!(s.contains("Stacked"));
    assert!(!s.contains("/Users/dor"), "fact sheet leaked a full path");
}

#[test]
fn facts_precompute_sums_so_the_model_never_adds() {
    let sweep = SweepReport {
        sessions_considered: 200,
        items: vec![
            SweepItem {
                idx: 1,
                kind: "skill".into(),
                id: "unused-skill".into(),
                source: None,
                used: 0,
                used_windowed: false,
                est_tokens: 1_200,
                recommend_disable: true,
                reason: "never invoked".into(),
            },
            SweepItem {
                idx: 2,
                kind: "mcp".into(),
                id: "quiet-server".into(),
                source: None,
                used: 0,
                used_windowed: true,
                est_tokens: 800,
                recommend_disable: true,
                reason: "never invoked".into(),
            },
        ],
    };
    let f = Facts::build(&ledger(), &findings(), Some(&sweep));
    // 2 and 2000 are facts, so the model may quote them. It could not have
    // derived either on its own without doing arithmetic.
    assert_eq!(f.value["configuration"]["unused_count"], 2);
    assert_eq!(f.value["configuration"]["unused_tokens_per_session"], 2_000);
}

// --- the guard -------------------------------------------------------------

fn allow() -> Allowlist {
    Allowlist::from_facts(&Facts::build(&ledger(), &findings(), None))
}

#[test]
fn numbers_inside_fact_strings_are_quotable() {
    // The figure only ever appears inside an insight's prose, never as a JSON
    // number. If string walking regressed, the model could not quote its own
    // finding back.
    assert!(allow().offenders("700,000 tokens went to startup").is_empty());
}

#[test]
fn fabricated_figures_are_caught() {
    let a = allow();
    assert_eq!(a.offenders("you wasted 918,273 tokens"), vec!["918273"]);
    assert_eq!(a.offenders("that is 42% of your spend"), vec!["42"]);
    assert!(!a.offenders("this would save you 3.7x").is_empty());
}

#[test]
fn rounding_a_real_fact_is_allowed_but_inventing_a_neighbour_is_not() {
    let f = Facts::build(&ledger(), &findings(), None);
    let a = Allowlist::from_facts(&f);
    // msgs_per_session is 2.25, stored rounded to 2.3.
    assert!(a.offenders("about 2.3 messages").is_empty());
    assert!(a.offenders("about 2 messages").is_empty());
    // But a value that is not a rounding of anything must not slip through.
    assert!(!a.offenders("about 9 messages").is_empty());
}

#[test]
fn abbreviations_expand_strictly() {
    let a = allow();
    // 700,000 is a fact, so "700k" is the same fact written shorter.
    assert!(a.offenders("700k tokens").is_empty());
    // 40 is a fact (sessions). "40k" is NOT, and must not borrow it.
    assert!(!a.offenders("40k tokens").is_empty());
}

#[test]
fn sentence_punctuation_is_not_a_decimal_point() {
    // "...floor is 70. Fewer sessions..." must read as 70, not 70.0-something,
    // and must not smear into the next sentence.
    let a = allow();
    assert!(a.offenders("startup is 70. Fewer sessions help.").is_empty());
}

#[test]
fn annotations_must_name_a_real_finding() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = r#"[
      {"insight_id":"floor-dominates","headline":"Startup dominates","why":"Most sessions are short."},
      {"insight_id":"invented-finding","headline":"Something else","why":"Made up."}
    ]"#;
    let got = accept(raw, &f).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].insight_id, "floor-dominates");
}

#[test]
fn annotations_with_invented_numbers_are_dropped_entirely() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = r#"[{"insight_id":"floor-dominates","headline":"You burned 88% on startup","why":"Short sessions."}]"#;
    assert!(accept(raw, &f).unwrap().is_empty());
}

#[test]
fn a_code_fence_or_preamble_does_not_lose_valid_output() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = "Sure, here you go:\n```json\n[{\"insight_id\":\"floor-dominates\",\"headline\":\"Startup dominates\",\"why\":\"Short sessions.\"}]\n```";
    assert_eq!(accept(raw, &f).unwrap().len(), 1);
}

#[test]
fn one_annotation_per_finding() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = r#"[
      {"insight_id":"floor-dominates","headline":"First","why":"One."},
      {"insight_id":"floor-dominates","headline":"Second","why":"Two."}
    ]"#;
    let got = accept(raw, &f).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].headline, "First");
}

#[test]
fn overlong_or_empty_annotations_are_dropped() {
    let f = Facts::build(&ledger(), &findings(), None);
    let long = "x".repeat(500);
    let raw = format!(
        r#"[{{"insight_id":"floor-dominates","headline":"ok","why":"{long}"}},
            {{"insight_id":"floor-dominates","headline":"","why":"empty headline"}}]"#
    );
    assert!(accept(&raw, &f).unwrap().is_empty());
}

/// The fact sheet has to fit in the context window alongside the instructions
/// and the answer, and the fixtures above cannot prove that: they have one
/// finding and one project, where a real tree has dozens of each.
///
/// Ignored because it reads the developer's own `~/.piggy`. Run with
/// `cargo test -p piggy-core --test advisor_tests -- --ignored --nocapture`.
#[test]
#[ignore = "reads the real ~/.piggy database"]
fn real_data_fact_sheet_fits_the_context_window() {
    use piggy_core::{config, sweep, Pricing, Store};

    let home = config::piggy_home();
    let store = Store::open(&home).expect("open the real store");
    let pricing = Pricing::load(&home);
    let ledger = store.ledger(None, &pricing).expect("build the real ledger");
    let found = piggy_core::insights(&ledger);
    let swept = sweep::scan(&store, 200).ok();
    let f = Facts::build(&ledger, &found, swept.as_ref());

    // The exact string the prompt sends, not a re-serialization of it.
    let json = f.prompt_json();
    // ~3.5 chars per token is a deliberate over-estimate for dense JSON, which
    // tokenizes worse than prose.
    let est = json.len() / 3;
    println!(
        "fact sheet: {} bytes, ~{est} tokens, {} findings, allow-list {} numbers",
        json.len(),
        f.insight_ids.len(),
        Allowlist::from_facts(&f).len()
    );

    // The catalog's smallest window is 4096, and generation needs room to answer.
    assert!(
        est < 2800,
        "fact sheet is ~{est} tokens, too close to the 4096 window"
    );
}

/// A truncated response must not cost us the items that did complete.
///
/// Taken from a real Llama 3.2 3B run that hit the token ceiling partway through
/// its fourth object. Before the salvage, the unterminated string killed the
/// parse and all three complete annotations were discarded.
#[test]
fn a_truncated_array_keeps_its_complete_items() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = r#"[
  {
    "insight_id": "floor-dominates",
    "headline": "Startup dominates",
    "why": "Short sessions."
  },
  {
    "insight_id": "floor-dominates",
    "headline": "Duplicate, should be dropped",
    "why": "Two."
  },
  {
    "insight_id": "churn:/Users/dor/code/qwen_bench",
    "headline": "qwen_bench ran many sessions with high overhe"#;
    let got = accept(raw, &f).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].headline, "Startup dominates");
}

#[test]
fn a_bracket_inside_a_string_does_not_end_the_array_early() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = r#"[{"insight_id":"floor-dominates","headline":"Your [skills] load at startup","why":"See the ] bracket."}]"#;
    let got = accept(raw, &f).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].headline, "Your [skills] load at startup");
}

#[test]
fn a_truncation_before_any_complete_item_yields_nothing() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = r#"[{"insight_id":"floor-dominates","headline":"cut off here"#;
    assert!(accept(raw, &f).is_err());
}

/// Verbatim from a live Qwen3 4B run. The prompt already forbade hedging and it
/// hedged anyway, which is why this is enforced here rather than asked for there.
#[test]
fn hedged_guesses_at_a_cause_are_dropped() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = r#"[
      {"insight_id":"floor-dominates","headline":"Startup dominates",
       "why":"This suggests the hook is configured to run on every session."},
      {"insight_id":"per-turn-injections","headline":"A hook fires every turn",
       "why":"It may be enabled globally."}
    ]"#;
    assert!(accept(raw, &f).unwrap().is_empty());
}

#[test]
fn a_confident_claim_is_kept() {
    let f = Facts::build(&ledger(), &findings(), None);
    let raw = r#"[{"insight_id":"floor-dominates","headline":"Startup dominates",
                   "why":"Your skill listing is loaded before the first message in every project."}]"#;
    assert_eq!(accept(raw, &f).unwrap().len(), 1);
}

#[test]
fn garbage_fails_closed_rather_than_panicking() {
    let f = Facts::build(&ledger(), &findings(), None);
    assert!(accept("I refuse to answer.", &f).is_err());
    assert!(accept("", &f).is_err());
    // Valid JSON of the wrong shape is an error, not a silent empty success.
    assert!(accept("{\"insight_id\":\"floor-dominates\"}", &f).is_err());
}
