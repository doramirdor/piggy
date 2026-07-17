//! Independent statistical-correctness attack on M3 attribution.
//!
//! These tests are written from scratch (not derived from the builder's tests)
//! to probe the honesty guarantee from the outside:
//!
//!  * Monte-Carlo false-positive rate of the green `measured` badge under a true
//!    null (both groups from the same distribution).
//!  * Recovery / power at planted effects of 0%, 5%, 20%, 50%.
//!  * Confounded designs: session-length confound (should be neutralised by the
//!    per-turn normaliser), model/token-mix confound, heavy-tailed outliers, and
//!    the non-randomised pre-install pooling hazard.
//!  * Degenerate zero-variance data producing a zero-width CI.
//!
//! Everything runs against a real `Store` in a fresh `tempdir` (never touching
//! `~/.piggy`), matching the pattern of the builder's own tests.

use std::collections::BTreeMap;

use piggy_core::attribution::{self, Badge};
use piggy_core::rng::XorShift64;
use piggy_core::store::source;
use piggy_core::{ModelTokens, Pricing, SaverTag, SessionParse, Store};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn insert_session(
    store: &mut Store,
    pricing: &Pricing,
    id: &str,
    model: &str,
    turns: u64,
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
    subagent: bool,
) {
    let mut models = BTreeMap::new();
    models.insert(
        model.to_string(),
        ModelTokens {
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: cache_create,
            cache_creation_1h_tokens: 0,
            cache_read_tokens: cache_read,
        },
    );
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some("/proj".into()),
        git_branch: None,
        first_ts: Some("2026-01-01T00:00:00.000Z".into()),
        last_ts: Some("2026-01-01T00:10:00.000Z".into()),
        models,
        n_assistant_msgs: turns,
        n_user_msgs: turns,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        parse_errors: 0,
    };
    let path = if subagent {
        format!("/proj/subagents/{id}.jsonl")
    } else {
        format!("/proj/{id}.jsonl")
    };
    store.upsert_session(&parse, pricing, &path, 1, 1).unwrap();
}

/// Insert a session with an explicit `started_at`, for the tests that care which
/// era a session belongs to. The headline picks the live saver set by recency, so
/// these fixtures need real timestamps rather than the shared constant one.
fn insert_session_at(
    store: &mut Store,
    pricing: &Pricing,
    id: &str,
    turns: u64,
    output: u64,
    started_at: &str,
) {
    let mut models = BTreeMap::new();
    models.insert(
        "claude-sonnet-4-5".to_string(),
        ModelTokens {
            input_tokens: 400,
            output_tokens: output,
            cache_creation_tokens: 0,
            cache_creation_1h_tokens: 0,
            cache_read_tokens: 0,
        },
    );
    let parse = SessionParse {
        session_id: id.to_string(),
        source: "claude-code".to_string(),
        interface: "unknown".to_string(),
        client: None,
        project_path: Some("/proj".into()),
        git_branch: None,
        first_ts: Some(started_at.to_string()),
        last_ts: Some(started_at.to_string()),
        models,
        n_assistant_msgs: turns,
        n_user_msgs: turns,
        n_tool_results: 0,
        sidechain: ModelTokens::default(),
        tool_use_counts: BTreeMap::new(),
        parse_errors: 0,
    };
    store
        .upsert_session(&parse, pricing, &format!("/proj/{id}.jsonl"), 1, 1)
        .unwrap();
}

/// Uniform in `[0.0, 1.0)` from the deterministic PRNG (matches builder helper).
fn unit(rng: &mut XorShift64) -> f64 {
    rng.next_u64() as f64 / (u64::MAX as f64 + 1.0)
}

/// Symmetric multiplicative noise centred (in median) on 1.0, width `w`.
fn noise(rng: &mut XorShift64, w: f64) -> f64 {
    1.0 - w / 2.0 + unit(rng) * w
}

/// Build a fresh store of `n` ON + `n` OFF sessions for saver `rtk`, where the
/// ON output-per-turn base rate is `on_rate` and OFF is `off_rate`. Returns the
/// output-stream stat for `rtk`.
fn one_trial(
    on_rate: f64,
    off_rate: f64,
    n: usize,
    noise_w: f64,
    data_seed: u64,
    boot_seed: u64,
) -> attribution::StreamStat {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(data_seed);

    for i in 0..n {
        // OFF
        let turns = 5 + rng.below(20) as u64;
        let out = (off_rate * noise(&mut rng, noise_w) * turns as f64).round() as u64;
        let id = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();

        // ON
        let turns = 5 + rng.below(20) as u64;
        let out = (on_rate * noise(&mut rng, noise_w) * turns as f64).round() as u64;
        let id = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }

    let a = attribution::attribute(&store, &pricing, "rtk", boot_seed).unwrap();
    a.output().unwrap().clone()
}

// ---------------------------------------------------------------------------
// 1. Null false-positive rate of the green badge (per-stream).
// ---------------------------------------------------------------------------
//
// Both groups are drawn from the SAME distribution. A 90% CI gate should reject
// the null (badge measured) roughly 10% of the time. We assert the empirical
// rate is not grossly anti-conservative (a broken, too-narrow CI would fire far
// more often), and print the exact number as evidence.

#[test]
fn null_per_stream_false_positive_rate() {
    let trials = 200usize;
    let mut fp = 0usize;
    let mut sign_pos = 0usize;
    for t in 0..trials {
        let out = one_trial(
            1000.0,
            1000.0,
            14,
            0.8,
            0x51A7_0000 ^ (t as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            0xB007_0000 ^ (t as u64).wrapping_mul(0x2545_F491_4F6C_DD1D),
        );
        if out.badge == Badge::Measured {
            fp += 1;
            if out.delta.unwrap() > 0.0 {
                sign_pos += 1;
            }
        }
    }
    let rate = fp as f64 / trials as f64;
    eprintln!(
        "[null_per_stream_false_positive_rate] measured badges on TRUE NULL: {fp}/{trials} = {rate:.3} \
         (of which {sign_pos} claimed positive savings)"
    );
    // Nominal for a two-sided 90% CI is ~0.10. Fail only on gross anti-conservatism.
    assert!(
        rate < 0.22,
        "false-positive rate {rate:.3} is far above the ~0.10 nominal of a 90% CI — the \
         bootstrap CI is too narrow / anti-conservative"
    );
}

// ---------------------------------------------------------------------------
// 2. Per-SAVER honesty: chance that a truly-null saver shows >=1 green stream.
// ---------------------------------------------------------------------------
//
// A saver has FOUR stream badges. Even if each stream's null FP rate is ~10%,
// the chance that AT LEAST ONE of the four badges lights up green is ~1-0.9^4
// ~= 0.34. This surfaces the honesty-relevant number: how often a do-nothing
// saver displays some green "measured savings".

#[test]
fn null_saver_any_stream_false_positive_rate() {
    let trials = 160usize;
    let mut any_green = 0usize;
    for t in 0..trials {
        let home = tempfile::tempdir().unwrap();
        let pricing = Pricing::embedded();
        let mut store = Store::open(home.path()).unwrap();
        let mut rng = XorShift64::new(0xA11_0000 ^ (t as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        // All four streams truly null: same distribution for ON and OFF.
        for i in 0..14 {
            for (enabled, tag) in [(false, "off"), (true, "on")] {
                let turns = 5 + rng.below(20) as u64;
                let inp = (500.0 * noise(&mut rng, 0.8) * turns as f64).round() as u64;
                let out = (1000.0 * noise(&mut rng, 0.8) * turns as f64).round() as u64;
                let cc = (300.0 * noise(&mut rng, 0.8) * turns as f64).round() as u64;
                let cr = (2000.0 * noise(&mut rng, 0.8) * turns as f64).round() as u64;
                let id = format!("{tag}-{i}");
                insert_session(
                    &mut store,
                    &pricing,
                    &id,
                    "claude-sonnet-4-5",
                    turns,
                    inp,
                    out,
                    cc,
                    cr,
                    false,
                );
                store
                    .set_session_savers(&id, &[SaverTag::new("rtk", enabled, source::ROTATION)])
                    .unwrap();
            }
        }
        let a = attribution::attribute(&store, &pricing, "rtk", 0xF00D ^ t as u64).unwrap();
        if a.streams.iter().any(|s| s.badge == Badge::Measured) {
            any_green += 1;
        }
    }
    let rate = any_green as f64 / trials as f64;
    eprintln!(
        "[null_saver_any_stream_false_positive_rate] truly-null savers showing >=1 GREEN stream: \
         {any_green}/{trials} = {rate:.3}  (expected ~1-0.9^4 = 0.34 for independent 90% CIs)"
    );
    // Only a sanity ceiling; the point is the printed number.
    assert!(
        rate < 0.60,
        "a do-nothing saver lights up a green badge {rate:.3} of the time — investigate"
    );
}

// ---------------------------------------------------------------------------
// 3. Recovery / power at planted effects of 0%, 5%, 20%, 50%.
// ---------------------------------------------------------------------------

#[test]
fn recovery_across_planted_effects() {
    let trials = 60usize;
    let n = 40usize; // comfortably above MIN_GROUP so power is real
    for &true_effect in &[0.0f64, 0.05, 0.20, 0.50] {
        let off_rate = 1000.0;
        let on_rate = off_rate * (1.0 - true_effect);
        let mut measured = 0usize;
        let mut ci_covers = 0usize;
        let mut delta_sum = 0.0f64;
        let mut delta_n = 0usize;
        for t in 0..trials {
            let out = one_trial(
                on_rate,
                off_rate,
                n,
                0.6,
                0xE_0000 ^ ((true_effect * 1000.0) as u64) ^ (t as u64).wrapping_mul(0x9E37_79B9),
                0xC_0000 ^ (t as u64).wrapping_mul(0x2545_F491),
            );
            if let Some(d) = out.delta {
                delta_sum += d;
                delta_n += 1;
            }
            if let Some((lo, hi)) = out.ci {
                if lo <= true_effect && true_effect <= hi {
                    ci_covers += 1;
                }
            }
            if out.badge == Badge::Measured {
                measured += 1;
            }
        }
        let mean_delta = delta_sum / delta_n.max(1) as f64;
        let power = measured as f64 / trials as f64;
        let coverage = ci_covers as f64 / trials as f64;
        eprintln!(
            "[recovery] true={:.2}  mean_delta={:.3}  measured={}/{} (power {:.2})  \
             CI_covers_truth={}/{} ({:.2})",
            true_effect, mean_delta, measured, trials, power, ci_covers, trials, coverage
        );

        if true_effect == 0.0 {
            // Null: power == false-positive rate; must be low-ish.
            assert!(
                power < 0.25,
                "null effect badged measured {power:.2} of the time (too high)"
            );
            assert!(
                mean_delta.abs() < 0.05,
                "null mean delta {mean_delta:.3} should sit near zero"
            );
        } else {
            // Point estimate should track the truth (median-based, so low bias).
            assert!(
                (mean_delta - true_effect).abs() < 0.06,
                "mean recovered delta {mean_delta:.3} strays from planted {true_effect:.2}"
            );
        }
        if true_effect >= 0.20 {
            // A large effect at n=40/side should almost always be detected, and
            // a 90% CI should cover the truth the large majority of the time.
            assert!(
                power > 0.80,
                "power {power:.2} at true effect {true_effect:.2} is low — under-powered"
            );
            assert!(
                coverage > 0.75,
                "90% CI covered the truth only {coverage:.2} of trials at effect {true_effect:.2}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Session-length confound: per-turn normalisation MUST neutralise it.
// ---------------------------------------------------------------------------
//
// ON sessions are systematically far longer (many more turns) than OFF, but the
// per-turn output distribution is identical. A correct normaliser recovers ~0
// and never badges measured off the length difference alone.

#[test]
fn session_length_confound_is_neutralised() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x1EE7_1EE7_1EE7_1EE7);
    for i in 0..30 {
        // OFF: short sessions (5-9 turns).
        let turns = 5 + rng.below(5) as u64;
        let out = (1000.0 * noise(&mut rng, 0.6) * turns as f64).round() as u64;
        let id = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
        // ON: long sessions (60-99 turns), SAME per-turn rate.
        let turns = 60 + rng.below(40) as u64;
        let out = (1000.0 * noise(&mut rng, 0.6) * turns as f64).round() as u64;
        let id = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }
    let a = attribution::attribute(&store, &pricing, "rtk", 0x5EED).unwrap();
    let out = a.output().unwrap();
    eprintln!(
        "[session_length_confound] delta={:?} badge={:?} median_on={:.1} median_off={:.1}",
        out.delta, out.badge, out.median_on, out.median_off
    );
    assert!(
        out.delta.unwrap().abs() < 0.10,
        "per-turn normalisation failed to neutralise a pure session-length confound: delta={:?}",
        out.delta
    );
}

// ---------------------------------------------------------------------------
// 5. Heavy-tailed outliers must not corrupt the median-based delta.
// ---------------------------------------------------------------------------

#[test]
fn heavy_tailed_outliers_do_not_corrupt_delta() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x0A71_1E50);
    let n = 30usize;
    for i in 0..n {
        // OFF ~1000/turn, ON ~800/turn (true 20%).
        let turns = 8 + rng.below(6) as u64;
        let mut off = 1000.0 * noise(&mut rng, 0.4);
        let mut on = 800.0 * noise(&mut rng, 0.4);
        // Inject a few monster outliers (10-20x) into BOTH groups.
        if i % 7 == 0 {
            off *= 15.0;
        }
        if i % 9 == 0 {
            on *= 18.0;
        }
        let off_tok = (off * turns as f64).round() as u64;
        let on_tok = (on * turns as f64).round() as u64;
        let ido = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &ido,
            "claude-sonnet-4-5",
            turns,
            400,
            off_tok,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&ido, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
        let idn = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &idn,
            "claude-sonnet-4-5",
            turns,
            400,
            on_tok,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&idn, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }
    let a = attribution::attribute(&store, &pricing, "rtk", 0xBEEF).unwrap();
    let out = a.output().unwrap();
    eprintln!(
        "[heavy_tailed] delta={:?} ci={:?} badge={:?}",
        out.delta, out.ci, out.badge
    );
    // Median is robust: the recovered delta should still sit near the true 0.20
    // despite ~15-18x outliers in both arms.
    let d = out.delta.unwrap();
    assert!(
        (d - 0.20).abs() < 0.10,
        "outliers corrupted the median-based delta: {d:.3} (expected ~0.20)"
    );
}

// ---------------------------------------------------------------------------
// 6. Model/token-rate mix confound — per-turn normalisation does NOT adjust
//    for composition differences between the two arms.
// ---------------------------------------------------------------------------
//
// Within every session the saver does literally nothing. But the ON arm is
// dominated by low-token-rate sessions and the OFF arm by high-token-rate ones
// (as would happen if assignment were correlated with task/model type). The
// per-turn normaliser compares raw token/turn rates and has no way to know the
// composition differs, so it reports a spurious "measured" saving.

#[test]
fn composition_confound_yields_false_measured_badge() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00C0_FFEE);
    // ON arm: 20 "light" sessions (500/turn). OFF arm: 20 "heavy" sessions
    // (1500/turn). The saver has zero within-cell effect — the gap is pure
    // composition confound.
    for i in 0..20 {
        let turns = 8 + rng.below(6) as u64;
        let on = (500.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let idn = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &idn,
            "claude-sonnet-4-5",
            turns,
            400,
            on,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&idn, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
        let turns = 8 + rng.below(6) as u64;
        let off = (1500.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let ido = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &ido,
            "claude-sonnet-4-5",
            turns,
            400,
            off,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&ido, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
    }
    let a = attribution::attribute(&store, &pricing, "rtk", 0x1357).unwrap();
    let out = a.output().unwrap();
    eprintln!(
        "[composition_confound] delta={:?} ci={:?} badge={:?}",
        out.delta, out.ci, out.badge
    );
    // This documents the (expected, but honesty-relevant) blind spot: a pure
    // composition confound is reported as a large green "measured" saving.
    assert_eq!(
        out.badge,
        Badge::Measured,
        "composition confound was expected to (mis)fire as measured"
    );
    assert!(
        out.delta.unwrap() > 0.4,
        "expected a large spurious saving from the composition confound"
    );
}

// ---------------------------------------------------------------------------
// 7. Non-randomised pre-install pooling must NOT manufacture a measured badge.
// ---------------------------------------------------------------------------
//
// A saver that is TRULY NULL under randomised rotation (ON and OFF drawn from
// the same distribution) plus a heavier-usage pre-install era (all-off,
// observational) must not be pushed to a green "measured" badge by that
// non-randomised drift. `attribute()` keeps the pre_install rows out of the
// measured comparison: with ≥ MIN_GROUP randomised OFF sessions it measures off
// those alone, so the true null correctly stays `measuring`. (The earlier build
// pooled all disabled sessions and dishonestly fired `Measured` here.)

#[test]
fn pre_install_pooling_does_not_manufacture_a_measured_badge() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x009A_551E);

    // Randomised rotation arm: ON and OFF are the SAME distribution (true null).
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (1000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("rot-off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();

        let turns = 8 + rng.below(6) as u64;
        let out = (1000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("rot-on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }

    // Non-randomised pre-install history: heavier usage era (2000/turn), all-off.
    // This is observational — nothing about it is attributable to the saver.
    for i in 0..30 {
        let turns = 8 + rng.below(6) as u64;
        let out = (2000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("pre-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            turns,
            400,
            out,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::PRE_INSTALL)])
            .unwrap();
    }

    let a = attribution::attribute(&store, &pricing, "rtk", 0x2468).unwrap();
    let out = a.output().unwrap();
    eprintln!(
        "[pre_install_pooling] n_on={} n_off={} off_by_source={:?} delta={:?} ci={:?} badge={:?}",
        a.n_on, a.n_off, a.off_by_source, out.delta, out.ci, out.badge
    );

    // The 30 pre_install rows are counted for the footnote but kept OUT of the
    // measured comparison: only the 15 randomised rotation-OFF sessions back it.
    assert_eq!(a.off_by_source.get("pre_install").copied(), Some(30));
    assert_eq!(a.off_by_source.get("rotation").copied(), Some(15));
    assert_eq!(
        out.n_off, 15,
        "pre_install rows must not enter the measured OFF vector (only the 15 randomised OFF)"
    );

    // The saver does nothing under randomisation, so it must NOT badge measured;
    // the pooled pre_install drift can no longer manufacture a green badge.
    assert_ne!(
        out.badge,
        Badge::Measured,
        "non-randomised pre_install drift must not produce a measured badge"
    );
    assert_eq!(
        out.badge,
        Badge::Measuring,
        "a true null under randomisation stays measuring"
    );
    let (lo, hi) = out.ci.unwrap();
    assert!(
        lo <= 0.0 && hi >= 0.0,
        "the true-null randomised CI [{lo:.3}, {hi:.3}] should straddle zero"
    );
}

// ---------------------------------------------------------------------------
// 8. Degenerate zero-variance data => zero-width CI must NOT be badged.
// ---------------------------------------------------------------------------
//
// If every session in each arm has an identical per-turn rate, every bootstrap
// resample yields the same medians, so the CI collapses to a single point. A
// zero-width CI is infinite false precision, not evidence: the gate now requires
// positive CI width, so this degenerate case stays `measuring` rather than
// showing an overconfident green badge. (The earlier build fired `Measured`.)

#[test]
fn zero_variance_ci_is_not_badged_measured() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    // 12 per side, ALL identical: turns=10, OFF out=10000 (1000/turn),
    // ON out=8000 (800/turn). No variance anywhere.
    for i in 0..12 {
        let id = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            400,
            10_000,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
        let id = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            400,
            8_000,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }
    let a = attribution::attribute(&store, &pricing, "rtk", 0x777).unwrap();
    let out = a.output().unwrap();
    let (lo, hi) = out.ci.unwrap();
    eprintln!(
        "[zero_variance] delta={:?} ci=({lo:.4},{hi:.4}) width={:.6} badge={:?}",
        out.delta,
        hi - lo,
        out.badge
    );
    // The CI is genuinely degenerate (zero width)...
    assert!(
        (hi - lo).abs() < 1e-9,
        "expected a degenerate zero-width CI, got width {:.6}",
        hi - lo
    );
    // ...so the gate must refuse to badge it: no width means no evidence.
    assert_eq!(
        out.badge,
        Badge::Measuring,
        "a zero-width CI is infinite false precision and must not badge measured"
    );
}

// ---------------------------------------------------------------------------
// 9. Subagent exclusion actually removes rows from the groups (not just rates).
// ---------------------------------------------------------------------------

#[test]
fn subagent_rows_never_enter_attribution_groups() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    // 12 legit ON + 12 legit OFF, all at 1000/turn (true null).
    for i in 0..12 {
        let id = format!("off-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            400,
            10_000,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
        let id = format!("on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            400,
            10_000,
            0,
            0,
            false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }
    // Add 20 subagent ON sessions with a wildly different rate. If they leaked
    // into the ON group they would swing the delta massively.
    for i in 0..20 {
        let id = format!("sub-on-{i}");
        insert_session(
            &mut store,
            &pricing,
            &id,
            "claude-sonnet-4-5",
            10,
            400,
            100,
            0,
            0,
            true,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }
    let a = attribution::attribute(&store, &pricing, "rtk", 0x5A).unwrap();
    let out = a.output().unwrap();
    eprintln!(
        "[subagent_exclusion] n_on={} n_off={} delta={:?}",
        a.n_on, a.n_off, out.delta
    );
    assert_eq!(out.n_on, 12, "subagent ON rows must not enter the ON group");
    assert_eq!(out.n_off, 12);
    // Because subagents are excluded, ON==OFF==1000/turn → delta ~0.
    assert!(out.delta.unwrap().abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// 10. A manual ON era must NOT be measured against an older randomised OFF era.
// ---------------------------------------------------------------------------
//
// The mirror of test 7, on the other side of the comparison. The OFF group has
// always been split by randomisation; the ON group used to take every enabled
// row whatever its source, so this slipped through:
//
//   1. the user manually turns a saver on;
//   2. `rotation::controlled_savers` drops it from rotation for good, because a
//      manual toggle pins it;
//   3. every later session tags it (enabled, source=manual);
//   4. the older rotation/holdout OFF rows still sit in `off_randomized`.
//
// With >= MIN_GROUP randomised OFF rows the ceiling was `Measured`, so "recent
// manual-on era vs older randomised-off era" rendered as a green measured badge.
// Here the saver does NOTHING within a cell; the entire gap is era drift (the
// user's later work is simply lighter). A measured badge would be a lie.

#[test]
fn a_manual_on_era_is_not_measured_against_an_older_randomised_off_era() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00C0_FFEE);

    // Older randomised OFF era: 15 rotation-off sessions at ~2000 tokens/turn.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (2000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("rot-off-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
    }

    // Later manual ON era: the user pinned the saver on, and their work happens
    // to be lighter now (~1000/turn). The saver itself did nothing: the 50% gap
    // is pure era drift, exactly what randomisation exists to rule out.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (1000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("manual-on-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::MANUAL)])
            .unwrap();
    }

    let a = attribution::attribute(&store, &pricing, "rtk", 0x1357).unwrap();
    let out = a.output().unwrap();
    eprintln!(
        "[manual_on_era] n_on={} n_off={} delta={:?} ci={:?} badge={:?}",
        out.n_on, out.n_off, out.delta, out.ci, out.badge
    );

    // The comparison is not randomised on the ON side, so whatever figure comes
    // out, it cannot be badged measured.
    assert_ne!(
        out.badge,
        Badge::Measured,
        "a manual-on era vs an older randomised-off era is observational: \
         the ~50% gap is era drift, not the saver, and must never badge measured"
    );
    // Both halves of the fix, not just the downgrade. `assert_ne!` alone also
    // passes when the ON group collapses to empty and the percentage silently
    // disappears (n_on=0 -> Measuring), which would be a different regression
    // wearing the same green-badge-is-gone disguise. The point is that Piggy
    // still SHOWS the number, honestly labelled.
    assert_eq!(
        out.badge,
        Badge::Estimated,
        "the figure is still worth showing, it just isn't measured"
    );
    assert_eq!(
        out.n_on, 15,
        "the manual ON rows are pooled (not discarded) once they are the only ON \
         evidence there is"
    );
    assert!(
        out.delta.is_some(),
        "an estimated badge still carries a point estimate"
    );
}

// ---------------------------------------------------------------------------
// 11. The HEADLINE must not measure a manual-on era against a holdout era.
// ---------------------------------------------------------------------------
//
// The session-level mirror of test 10, on the number users actually read:
// "Your Claude plan lasts N.N× longer, measured against N holdout sessions".
//
// `classified_sessions` used to decide FullOn purely on "every row enabled",
// never consulting the row's source, while the baseline side WAS source-split.
// So a session where every saver is (enabled, manual) counted as a randomized
// full-on session, and the headline compared a manual-on era against an older
// randomized holdout era and badged it measured. Same confound as test 10, one
// layer up, and far more visible.

#[test]
fn the_headline_does_not_measure_a_manual_on_era_against_a_holdout_era() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00BA_DBED);

    // Older randomised holdout era: everything off, ~2000 tokens/turn.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (2000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("holdout-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::HOLDOUT)])
            .unwrap();
    }

    // Later manual-on era: the user pinned the saver on, and their work is
    // lighter now (~1000/turn). The saver did nothing; the gap is era drift.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (1000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("manual-full-on-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::MANUAL)])
            .unwrap();
    }

    let hl = attribution::headline(&store, &pricing, 0x1357).unwrap();
    eprintln!(
        "[headline_manual_on] baseline={:?} n_full_on={} n_baseline={} on_randomized={} mult={:?}",
        hl.baseline, hl.n_full_on, hl.n_baseline, hl.on_randomized, hl.multiplier
    );

    // The multiplier still computes (~2x) and is still worth showing. What it
    // must not do is call itself measured.
    assert!(
        !hl.on_randomized,
        "a full-on group made of manually-pinned sessions is not randomized"
    );
    for s in &hl.streams {
        assert_ne!(
            s.badge,
            Badge::Measured,
            "stream {:?}: a manual-on era vs a holdout era is observational drift, \
             not a measured saving",
            s.stream
        );
    }
}

// ---------------------------------------------------------------------------
// 12. A holdout with a pinned saver running through it is not a no-savers
//     baseline, so the headline must not call itself measured (#3).
// ---------------------------------------------------------------------------
//
// `rotation::controlled_savers` drops manually-toggled savers on purpose, so a
// saver the user pinned ON never gets turned off by the holdout slot: it rides
// straight through. `classified_sessions` used to file a session as Holdout on
// the presence of ANY holdout row, so those sessions backed a green headline
// while the "every saver off" baseline still had a saver running.
//
// The headline's claim is "your plan lasts N.N x longer" against no savers at
// all. That counterfactual was never observed here, so the figure is a
// projection: show it, label it estimated.
//
// The full-on era below is deliberately CLEAN (headroom has been uninstalled by
// then, so every remaining tag is rotation-sourced). That isolates this bug from
// the ON-side one: `on_randomized` is true here, so the only thing that can stop
// a green headline is noticing the holdout itself was dirty. Had headroom stayed
// pinned through the full-on era, the ON-side quarantine would have caught it
// first and this test would pass without exercising the fix at all.

#[test]
fn a_holdout_with_a_pinned_saver_cannot_back_a_measured_headline() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00D1_5EA5);

    // "Holdout" era: rtk is rotated off, but headroom is pinned on by the user
    // and rotation never touches it. Not an all-off baseline.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (2000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("dirty-holdout-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", false, source::HOLDOUT),
                    SaverTag::new("headroom", true, source::MANUAL),
                ],
            )
            .unwrap();
    }

    // Full-on era, after headroom was uninstalled: only rtk is tagged, and by
    // rotation, so this side is genuinely randomized.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (1000.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("full-on-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }

    let hl = attribution::headline(&store, &pricing, 0x2468).unwrap();
    eprintln!(
        "[dirty_holdout] baseline={:?} n_baseline={} baseline_clean={} ceiling={:?} mult={:?}",
        hl.baseline, hl.n_baseline, hl.baseline_clean, hl.ceiling, hl.multiplier
    );

    // The sessions still count: they are real evidence and the number still shows.
    assert_eq!(
        hl.n_baseline, 15,
        "the contaminated holdouts are kept as a baseline, not thrown away"
    );
    assert!(hl.multiplier.is_some(), "the figure is still worth showing");
    // The ON side is clean, so this is the holdout's contamination doing the work
    // and not the ON-side quarantine standing in for it.
    assert!(
        hl.on_randomized,
        "the full-on era is rotation-only, so the ON side is randomized: whatever \
         downgrades this headline has to be the dirty holdout"
    );
    // What the dirty holdout cannot do is back a measured claim.
    assert!(
        !hl.baseline_clean,
        "a holdout with headroom pinned on is not a no-savers baseline"
    );
    assert_eq!(
        hl.ceiling,
        Badge::Estimated,
        "the no-savers counterfactual was never observed, so the headline is a projection"
    );
}

// ---------------------------------------------------------------------------
// 13. An empty ON group must not compute a 100% saving (#4).
// ---------------------------------------------------------------------------
//
// `median(&[])` is 0.0, so `1 - median(on)/median(off)` on an empty ON group
// used to yield Some(1.0): a nominal 100% saving out of no data. Only the
// MIN_GROUP gate kept it off the screen. A saver that has NEVER been on is the
// realistic way to reach an empty ON group.

#[test]
fn a_saver_never_seen_on_has_no_delta_rather_than_a_100_percent_one() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00FA_CADE);

    // 20 sessions, rtk off in every one. No ON sessions at all.
    for i in 0..20 {
        let turns = 8 + rng.below(6) as u64;
        let out = (1500.0 * noise(&mut rng, 0.5) * turns as f64).round() as u64;
        let id = format!("off-only-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::ROTATION)])
            .unwrap();
    }

    let a = attribution::attribute(&store, &pricing, "rtk", 0x1111).unwrap();
    let out = a.output().unwrap();
    eprintln!(
        "[never_on] n_on={} n_off={} delta={:?} badge={:?}",
        out.n_on, out.n_off, out.delta, out.badge
    );

    assert_eq!(out.n_on, 0);
    assert_eq!(
        out.delta, None,
        "a saver never seen on saved nothing measurable; 1 - median(&[])/median(off) == 1.0 \
         is a 100% saving conjured from no data"
    );
    assert_eq!(out.badge, Badge::Measuring);
}

// ---------------------------------------------------------------------------
// 14. Clean and contaminated holdouts must never be pooled into one baseline.
// ---------------------------------------------------------------------------
//
// The first cut of the #3 fix reused `pick_group` for the baseline, which pools
// its two groups when the preferred one is thin. That is sound for groups that
// differ only in PROVENANCE, but a clean holdout ("every saver off") and a
// contaminated one ("every saver off except the pinned one, still running") are
// different treatment arms. The median of their union tracks whichever arm is
// bigger, so the headline moves with the mix instead of with the savers.
//
// Here the clean holdouts (the truth, 2000/turn) are outnumbered by heavier
// contaminated ones (3000/turn). Pooling would drag the baseline median up and
// INFLATE the multiplier well above the clean-only truth, in the direction of
// Piggy's own product claim. Worse, pooling would clear MIN_GROUP on a count
// (9 + 15) that no single population supports.

#[test]
fn clean_and_contaminated_holdouts_are_not_pooled_into_one_baseline() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00C1_EA11);

    // 9 clean holdouts: genuinely everything off. Below MIN_GROUP on their own.
    for i in 0..9 {
        let turns = 8 + rng.below(6) as u64;
        let out = (2000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("clean-holdout-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::HOLDOUT)])
            .unwrap();
    }

    // 15 contaminated holdouts from a heavier era, headroom pinned on throughout.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (3000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("dirty-holdout-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", false, source::HOLDOUT),
                    SaverTag::new("headroom", true, source::MANUAL),
                ],
            )
            .unwrap();
    }

    // Full-on era, rotation-only.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (1000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("full-on-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }

    let hl = attribution::headline(&store, &pricing, 0x3691).unwrap();
    eprintln!(
        "[no_pooling] n_baseline={} baseline_clean={} ceiling={:?} mult={:?}",
        hl.n_baseline, hl.baseline_clean, hl.ceiling, hl.multiplier
    );

    // 9 clean is under MIN_GROUP, so the contaminated arm is used ALONE. The
    // giveaway for pooling is a baseline of 24: a count no single population has.
    assert_eq!(
        hl.n_baseline, 15,
        "the baseline must be one coherent population (the 15 contaminated), never \
         the 24-session union of two different treatment arms"
    );
    assert!(!hl.baseline_clean);
    assert_eq!(hl.ceiling, Badge::Estimated);
}

// ---------------------------------------------------------------------------
// 15. Switching one saver off by hand must not kill the headline (#5).
// ---------------------------------------------------------------------------
//
// `controlled_savers` pins a hand-toggled saver out of rotation, so it is tagged
// (disabled, manual) in EVERY later session. `classified_sessions` tested "no
// saver is off at all" for full-on, so every one of those sessions classified
// Mixed, n_full_on stayed 0, and the headline read "measuring" forever at any
// session count, with no hint that a switch flipped months ago was the cause.
//
// Full-on has to mean "every saver the scheduler is running is on". A saver the
// user switched off is not one of those: it is off in the holdout too, so it
// leaves the contrast rather than poisoning it, and what remains ("everything
// else on" vs "nothing on") is exactly the setup the user is actually running.
// Provenance still caps it at estimated, since Piggy is not rotating that saver.

#[test]
fn a_hand_switched_off_saver_does_not_kill_the_headline() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00DE_AD11);

    // The user switched caveman off by hand. It is off in every session below,
    // holdout and full-on alike, so it is a constant and not a confound.
    for i in 0..15 {
        // Holdout: everything the scheduler runs is off, and caveman is off too,
        // so this really is an all-off baseline.
        let turns = 8 + rng.below(6) as u64;
        let out = (2000.0 * noise(&mut rng, 0.4) * turns as f64).round() as u64;
        let id = format!("holdout-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", false, source::HOLDOUT),
                    SaverTag::new("caveman", false, source::MANUAL),
                ],
            )
            .unwrap();

        // Full-on for this user: rtk on by rotation, caveman still hand-off.
        let turns = 8 + rng.below(6) as u64;
        let out = (1200.0 * noise(&mut rng, 0.4) * turns as f64).round() as u64;
        let id = format!("full-on-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", true, source::ROTATION),
                    SaverTag::new("caveman", false, source::MANUAL),
                ],
            )
            .unwrap();
    }

    let hl = attribution::headline(&store, &pricing, 0x8642).unwrap();
    eprintln!(
        "[hand_off] n_full_on={} n_baseline={} baseline_clean={} ceiling={:?} mult={:?}",
        hl.n_full_on, hl.n_baseline, hl.baseline_clean, hl.ceiling, hl.multiplier
    );

    // The whole bug: this used to be 0 forever.
    assert_eq!(
        hl.n_full_on, 15,
        "sessions where every scheduler-run saver is on are full-on, even though the \
         user has one switched off by hand"
    );
    assert!(
        hl.multiplier.is_some(),
        "the headline still has a number: 'everything else on' vs 'nothing on' is exactly \
         the setup this user runs"
    );
    // The holdout genuinely was all-off here, so the baseline is clean...
    assert!(hl.baseline_clean);
    // ...but a hand-set saver is still not something Piggy randomized.
    assert!(!hl.on_randomized);
    assert_eq!(hl.ceiling, Badge::Estimated);
}

// A real single-off rotation slot must STILL be excluded from full-on: the guard
// above narrows "any saver off" to "the scheduler turned a saver off", and that
// is exactly what a single-off slot is.
#[test]
fn a_scheduler_single_off_slot_is_still_not_full_on() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x0051_5A11);

    for i in 0..12 {
        let turns = 8 + rng.below(6) as u64;
        let out = (1500.0 * noise(&mut rng, 0.4) * turns as f64).round() as u64;
        let id = format!("single-off-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        // rtk rotated OFF for its single-off slot, headroom on: not full-on.
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", false, source::ROTATION),
                    SaverTag::new("headroom", true, source::ROTATION),
                ],
            )
            .unwrap();
    }

    let hl = attribution::headline(&store, &pricing, 0x9753).unwrap();
    assert_eq!(
        hl.n_full_on, 0,
        "a single-off rotation slot is Mixed, not full-on: the scheduler turned that \
         saver off on purpose"
    );
}

// ---------------------------------------------------------------------------
// 16. Every saver switched off by hand means NO headline, not a free multiplier.
// ---------------------------------------------------------------------------
//
// The boundary where #5's own reasoning collapses. "Full-on means every saver
// the scheduler runs is on" is fine while there IS an everything-else. Switch
// every saver off by hand and there are no scheduler-disabled rows at all, so
// `!any_scheduler_disabled` is vacuously true and a session running NOTHING
// would classify as full-on. The headline would then publish a multiplier off
// pure pre/post-install drift to a user with every saver off.
//
// Reachable without rotation ever running: `controlled_savers` returns empty
// once every saver is manual, so `tick` reports NothingToRotate and no rotation
// or holdout row is ever written.

#[test]
fn every_saver_switched_off_by_hand_publishes_no_headline() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00E0_FF11);

    // Pre-Piggy history, heavier era.
    for i in 0..12 {
        let turns = 8 + rng.below(6) as u64;
        let out = (300.0 * noise(&mut rng, 0.4) * turns as f64).round() as u64;
        let id = format!("pre-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::PRE_INSTALL)])
            .unwrap();
    }

    // Piggy installed, then every saver switched off by hand. Lighter era, but
    // no saver is running in EITHER era, so the whole gap is drift.
    for i in 0..12 {
        let turns = 8 + rng.below(6) as u64;
        let out = (150.0 * noise(&mut rng, 0.4) * turns as f64).round() as u64;
        let id = format!("all-off-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", false, source::MANUAL),
                    SaverTag::new("caveman", false, source::MANUAL),
                ],
            )
            .unwrap();
    }

    let hl = attribution::headline(&store, &pricing, 0x1234).unwrap();
    eprintln!(
        "[all_hand_off] n_full_on={} n_baseline={} mult={:?}",
        hl.n_full_on, hl.n_baseline, hl.multiplier
    );

    assert_eq!(
        hl.n_full_on, 0,
        "a session with every saver switched off is not a full-on session, however \
         vacuously true 'no scheduler-disabled saver' is"
    );
    assert_eq!(
        hl.multiplier, None,
        "no saver was running, so there is no savings multiplier to publish: the drift \
         between the eras is not a saving"
    );
}

// ---------------------------------------------------------------------------
// 17. The headline describes the saver set you run NOW, not an average of every
//     set you have ever run (#6).
// ---------------------------------------------------------------------------
//
// A session records no saver set of its own, so the ON group used to pool every
// era the setup had ever been in. Install a saver, uninstall one, hand-toggle
// one, and "everything on" quietly means something different on either side of
// that moment. The pooled median then tracked the era MIX rather than the
// savers: with saver behaviour held constant and only the ratio of old-era to
// new-era sessions varying, the multiplier swung from 3.84x to 1.93x, and at a
// 50/50 mix printed a figure describing no setup the user had ever run.
//
// Uses uninstall churn (no manual tags anywhere) on purpose: that is the form
// that used to be badged MEASURED, so it is the strongest version of the bug.

/// Build a store where `n_old` full-on sessions ran rtk+caveman and `n_new` ran
/// rtk alone (caveman uninstalled), against clean holdouts. Returns the headline.
fn churn_store(n_old: usize, n_new: usize) -> attribution::Headline {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    // A separate stream per era on purpose: with one shared RNG, changing n_old
    // would shift the draws for the NEW-era sessions too, and the sweep below
    // would measure the fixture instead of the code.
    let mut rng = XorShift64::new(0x00CB_11A5);

    // Baseline: nothing on. Same treatment in either era.
    for i in 0..15 {
        let turns = 8 + rng.below(6) as u64;
        let out = (2000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("holdout-{i}");
        insert_session_at(
            &mut store, &pricing, &id, turns, out, "2026-01-01T00:00:00.000Z",
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::HOLDOUT)])
            .unwrap();
    }
    // OLD era: rtk + caveman, both scheduler-run. Heavier savings (500/turn).
    let mut rng = XorShift64::new(0x00DA_7A01);
    for i in 0..n_old {
        let turns = 8 + rng.below(6) as u64;
        let out = (500.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("old-{i}");
        insert_session_at(
            &mut store, &pricing, &id, turns, out, "2026-02-01T00:00:00.000Z",
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", true, source::ROTATION),
                    SaverTag::new("caveman", true, source::ROTATION),
                ],
            )
            .unwrap();
    }
    // NEW era: caveman uninstalled, so it has no row at all. rtk alone
    // (1000/turn). This is the setup the user actually runs.
    let mut rng = XorShift64::new(0x00DA_7A02);
    for i in 0..n_new {
        let turns = 8 + rng.below(6) as u64;
        let out = (1000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("new-{i}");
        insert_session_at(
            &mut store, &pricing, &id, turns, out, "2026-03-01T00:00:00.000Z",
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }
    attribution::headline(&store, &pricing, 0x4242).unwrap()
}

#[test]
fn the_headline_tracks_the_live_saver_set_not_the_era_mix() {
    // Ground truth for the live setup (rtk alone): the 0-old case, where no other
    // era exists to blend in.
    let truth = churn_store(0, 20).multiplier.unwrap();

    // Now hold saver behaviour fixed and vary ONLY how many old-era sessions sit
    // in the DB alongside. The live-set answer must not move.
    for n_old in [0usize, 4, 10, 16, 20] {
        let hl = churn_store(n_old, 20);
        let mult = hl.multiplier.unwrap();
        eprintln!(
            "[churn] old={n_old:>2} new=20 -> n_full_on={} mult={mult:.4} (truth {truth:.4})",
            hl.n_full_on
        );
        assert_eq!(
            hl.n_full_on, 20,
            "only the 20 live-set sessions belong in the ON group; the {n_old} sessions \
             from the abandoned rtk+caveman setup are a different treatment"
        );
        assert!(
            (mult - truth).abs() < 1e-9,
            "the headline moved from {truth:.4} to {mult:.4} purely because {n_old} \
             sessions from an old setup exist: it is tracking the era mix, not the savers"
        );
    }
}

// ---------------------------------------------------------------------------
// 18. A stale era must not win the headline and be badged measured (#7).
// ---------------------------------------------------------------------------
//
// `pick_group` prefers the randomized group whenever it alone clears MIN_GROUP.
// With >= 10 sessions from an OLD randomized era, that branch won outright and
// every session describing the user's live setup was discarded: the dashboard
// made its strongest claim, MEASURED with no note and no hedge, about a
// configuration the user had abandoned. Recency has to beat seniority here.

#[test]
fn a_stale_randomized_era_does_not_win_the_headline() {
    // 12 old-era sessions is enough to clear MIN_GROUP on its own, which is
    // exactly what used to let it take over.
    let hl = churn_store(12, 20);
    eprintln!(
        "[stale] n_full_on={} ceiling={:?} mult={:?}",
        hl.n_full_on, hl.ceiling, hl.multiplier
    );
    assert_eq!(
        hl.n_full_on, 20,
        "the 12 old randomized sessions must not displace the 20 that describe the \
         setup the user is running now"
    );
    let truth = churn_store(0, 20).multiplier.unwrap();
    assert!(
        (hl.multiplier.unwrap() - truth).abs() < 1e-9,
        "the headline must describe the live setup, not the abandoned one"
    );
}

// ---------------------------------------------------------------------------
// 19. The live-set vote must be deterministic on tied timestamps.
// ---------------------------------------------------------------------------
//
// `max_by` returns the LAST maximum and `classified` is built by iterating a
// HashMap, whose order Rust re-randomizes per instance. So when the newest
// full-on sessions of two different saver sets tie on started_at, `live_set` was
// decided by hash order: the same database backed two different headline
// numbers between two refreshes IN ONE PROCESS, both badged the same. Real logs
// carry millisecond timestamps and do not collide, but the test helpers stamp a
// constant one, so this is a live trap for future fixtures.

#[test]
fn the_live_set_vote_is_deterministic_when_timestamps_tie() {
    fn build() -> attribution::Headline {
        let home = tempfile::tempdir().unwrap();
        let pricing = Pricing::embedded();
        let mut store = Store::open(home.path()).unwrap();
        let mut rng = XorShift64::new(0x0071_E000);
        for i in 0..12 {
            let turns = 8 + rng.below(4) as u64;
            let out = (2000.0 * noise(&mut rng, 0.2) * turns as f64).round() as u64;
            let id = format!("holdout-{i}");
            insert_session_at(&mut store, &pricing, &id, turns, out, "2026-01-01T00:00:00.000Z");
            store
                .set_session_savers(&id, &[SaverTag::new("rtk", false, source::HOLDOUT)])
                .unwrap();
        }
        // Two saver sets whose newest sessions share an EXACT timestamp.
        for i in 0..12 {
            let turns = 8 + rng.below(4) as u64;
            let out = (500.0 * noise(&mut rng, 0.2) * turns as f64).round() as u64;
            let id = format!("setA-{i}");
            insert_session_at(&mut store, &pricing, &id, turns, out, "2026-03-01T00:00:00.000Z");
            store
                .set_session_savers(
                    &id,
                    &[
                        SaverTag::new("rtk", true, source::ROTATION),
                        SaverTag::new("caveman", true, source::ROTATION),
                    ],
                )
                .unwrap();

            let turns = 8 + rng.below(4) as u64;
            let out = (1000.0 * noise(&mut rng, 0.2) * turns as f64).round() as u64;
            let id = format!("setB-{i}");
            insert_session_at(&mut store, &pricing, &id, turns, out, "2026-03-01T00:00:00.000Z");
            store
                .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
                .unwrap();
        }
        attribution::headline(&store, &pricing, 0x5150).unwrap()
    }

    // Fresh Store per call means a fresh HashMap, which is what re-rolls the
    // iteration order. One build is not a test; the point is that they agree.
    let first = build().multiplier.unwrap();
    for _ in 0..25 {
        let m = build().multiplier.unwrap();
        assert!(
            (m - first).abs() < 1e-9,
            "the headline changed from {first:.4} to {m:.4} on byte-identical data: the \
             live-set vote is being decided by HashMap order, so one database backs two \
             different numbers between refreshes"
        );
    }
}

// ---------------------------------------------------------------------------
// 20. A saver must not absorb the other savers' savings (#8).
// ---------------------------------------------------------------------------
//
// Rotation turns X off in two different kinds of slot, and they are NOT the same
// treatment: the single-off slot (X off, everything else running) and the
// holdout (X off and everything else off too). Pooling both into X's OFF group
// compared "X on, others on" against a 50/50 mix of "others on" and "others
// off", so the other savers' savings landed on X.
//
// Not an edge case: at shipping defaults every saver is off in exactly 2 of the
// 10 slots, 1 holdout + 1 single-off, so the OFF group was 50% "nothing on" for
// every user by construction. The mix weight was `holdout_fraction` - a
// measurement-cadence dial that silently moved every saver's percentage.
//
// Here rtk's true within-cell effect is exactly 50% (single-off-rtk 1000/turn vs
// full-on 500/turn). Only the number of holdouts varies. rtk's number must not
// notice them.

fn rtk_delta_with_holdouts(n_holdouts: usize) -> f64 {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();

    // Full-on: rtk AND caveman on. 500/turn.
    let mut rng = XorShift64::new(0x0088_0001);
    for i in 0..15 {
        let turns = 8 + rng.below(4) as u64;
        let out = (500.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("fullon-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", true, source::ROTATION),
                    SaverTag::new("caveman", true, source::ROTATION),
                ],
            )
            .unwrap();
    }
    // Single-off-rtk: rtk off, caveman still on. 1000/turn => rtk's true effect
    // is exactly 50%, and this is the ONLY comparison that isolates rtk.
    let mut rng = XorShift64::new(0x0088_0002);
    for i in 0..15 {
        let turns = 8 + rng.below(4) as u64;
        let out = (1000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("singleoff-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", false, source::ROTATION),
                    SaverTag::new("caveman", true, source::ROTATION),
                ],
            )
            .unwrap();
    }
    // Holdouts: rtk off AND caveman off. 2000/turn. rtk is off here too, so these
    // used to land in rtk's OFF group and drag its median up.
    let mut rng = XorShift64::new(0x0088_0003);
    for i in 0..n_holdouts {
        let turns = 8 + rng.below(4) as u64;
        let out = (2000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("holdout-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(
                &id,
                &[
                    SaverTag::new("rtk", false, source::HOLDOUT),
                    SaverTag::new("caveman", false, source::HOLDOUT),
                ],
            )
            .unwrap();
    }

    let a = attribution::attribute(&store, &pricing, "rtk", 0x0BAD).unwrap();
    let out = a.output().unwrap();
    eprintln!(
        "[absorb] holdouts={n_holdouts:>2} -> n_on={} n_off={} delta={:?}",
        out.n_on, out.n_off, out.delta
    );
    out.delta.unwrap()
}

#[test]
fn a_saver_does_not_absorb_the_other_savers_savings() {
    let truth = rtk_delta_with_holdouts(0);
    assert!(
        (truth - 0.5).abs() < 0.05,
        "sanity: rtk's planted within-cell effect is 50%, got {truth:.4}"
    );
    for n in [5usize, 15, 30] {
        let d = rtk_delta_with_holdouts(n);
        assert!(
            (d - truth).abs() < 1e-9,
            "rtk's number moved from {truth:.4} to {d:.4} because {n} HOLDOUT sessions \
             exist. rtk did nothing different in them: it is absorbing caveman's savings, \
             because a holdout is 'nothing on' and a single-off slot is 'everything else \
             on', and those are not the same comparison"
        );
    }
}

// The single-saver case must be untouched: with no other savers, a holdout and a
// single-off slot ARE the same state, so there is nothing to isolate and the
// holdout rows are legitimate OFF data.
#[test]
fn a_lone_saver_still_uses_its_holdout_sessions() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x0099_0001);

    for i in 0..12 {
        let turns = 8 + rng.below(4) as u64;
        let out = (1000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("on-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();

        let turns = 8 + rng.below(4) as u64;
        let out = (2000.0 * noise(&mut rng, 0.3) * turns as f64).round() as u64;
        let id = format!("holdout-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::HOLDOUT)])
            .unwrap();
    }

    let a = attribution::attribute(&store, &pricing, "rtk", 0x0C0D).unwrap();
    let out = a.output().unwrap();
    assert_eq!(
        out.n_off, 12,
        "with no other savers there is nothing to isolate from: the holdout sessions are \
         this saver's OFF group and must still count"
    );
    assert_eq!(out.badge, Badge::Measured);
}

// ---------------------------------------------------------------------------
// 21. Seeding the bootstrap has to actually make it reproducible.
// ---------------------------------------------------------------------------
//
// `bootstrap_deltas` resamples BY INDEX (`src[rng.below(src.len())]`), so the
// ORDER of the rate vectors decides which values a seeded run picks. Both group
// builders assemble their rows in a HashMap, whose iteration order Rust
// re-randomizes per instance, so an unsorted vector means the same seed produces
// a different CI on every call. The per-saver path had a test for this; the
// headline did not, and was nondeterministic.

#[test]
fn the_headline_ci_is_reproducible_for_a_fixed_seed() {
    let home = tempfile::tempdir().unwrap();
    let pricing = Pricing::embedded();
    let mut store = Store::open(home.path()).unwrap();
    let mut rng = XorShift64::new(0x00C1_0001);
    for i in 0..15 {
        let turns = 8 + rng.below(4) as u64;
        let out = (2000.0 * noise(&mut rng, 0.4) * turns as f64).round() as u64;
        let id = format!("holdout-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", false, source::HOLDOUT)])
            .unwrap();

        let turns = 8 + rng.below(4) as u64;
        let out = (1000.0 * noise(&mut rng, 0.4) * turns as f64).round() as u64;
        let id = format!("fullon-{i}");
        insert_session(
            &mut store, &pricing, &id, "claude-sonnet-4-5", turns, 400, out, 0, 0, false,
        );
        store
            .set_session_savers(&id, &[SaverTag::new("rtk", true, source::ROTATION)])
            .unwrap();
    }

    let first = attribution::headline(&store, &pricing, 0xFEED).unwrap();
    let f = first.streams.iter().find(|s| s.stream == attribution::Stream::Output).unwrap();
    for _ in 0..20 {
        let again = attribution::headline(&store, &pricing, 0xFEED).unwrap();
        let a = again.streams.iter().find(|s| s.stream == attribution::Stream::Output).unwrap();
        assert_eq!(
            f.ci, a.ci,
            "same store, same seed, different CI: the bootstrap is resampling a vector \
             whose order came out of a HashMap, so seeding it buys nothing"
        );
    }
}
