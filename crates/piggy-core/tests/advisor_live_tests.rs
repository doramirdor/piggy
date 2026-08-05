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
