//! Live end-to-end check of the local advisor.
//!
//! Everything here touches the real world: it downloads gigabytes from Hugging
//! Face into the real `~/.piggy/models`, and it reads the developer's own
//! session database. So it is both feature-gated and `#[ignore]`d, and it is the
//! only test in the crate that is.
//!
//! It exists because the rest of the suite cannot prove the two things most
//! likely to be wrong: that the download and its sha256 gate actually work
//! against the live CDN, and that a 1.7B model's real output survives the
//! grammar and the guard. Both are the sort of thing that compiles perfectly and
//! fails on contact.
//!
//! ```text
//! cargo test -p piggy-core --features local-llm,metal --test advisor_live_tests \
//!   -- --ignored --nocapture
//! ```
#![cfg(feature = "local-llm")]

use std::sync::atomic::AtomicBool;
use std::time::Instant;

use piggy_core::advisor::facts::Facts;
use piggy_core::advisor::guard::Allowlist;
use piggy_core::advisor::llama::Advisor;
use piggy_core::advisor::{download, model};
use piggy_core::{config, sweep, Pricing, Store};

#[test]
#[ignore = "downloads ~1.1 GB and runs a real model"]
fn downloads_verifies_and_generates() {
    // Defaults to the model most users will actually get. Override with
    // `PIGGY_ADVISOR_TEST_MODEL=<id>` to exercise a different tier without
    // editing this file, since the tiers differ in quality far more than in
    // plumbing and it is the quality that needs re-checking.
    let id = std::env::var("PIGGY_ADVISOR_TEST_MODEL")
        .unwrap_or_else(|_| "qwen3-4b-instruct-2507".to_string());
    let spec = model(&id).unwrap_or_else(|| panic!("no catalog model named {id}"));
    println!(
        "model: {} ({} bytes, peak ~{:.1} GB, ctx {})",
        spec.name,
        spec.bytes,
        spec.peak_bytes() as f64 / 1e9,
        spec.ctx
    );

    // --- download + verify ---
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let mut last = 0u64;
    download::fetch(spec, &cancel, |received, total| {
        let pct = if total == 0 { 0 } else { received * 100 / total };
        if pct >= last + 10 {
            last = pct;
            println!("  {pct}% ({received}/{total})");
        }
    })
    .expect("download and verify the weights");
    println!("downloaded in {:?}", started.elapsed());

    // Re-verify from scratch: `fetch` short-circuits on an already-present file,
    // so this is what actually proves the digest gate rather than the cache.
    download::verify(spec).expect("weights pass their sha256");
    println!("sha256 verified: {}", &spec.sha256[..16]);

    // --- real fact sheet ---
    let home = config::piggy_home();
    let store = Store::open(&home).expect("open the real store");
    let pricing = Pricing::load(&home);
    let ledger = store.ledger(None, &pricing).expect("build the ledger");
    let found = piggy_core::insights(&ledger);
    let swept = sweep::scan(&store, 200).ok();
    let facts = Facts::build(&ledger, &found, swept.as_ref());
    let allow = Allowlist::from_facts(&facts);
    println!(
        "facts: {} findings, {} chars, {} allowed numbers",
        facts.insight_ids.len(),
        facts.prompt_json().len(),
        allow.len()
    );
    assert!(!facts.insight_ids.is_empty(), "no findings to annotate");

    // --- load + generate ---
    let t = Instant::now();
    let advisor = Advisor::load(spec).expect("load the model");
    println!("loaded in {:?}", t.elapsed());

    let t = Instant::now();
    let raw = advisor.annotate_raw(&facts).expect("generate");
    println!("generated in {:?}\n--- raw ---\n{raw}\n-----------", t.elapsed());

    // No grammar constrains the sampler any more (see `llama.rs` for why), so a
    // preamble is allowed and only the extracted array has to parse. If the model
    // returned no array at all that is a legitimate miss, not a test failure:
    // `accept` will report it below and the UI would simply show no annotations.
    if let (Some(s), Some(e)) = (raw.find('['), raw.rfind(']')) {
        let parsed: serde_json::Value =
            serde_json::from_str(&raw[s..=e]).expect("the emitted array parses");
        assert!(parsed.is_array());
    }

    // --- the guard, on real generated text ---
    let accepted = advisor.annotate(&facts).expect("guard");
    println!("\n{} annotation(s) survived the guard:", accepted.len());
    for a in &accepted {
        println!("  [{}] {}\n    {}", a.insight_id, a.headline, a.why);
    }

    // Whatever survived must name a real finding and contain no invented figure.
    // This is the invariant the whole design exists to hold, checked here against
    // text a model actually wrote rather than a fixture we authored.
    for a in &accepted {
        assert!(
            facts.insight_ids.contains(&a.insight_id),
            "annotation named a finding that does not exist: {}",
            a.insight_id
        );
        assert!(
            allow.offenders(&a.headline).is_empty(),
            "headline contained a fabricated number: {}",
            a.headline
        );
        assert!(
            allow.offenders(&a.why).is_empty(),
            "why contained a fabricated number: {}",
            a.why
        );
    }
}

/// The per-saver advice pass, against the real database and the real model.
///
/// Separate from the download test above because it needs no network and is the
/// one that has to be re-read by a human: the guard can prove no number was
/// invented, but only a reader can tell whether the advice is worth printing.
///
/// ```text
/// cargo test -p piggy-core --features local-llm,metal --test advisor_live_tests \
///   -- --ignored --nocapture explains_savers
/// ```
#[test]
#[ignore = "runs a real model against the developer's own database"]
fn explains_savers_from_real_attribution() {
    let id = std::env::var("PIGGY_ADVISOR_TEST_MODEL")
        .unwrap_or_else(|_| "qwen3-4b-instruct-2507".to_string());
    let spec = model(&id).unwrap_or_else(|| panic!("no catalog model named {id}"));
    download::verify(spec).expect("weights present and verified");

    let home = config::piggy_home();
    let store = Store::open(&home).expect("open the real store");
    let pricing = Pricing::load(&home);
    let catalog = piggy_core::Catalog::embedded();
    let rate_map = store.session_rate_map(&pricing).expect("rate map");

    // A fixed seed: the bootstrap must not be what makes this run differ from
    // the last one when the prompt is what changed.
    let attribs: Vec<_> = catalog
        .entries
        .iter()
        .filter_map(|e| {
            let a = piggy_core::attribution::attribute_with_map(&store, &rate_map, &e.id, 42).ok()?;
            Some((e, a))
        })
        .collect();
    let rows: Vec<_> = attribs.iter().map(|(e, a)| (*e, a)).collect();
    let facts = Facts::savers(&rows);
    let allow = Allowlist::from_facts(&facts);
    println!(
        "saver sheet: {} savers, {} chars, {} allowed numbers",
        facts.insight_ids.len(),
        facts.prompt_json().len(),
        allow.len()
    );
    println!("{}", facts.prompt_json());
    assert!(
        !facts.insight_ids.is_empty(),
        "no saver has both arms of a comparison yet"
    );

    let t = Instant::now();
    let advisor = Advisor::load(spec).expect("load the model");
    println!("loaded in {:?}", t.elapsed());

    let t = Instant::now();
    let raw = advisor.explain_savers_raw(&facts).expect("generate");
    println!("generated in {:?}\n--- raw ---\n{raw}\n-----------", t.elapsed());

    let accepted = advisor.explain_savers(&facts).expect("guard");
    println!("\n{} line(s) survived the guard:", accepted.len());
    for a in &accepted {
        println!("  [{}] {}\n    {}", a.insight_id, a.headline, a.why);
    }

    for a in &accepted {
        assert!(
            facts.insight_ids.contains(&a.insight_id),
            "advice named a saver that is not on the sheet: {}",
            a.insight_id
        );
        assert!(allow.offenders(&a.headline).is_empty(), "{}", a.headline);
        assert!(allow.offenders(&a.why).is_empty(), "{}", a.why);
    }
}

// ---------------------------------------------------------------------------
// M5.4: the advice pass
// ---------------------------------------------------------------------------

/// The rank pass, against the real database and the real model.
///
/// The one thing the default-build suite cannot show: whether a 4B, given the
/// whole structured picture, ranks a real candidate list in an order a person
/// would recognise. The guard can prove no number was invented; only a reader
/// can tell whether the sentences are worth printing.
///
/// ```text
/// cargo test -p piggy-core --features local-llm,metal --test advisor_live_tests \
///   -- --ignored --nocapture suggest_survives_the_guard
/// ```
#[test]
#[ignore = "runs a real model against the developer's own database"]
fn suggest_survives_the_guard() {
    use piggy_core::advice;
    use piggy_core::advisor::facts::{AdviceInput, Facts as F};

    let id = std::env::var("PIGGY_ADVISOR_TEST_MODEL")
        .unwrap_or_else(|_| "qwen3-4b-instruct-2507".to_string());
    let spec = model(&id).unwrap_or_else(|| panic!("no catalog model named {id}"));
    download::verify(spec).expect("weights present and verified");

    let home = config::piggy_home();
    let mut store = Store::open(&home).expect("open the real store");
    let pricing = Pricing::load(&home);
    let catalog = piggy_core::Catalog::embedded();
    let state = piggy_core::PiggyState::load().expect("state");
    let opts = advice::GenerateOptions::new(&catalog, &pricing, &state);

    let inputs = advice::load_inputs(&mut store, &opts).expect("load the generators' inputs");
    let candidates = advice::generate_from(&inputs);
    let ledger = store
        .ledger(piggy_core::Period::Month.cutoff().as_deref(), &pricing)
        .expect("ledger");
    let found = piggy_core::insights(&ledger);
    let facts = F::advice(&AdviceInput {
        ledger: &ledger,
        trend: None,
        insights: &found,
        sweep: Some(&inputs.sweep),
        manifests: &inputs.manifests,
        server_usage: &inputs.server_usage,
        claudemd: &inputs.claudemd,
        project_mcp: &inputs.project_mcp,
        savers: &[],
        headline: None,
        candidates: &candidates,
    });

    let allow = Allowlist::from_facts(&facts);
    println!(
        "advice sheet: {} candidates, {} chars (~{} tokens), {} allowed numbers, hash {}",
        facts.candidate_ids.len(),
        facts.prompt_json().len(),
        facts.prompt_json().len() / 3,
        allow.len(),
        facts.hash()
    );
    assert!(
        !facts.candidate_ids.is_empty(),
        "no candidate to rank on this machine"
    );

    let t = Instant::now();
    let advisor = Advisor::load(spec).expect("load the model");
    println!("loaded in {:?}", t.elapsed());

    let t = Instant::now();
    let raw = advisor.suggest_raw(&facts).expect("generate");
    println!("generated in {:?}\n--- raw ---\n{raw}\n-----------", t.elapsed());

    let accepted = advisor.suggest(&facts).expect("guard");
    println!(
        "\n{} pick(s) survived, {} bundle(s):",
        accepted.picks.len(),
        accepted.bundles.len()
    );
    for p in &accepted.picks {
        println!("  [{}]\n    {}", p.id, p.rationale);
    }
    for b in &accepted.bundles {
        println!("  bundle {}: {:?}", b.project, b.ids);
    }

    for p in &accepted.picks {
        assert!(
            facts.candidate_ids.contains(&p.id),
            "a pick named a candidate that is not on the sheet: {}",
            p.id
        );
        assert!(allow.offenders(&p.rationale).is_empty(), "{}", p.rationale);
    }
}

/// A real rewrite of the developer's own largest CLAUDE.md.
///
/// Prints the shrink percentage and any rejection, because both are the answer:
/// a draft that the guard refuses is a designed state, and knowing *which* rule
/// refused it is the difference between a prompt problem and a rule problem.
///
/// ```text
/// cargo test -p piggy-core --features local-llm,metal --test advisor_live_tests \
///   -- --ignored --nocapture a_real_draft_survives_the_guard
/// ```
#[test]
#[ignore = "runs a real model against the developer's own CLAUDE.md files"]
fn a_real_draft_survives_the_guard() {
    use piggy_core::advisor::draft;
    use piggy_core::claudemd;

    let id = std::env::var("PIGGY_ADVISOR_TEST_MODEL")
        .unwrap_or_else(|_| "qwen3-4b-instruct-2507".to_string());
    let spec = model(&id).unwrap_or_else(|| panic!("no catalog model named {id}"));
    download::verify(spec).expect("weights present and verified");

    let home = config::piggy_home();
    let mut store = Store::open(&home).expect("open the real store");
    let report = claudemd::scan(&mut store).expect("scan the real CLAUDE.md files");
    let biggest = report
        .files
        .iter()
        .max_by_key(|f| f.file.est_tokens)
        .expect("at least one CLAUDE.md on this machine");
    let text = claudemd::read_file_text(
        std::path::Path::new(&biggest.file.path),
        biggest.file.project.clone(),
    )
    .expect("read it back");
    println!(
        "drafting {} ({} bytes, ~{} estimated tokens)",
        biggest.file.path, text.bytes, biggest.file.est_tokens
    );

    let t = Instant::now();
    let advisor = Advisor::load(spec).expect("load the model");
    println!("loaded in {:?}", t.elapsed());

    let t = Instant::now();
    let raw = advisor.draft_raw("this file", &text.text).expect("generate");
    println!("generated in {:?}, {} raw chars", t.elapsed(), raw.len());

    match draft::accept_draft(&text.text, &raw) {
        Ok(drafted) => {
            let shrink = 100.0 - (drafted.len() as f64 / text.text.len() as f64) * 100.0;
            println!("accepted: {} -> {} bytes ({shrink:.1}% smaller)", text.text.len(), drafted.len());
            println!("--- draft ---\n{drafted}\n-------------");
            assert!(drafted.len() * 10 <= text.text.len() * 9);
        }
        Err(e) => {
            println!("refused: {}", e.reason());
            println!("--- raw ---\n{raw}\n-----------");
        }
    }
}

/// The advisor's own tokenizer, loaded without the advisor.
///
/// Two properties, and the second is the one that decides whether `piggy probe`
/// can use it at all: the label has to be the model id (anything else flips the
/// UI's estimate badge), and a `vocab_only` load has to be fast enough to sit in
/// front of a probe run rather than being a second and a half of model load.
///
/// ```text
/// cargo test -p piggy-core --features local-llm,metal --test advisor_live_tests \
///   -- --ignored --nocapture the_model_tokenizer_counts_a_real_schema
/// ```
#[test]
#[ignore = "loads real weights"]
fn the_model_tokenizer_counts_a_real_schema() {
    use piggy_core::advisor::tokenizer::ModelTokenizer;
    use piggy_core::probe::{BytesEstimate, SchemaTokenizer};

    let id = std::env::var("PIGGY_ADVISOR_TEST_MODEL")
        .unwrap_or_else(|_| "qwen3-4b-instruct-2507".to_string());
    let spec = model(&id).unwrap_or_else(|| panic!("no catalog model named {id}"));
    download::verify(spec).expect("weights present and verified");

    // Twice, because the first load of a 2.5 GB file off a cold page cache is
    // disk time and not work: measured here, 7.1s cold against 0.17s warm. The
    // second load is the one that says whether this reads a vocabulary or a
    // model, which is the property `piggy probe` depends on.
    let t = Instant::now();
    let _cold = ModelTokenizer::load(spec).expect("load the vocabulary");
    let cold = t.elapsed();
    let t = Instant::now();
    let tokenizer = ModelTokenizer::load(spec).expect("load the vocabulary again");
    let load = t.elapsed();
    println!("vocab-only load: {cold:?} cold, {load:?} warm");

    let schema = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/ok-server.mjs"),
    )
    .expect("a fixture to count");
    let real = tokenizer.count(&schema);
    let estimated = BytesEstimate.count(&schema);
    println!("{} bytes: {real} tokens measured, {estimated} estimated", schema.len());

    assert_eq!(tokenizer.label(), spec.id, "the label is the model id");
    assert!(real > 0);
    // Within 2x of the shipped estimate in both directions. Further apart than
    // that and either the estimate or the load is wrong, and both are worth
    // knowing about.
    assert!(real * 2 > estimated && estimated * 2 > real, "{real} against {estimated}");
    assert!(
        load < std::time::Duration::from_secs(2),
        "a warm vocab-only load has to be cheap enough to sit in front of a probe, \
         and anything near a full model load is not: {load:?}"
    );
}
