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
