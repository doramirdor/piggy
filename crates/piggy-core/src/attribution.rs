//! Honest per-saver savings attribution (M3).
//!
//! Everything here follows `docs/measurement.md` to the letter:
//!
//! * All comparisons are **normalized per-turn rates** (tokens per deduplicated
//!   assistant message), never raw totals — task size, model, and session length
//!   confound totals.
//! * Per saver X we compare the ON group (X enabled) against the OFF group
//!   (X disabled). Per stream the delta is `1 - median(rate_on)/median(rate_off)`.
//!   The OFF group is split by randomization: rotation single-off + holdout are
//!   randomized (measured-eligible); pre-install / manual sessions are
//!   observational. Observational rows are **never pooled into a measured
//!   badge** — leaning on them (only when randomized data is short) caps the
//!   figure at `estimated`, so pre/post-install drift can't masquerade as a
//!   randomized effect.
//! * The uncertainty is a **bootstrap 90% confidence interval** (1000 resamples)
//!   built with the crate's deterministic [`crate::rng`] PRNG. A finding is only
//!   badged `measured`/`estimated` when the CI excludes zero **with positive
//!   width** **and** both groups have at least [`MIN_GROUP`] sessions — otherwise
//!   it is `measuring` (never a point claim below the bar).
//! * Subagent sub-session files (`…/subagents/…`) are excluded from the groups:
//!   they inherit the parent's saver set but their per-turn rates are not
//!   comparable. Their tokens still land in the raw totals reported elsewhere.
//! * The headline "your plan lasts N.N× longer" is full-on vs holdout on
//!   price-weighted spend, so it is `estimated`; the per-stream percentages that
//!   accompany it are `measured` when the baseline is a live holdout and
//!   `estimated` when it falls back to observational pre-install history.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::params;

use crate::pricing::Pricing;
use crate::rng::XorShift64;
use crate::store::{source, Store};
use crate::ModelTokens;

/// Minimum sessions per side for a green `measured` badge.
pub const MIN_GROUP: usize = 10;
/// Bootstrap resample count for the confidence interval.
pub const BOOTSTRAP_N: usize = 1000;
/// Two-sided alpha for the **displayed** interval (the spec's 90% CI).
pub const CI_ALPHA: f64 = 0.10;
/// Number of per-stream badges shown together for one saver/headline. The badge
/// gate is Bonferroni-corrected across this family so the *family-wise* chance a
/// truly-null saver lights up any green badge stays near the ~10% a reader infers
/// from a single 90% CI — rather than the ~1-0.9^4 ≈ 34% of four naive gates. The
/// displayed CI is still the spec-mandated 90%; the correction only ever
/// *withholds* a badge, never invents one.
pub const STREAM_FAMILY: usize = Stream::ALL.len();

/// The four token streams a saver can move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Input,
    Output,
    CacheCreate,
    CacheRead,
}

impl Stream {
    /// All four, in display order.
    pub const ALL: [Stream; 4] = [
        Stream::Input,
        Stream::Output,
        Stream::CacheCreate,
        Stream::CacheRead,
    ];

    /// Human label for a report row.
    pub fn label(&self) -> &'static str {
        match self {
            Stream::Input => "input",
            Stream::Output => "output",
            Stream::CacheCreate => "cache write",
            Stream::CacheRead => "cache read",
        }
    }

    fn tokens_of(&self, r: &SessionRates) -> u64 {
        match self {
            Stream::Input => r.input,
            Stream::Output => r.output,
            Stream::CacheCreate => r.cache_create,
            Stream::CacheRead => r.cache_read,
        }
    }
}

/// Per-session normalized figures (one row of the read model).
#[derive(Debug, Clone)]
pub struct SessionRates {
    pub session_id: String,
    /// Deduplicated assistant turns — the per-turn normalizer.
    pub turns: u64,
    pub input: u64,
    pub output: u64,
    pub cache_create: u64,
    pub cache_read: u64,
    /// Price-weighted plan spend (input + output + cache-write), cache reads
    /// excluded. Only meaningful when [`Self::fully_priced`].
    pub priced_spend: f64,
    /// Whether every model in the session had a known price.
    pub fully_priced: bool,
}

impl SessionRates {
    fn rate(&self, tokens: u64) -> Option<f64> {
        if self.turns == 0 {
            None
        } else {
            Some(tokens as f64 / self.turns as f64)
        }
    }
    fn spend_rate(&self) -> Option<f64> {
        if self.turns == 0 || !self.fully_priced {
            None
        } else {
            Some(self.priced_spend / self.turns as f64)
        }
    }
}

/// Session-level A/B classification for the headline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionGroup {
    /// Every managed saver was on, and every one of them was there because
    /// Piggy's scheduler put it there (`rotation` / `holdout`). Randomized, so
    /// measured-eligible.
    FullOn,
    /// Every managed saver was on, but at least one because the user pinned it
    /// (`manual`) or it predates Piggy. Same state, non-randomized provenance:
    /// usable as an observational ON group, capped at `estimated`.
    FullOnObservational,
    /// Rotation holdout — every saver off.
    Holdout,
    /// Predates Piggy — observational baseline (all off).
    PreInstall,
    /// Some on, some off (single-off rotation slots).
    Mixed,
}

/// Whether a badge may show a number, and what kind of claim it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// Derived from a **randomized** A/B comparison (rotation single-off +
    /// holdout): CI excludes zero, has positive width, and both groups meet
    /// [`MIN_GROUP`]. The only kind that earns the green "measured" claim.
    Measured,
    /// Same math, but the OFF/baseline group is the **observational**
    /// pre-install baseline (non-randomized) rather than a live holdout. Shown
    /// with a number but labelled `estimated` — never conflated with measured.
    Estimated,
    /// Below the bar — show "not enough data yet · n", never a point estimate.
    Measuring,
}

impl Badge {
    /// Whether this badge shows a point percentage (measured or estimated).
    pub fn shows_number(&self) -> bool {
        matches!(self, Badge::Measured | Badge::Estimated)
    }
}

/// One stream's measured delta with its confidence interval and badge.
#[derive(Debug, Clone)]
pub struct StreamStat {
    pub stream: Stream,
    pub n_on: usize,
    pub n_off: usize,
    pub median_on: f64,
    pub median_off: f64,
    /// `1 - median_on/median_off`; `None` when the OFF median is zero.
    pub delta: Option<f64>,
    /// 90% bootstrap CI on `delta`.
    pub ci: Option<(f64, f64)>,
    pub badge: Badge,
}

impl StreamStat {
    /// The point percentage a badge is allowed to show — `Some` for both
    /// `measured` and `estimated`, `None` while still `measuring` (so the caller
    /// shows the neutral "not enough data yet" state, never a point estimate).
    pub fn shown_pct(&self) -> Option<f64> {
        match (self.badge, self.delta) {
            (b, Some(d)) if b.shows_number() => Some(d * 100.0),
            _ => None,
        }
    }

    /// A percentage figure only when the claim is a **measured** (randomized)
    /// one — never for an observational `estimated` figure.
    pub fn measured_pct(&self) -> Option<f64> {
        match (self.badge, self.delta) {
            (Badge::Measured, Some(d)) => Some(d * 100.0),
            _ => None,
        }
    }
}

/// Full attribution for one saver.
#[derive(Debug, Clone)]
pub struct SaverAttribution {
    pub saver_id: String,
    pub n_on: usize,
    pub n_off: usize,
    /// Breakdown of the OFF group by source (`rotation`/`holdout`/`pre_install`),
    /// so the report can flag the pre-install baseline separately.
    pub off_by_source: BTreeMap<String, usize>,
    pub streams: Vec<StreamStat>,
}

impl SaverAttribution {
    /// The output-stream stat (the headline per-saver number).
    pub fn output(&self) -> Option<&StreamStat> {
        self.streams.iter().find(|s| s.stream == Stream::Output)
    }
}

/// Which baseline the headline multiplier is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlineBaseline {
    /// Live holdout sessions (the honest default).
    Holdout,
    /// Pre-install history — observational, labelled as such.
    PreInstall,
    /// No baseline available yet.
    None,
}

/// The dashboard headline.
#[derive(Debug, Clone)]
pub struct Headline {
    pub baseline: HeadlineBaseline,
    pub n_full_on: usize,
    pub n_baseline: usize,
    /// Whether the full-on side is backed by **randomized** sessions (every
    /// saver on because Piggy's scheduler said so). False once the ON group has
    /// to lean on manually-pinned sessions, which are observational however many
    /// of them there are.
    ///
    /// A `measured` label needs BOTH sides randomized, so callers must check
    /// this as well as `baseline == Holdout`. Without it, "recent manual-on era
    /// vs older holdout era" reads as measured and any drift between the eras is
    /// credited to the savers.
    pub on_randomized: bool,
    /// `median(baseline spend rate) / median(full_on spend rate)` — "lasts N.N×
    /// longer". Price-weighted, hence `estimated`. `None` if not computable.
    pub multiplier: Option<f64>,
    /// Per-stream measured deltas (full-on vs baseline), shown before the ×.
    pub streams: Vec<StreamStat>,
}

// ---------------------------------------------------------------------------
// Read model (SQL → per-session rows)
// ---------------------------------------------------------------------------

impl Store {
    /// Per-session normalized rates for every non-subagent session, keyed by id.
    ///
    /// Subagent sub-session files (`…/subagents/…`) are excluded here so they
    /// never enter an attribution group. Token sums come from `session_models`;
    /// the price-weighted spend uses `pricing` (models without a price mark the
    /// session `fully_priced = false`).
    pub fn session_rate_map(
        &self,
        pricing: &Pricing,
    ) -> Result<std::collections::HashMap<String, SessionRates>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.session_id, s.n_msgs,
                    sm.model, COALESCE(sm.input_tokens, 0), COALESCE(sm.output_tokens, 0),
                    COALESCE(sm.cache_creation_tokens, 0), COALESCE(sm.cache_creation_1h_tokens, 0),
                    COALESCE(sm.cache_read_tokens, 0)
             FROM sessions s
             LEFT JOIN session_models sm ON sm.session_id = s.session_id
             WHERE NOT EXISTS (
                 SELECT 1 FROM files f
                 WHERE f.session_id = s.session_id AND f.path LIKE '%/subagents/%'
             )",
        )?;
        let mut map: std::collections::HashMap<String, SessionRates> =
            std::collections::HashMap::new();
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,         // session_id
                r.get::<_, u64>(1)?,            // n_msgs (turns)
                r.get::<_, Option<String>>(2)?, // model
                r.get::<_, u64>(3)?,            // input
                r.get::<_, u64>(4)?,            // output
                r.get::<_, u64>(5)?,            // cache_create total
                r.get::<_, u64>(6)?,            // cache_create 1h
                r.get::<_, u64>(7)?,            // cache_read
            ))
        })?;
        for row in rows {
            let (sid, turns, model, input, output, cc, cc1h, cr) = row?;
            let entry = map.entry(sid.clone()).or_insert_with(|| SessionRates {
                session_id: sid.clone(),
                turns,
                input: 0,
                output: 0,
                cache_create: 0,
                cache_read: 0,
                priced_spend: 0.0,
                fully_priced: true,
            });
            // turns is a per-session fact; keep the max seen (rows repeat it).
            entry.turns = entry.turns.max(turns);
            entry.input += input;
            entry.output += output;
            entry.cache_create += cc;
            entry.cache_read += cr;
            if let Some(model) = model {
                let tok = ModelTokens {
                    input_tokens: input,
                    output_tokens: output,
                    cache_creation_tokens: cc,
                    cache_creation_1h_tokens: cc1h,
                    cache_read_tokens: cr,
                };
                match pricing.plan_metered_spend(&model, &tok) {
                    Some(spend) => entry.priced_spend += spend,
                    None => entry.fully_priced = false,
                }
            }
        }
        Ok(map)
    }

    /// Every session's `(saver_id, enabled, source)` snapshot for `saver_id`,
    /// paired with its rates. Sessions with no tag for this saver are omitted.
    fn saver_group_rows(
        &self,
        saver_id: &str,
        rate_map: &std::collections::HashMap<String, SessionRates>,
    ) -> Result<Vec<(bool, String, SessionRates)>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, enabled, source FROM session_savers WHERE saver_id = ?1",
        )?;
        let rows = stmt.query_map(params![saver_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (sid, enabled, src) = row?;
            if let Some(rates) = rate_map.get(&sid) {
                out.push((enabled, src, rates.clone()));
            }
        }
        Ok(out)
    }

    /// The session-level A/B classification for every tagged, non-subagent
    /// session, paired with its rates.
    fn classified_sessions(
        &self,
        rate_map: &std::collections::HashMap<String, SessionRates>,
    ) -> Result<Vec<(SessionGroup, SessionRates)>> {
        // Pull every tag, group by session in Rust.
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, enabled, source FROM session_savers")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? != 0,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut per_session: std::collections::HashMap<String, (bool, bool, bool, bool, bool)> =
            std::collections::HashMap::new();
        // tuple = (any_holdout, any_pre_install, all_enabled, any_disabled,
        //          all_randomized)
        for row in rows {
            let (sid, enabled, src) = row?;
            let e = per_session
                .entry(sid)
                .or_insert((false, false, true, false, true));
            if src == source::HOLDOUT {
                e.0 = true;
            }
            if src == source::PRE_INSTALL {
                e.1 = true;
            }
            if enabled {
                // all_enabled stays true only if every row is enabled
            } else {
                e.2 = false;
                e.3 = true;
            }
            if !is_randomized(&src) {
                e.4 = false;
            }
        }
        let mut out = Vec::new();
        for (sid, (holdout, pre, all_on, any_off, all_randomized)) in per_session {
            let Some(rates) = rate_map.get(&sid) else {
                continue;
            };
            let group = if holdout {
                SessionGroup::Holdout
            } else if pre {
                SessionGroup::PreInstall
            } else if all_on && !any_off {
                // Same "everything on" state, but only randomized provenance can
                // back a measured claim. A user who pins savers on by hand takes
                // them out of rotation for good, so without this split a
                // manual-on era compared against an older randomized holdout era
                // reads as a green measured headline, with any drift between the
                // eras landing on the savers.
                if all_randomized {
                    SessionGroup::FullOn
                } else {
                    SessionGroup::FullOnObservational
                }
            } else {
                SessionGroup::Mixed
            };
            out.push((group, rates.clone()));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// Median of a slice (0.0 for empty). Copies and sorts; inputs are small.
pub fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// The delta `1 - median(on)/median(off)`, or `None` if `off` medians to zero.
fn delta_of(on: &[f64], off: &[f64]) -> Option<f64> {
    let mo = median(off);
    if mo == 0.0 {
        return None;
    }
    Some(1.0 - median(on) / mo)
}

/// Bootstrap the sorted delta distribution by resampling both groups with
/// replacement. Deterministic given `seed`. Returns `None` if either group is
/// empty or every resample hit a degenerate off-median.
fn bootstrap_deltas(on: &[f64], off: &[f64], seed: u64) -> Option<Vec<f64>> {
    if on.is_empty() || off.is_empty() {
        return None;
    }
    let mut rng = XorShift64::new(seed);
    let mut deltas = Vec::with_capacity(BOOTSTRAP_N);
    let resample = |rng: &mut XorShift64, src: &[f64], scratch: &mut Vec<f64>| {
        scratch.clear();
        for _ in 0..src.len() {
            scratch.push(src[rng.below(src.len())]);
        }
    };
    let mut on_s = Vec::with_capacity(on.len());
    let mut off_s = Vec::with_capacity(off.len());
    for _ in 0..BOOTSTRAP_N {
        resample(&mut rng, on, &mut on_s);
        resample(&mut rng, off, &mut off_s);
        let mo = median(&off_s);
        if mo == 0.0 {
            continue;
        }
        deltas.push(1.0 - median(&on_s) / mo);
    }
    if deltas.is_empty() {
        return None;
    }
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(deltas)
}

/// A two-sided CI at confidence `1 - alpha` from a **sorted** bootstrap sample.
fn ci_at(sorted: &[f64], alpha: f64) -> (f64, f64) {
    (
        percentile(sorted, alpha / 2.0),
        percentile(sorted, 1.0 - alpha / 2.0),
    )
}

/// Linear-interpolation percentile of a **sorted** slice (`q` in `0.0..=1.0`).
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let pos = q * (sorted.len() as f64 - 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = pos - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

/// Whether a CI strictly excludes zero (both bounds the same, non-zero sign).
fn ci_excludes_zero(ci: (f64, f64)) -> bool {
    (ci.0 > 0.0 && ci.1 > 0.0) || (ci.0 < 0.0 && ci.1 < 0.0)
}

/// Whether a CI is strong enough to earn a point badge: it excludes zero **and**
/// has positive width. A zero-width CI (identical arms, no variance) is infinite
/// false precision, not evidence, so it never clears the bar.
fn ci_is_significant(ci: (f64, f64)) -> bool {
    ci_excludes_zero(ci) && ci.1 > ci.0
}

/// Compute one stream's stat from paired ON/OFF session rate vectors.
///
/// `ceiling` is the strongest badge this comparison may earn: `Measured` when
/// the OFF group is randomized (rotation/holdout), `Estimated` when it is the
/// observational pre-install baseline. A comparison that clears the CI bar is
/// badged `ceiling`; otherwise `Measuring`.
///
/// The **displayed** interval is the spec's 90% CI, but the badge *gate* uses a
/// Bonferroni-corrected interval (alpha `CI_ALPHA / STREAM_FAMILY`) so showing
/// four per-stream badges doesn't inflate the family-wise false-positive rate.
fn stream_stat(stream: Stream, on: &[f64], off: &[f64], ceiling: Badge, seed: u64) -> StreamStat {
    debug_assert!(
        ceiling.shows_number(),
        "ceiling must be Measured or Estimated"
    );
    let delta = delta_of(on, off);
    let deltas = bootstrap_deltas(on, off, seed);
    // Displayed CI: the spec-mandated 90%.
    let ci = deltas.as_ref().map(|d| ci_at(d, CI_ALPHA));
    // Gate CI: family-corrected, so the *family-wise* rate stays near nominal.
    let gate_ci = deltas
        .as_ref()
        .map(|d| ci_at(d, CI_ALPHA / STREAM_FAMILY as f64));
    let enough = on.len() >= MIN_GROUP && off.len() >= MIN_GROUP;
    let significant = gate_ci.map(ci_is_significant).unwrap_or(false);
    let badge = if enough && delta.is_some() && significant {
        ceiling
    } else {
        Badge::Measuring
    };
    StreamStat {
        stream,
        n_on: on.len(),
        n_off: off.len(),
        median_on: median(on),
        median_off: median(off),
        delta,
        ci,
        badge,
    }
}

/// Per-stream rate vectors (skipping zero-turn sessions) for a set of sessions.
fn rate_vectors<'a>(stream: Stream, sessions: impl Iterator<Item = &'a SessionRates>) -> Vec<f64> {
    sessions
        .filter_map(|s| s.rate(stream.tokens_of(s)))
        .collect()
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Whether a session's `source` came from Piggy's **randomized** scheduler
/// (rotation single-off / full-on, or holdout). Only these are eligible for a
/// `measured` badge. `pre_install` (predates Piggy) and `manual` (a deliberate
/// user toggle) are non-randomized/observational and can back at most an
/// `estimated` figure.
///
/// This applies to BOTH sides of the comparison. Randomization is a property of
/// the contrast, not of the off-switch: a manual-on era measured against an
/// older randomized-off era is just as confounded as the reverse.
fn is_randomized(src: &str) -> bool {
    src == source::ROTATION || src == source::HOLDOUT
}

/// Choose the rows for one side of the comparison, and the best badge that side
/// can back.
///
/// Randomized rows alone when they meet [`MIN_GROUP`] (or when there is nothing
/// observational to add anyway), which keeps a `measured` claim on randomized
/// evidence only. Otherwise pool in the observational rows for a usable figure
/// and cap that side at `estimated`.
fn pick_group(
    randomized: Vec<SessionRates>,
    observational: Vec<SessionRates>,
) -> (Vec<SessionRates>, Badge) {
    if randomized.len() >= MIN_GROUP || observational.is_empty() {
        (randomized, Badge::Measured)
    } else {
        let mut pooled = randomized;
        pooled.extend(observational);
        (pooled, Badge::Estimated)
    }
}

/// Attribute savings to a single saver. `seed` seeds the bootstrap (fix it in
/// tests; time-seed it in production).
///
/// The OFF group is split by randomization. Non-randomized pre-install /
/// observational sessions are **never pooled into a measured badge** — that
/// would let pre/post-install drift masquerade as a randomized effect. When
/// there is enough randomized OFF data, the comparison is measured off that
/// alone. Only when randomized OFF is short do we fall back to the observational
/// baseline, and then the figure is capped at `estimated` (mirroring the
/// headline's holdout-preferred / pre-install-fallback logic).
pub fn attribute(
    store: &Store,
    pricing: &Pricing,
    saver_id: &str,
    seed: u64,
) -> Result<SaverAttribution> {
    let rate_map = store.session_rate_map(pricing)?;
    attribute_with_map(store, &rate_map, saver_id, seed)
}

/// Like [`attribute`] but reuses a prebuilt `rate_map`. Building the per-session
/// rate map is a full-table scan; a dashboard refresh attributes every curated
/// saver *and* the headline, so callers build the map once and pass it here to
/// avoid ~one full scan per saver.
pub fn attribute_with_map(
    store: &Store,
    rate_map: &std::collections::HashMap<String, SessionRates>,
    saver_id: &str,
    seed: u64,
) -> Result<SaverAttribution> {
    let rows = store.saver_group_rows(saver_id, rate_map)?;

    let on_randomized: Vec<SessionRates> = rows
        .iter()
        .filter(|(en, src, _)| *en && is_randomized(src))
        .map(|(_, _, r)| r.clone())
        .collect();
    let on_observational: Vec<SessionRates> = rows
        .iter()
        .filter(|(en, src, _)| *en && !is_randomized(src))
        .map(|(_, _, r)| r.clone())
        .collect();
    let off_randomized: Vec<SessionRates> = rows
        .iter()
        .filter(|(en, src, _)| !*en && is_randomized(src))
        .map(|(_, _, r)| r.clone())
        .collect();
    let off_observational: Vec<SessionRates> = rows
        .iter()
        .filter(|(en, src, _)| !*en && !is_randomized(src))
        .map(|(_, _, r)| r.clone())
        .collect();
    let mut off_by_source: BTreeMap<String, usize> = BTreeMap::new();
    for (en, src, _) in &rows {
        if !*en {
            *off_by_source.entry(src.clone()).or_insert(0) += 1;
        }
    }

    // Prefer the randomized rows on each side (measured-eligible). Only lean on
    // observational rows when the randomized group can't stand on its own, and
    // then cap that side's badge at `estimated`.
    //
    // Both sides get this treatment. Applying it to OFF alone left a hole: once
    // a user manually toggles a saver, `rotation::controlled_savers` pins it out
    // of rotation for good, so every later session is (enabled, source=manual)
    // while the older rotation/holdout rows stay in `off_randomized`. With
    // >= MIN_GROUP of those, the comparison became "recent manual-on era vs
    // older randomized-off era" and still badged green. That contrast is
    // observational: any drift between the eras lands on the saver.
    let (on_used, on_ceiling) = pick_group(on_randomized, on_observational);
    let (off_used, off_ceiling) = pick_group(off_randomized, off_observational);
    // The weaker side governs: a randomized OFF group cannot launder a
    // non-randomized ON group into a measured claim.
    let ceiling = if on_ceiling == Badge::Measured && off_ceiling == Badge::Measured {
        Badge::Measured
    } else {
        Badge::Estimated
    };
    let on = on_used;

    let streams = Stream::ALL
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let on_v = rate_vectors(s, on.iter());
            let off_v = rate_vectors(s, off_used.iter());
            // Per-stream seed offset keeps streams independent yet deterministic.
            stream_stat(
                s,
                &on_v,
                &off_v,
                ceiling,
                seed ^ ((i as u64 + 1) * 0x9E37_79B9),
            )
        })
        .collect();

    Ok(SaverAttribution {
        saver_id: saver_id.to_string(),
        n_on: on.len(),
        n_off: off_used.len(),
        off_by_source,
        streams,
    })
}

/// Compute the dashboard headline (full-on vs holdout, else vs pre-install).
pub fn headline(store: &Store, pricing: &Pricing, seed: u64) -> Result<Headline> {
    let rate_map = store.session_rate_map(pricing)?;
    headline_with_map(store, &rate_map, seed)
}

/// Like [`headline`] but reuses a prebuilt `rate_map` (see [`attribute_with_map`]).
pub fn headline_with_map(
    store: &Store,
    rate_map: &std::collections::HashMap<String, SessionRates>,
    seed: u64,
) -> Result<Headline> {
    let classified = store.classified_sessions(rate_map)?;

    let full_on_randomized: Vec<SessionRates> = classified
        .iter()
        .filter(|(g, _)| *g == SessionGroup::FullOn)
        .map(|(_, r)| r.clone())
        .collect();
    let full_on_observational: Vec<SessionRates> = classified
        .iter()
        .filter(|(g, _)| *g == SessionGroup::FullOnObservational)
        .map(|(_, r)| r.clone())
        .collect();
    let holdout: Vec<SessionRates> = classified
        .iter()
        .filter(|(g, _)| *g == SessionGroup::Holdout)
        .map(|(_, r)| r.clone())
        .collect();
    let pre_install: Vec<SessionRates> = classified
        .iter()
        .filter(|(g, _)| *g == SessionGroup::PreInstall)
        .map(|(_, r)| r.clone())
        .collect();

    // Prefer a live holdout; fall back to observational pre-install history.
    // A holdout is randomized (measured-eligible); the pre-install baseline is
    // observational, so its per-stream figures are capped at `estimated`.
    let (baseline_kind, baseline) = if !holdout.is_empty() {
        (HeadlineBaseline::Holdout, holdout)
    } else if !pre_install.is_empty() {
        (HeadlineBaseline::PreInstall, pre_install)
    } else {
        (HeadlineBaseline::None, Vec::new())
    };
    let baseline_ceiling = match baseline_kind {
        HeadlineBaseline::Holdout => Badge::Measured,
        // No live holdout: any figure is observational.
        HeadlineBaseline::PreInstall | HeadlineBaseline::None => Badge::Estimated,
    };

    // The ON side gets the same treatment as the baseline, and as the per-saver
    // path in `attribute_with_map`. A randomized holdout on one side cannot make
    // a manual-on era on the other side measured: randomization is a property of
    // the contrast, not of the off-switch.
    let (full_on, on_ceiling) = pick_group(full_on_randomized, full_on_observational);
    let ceiling = if baseline_ceiling == Badge::Measured && on_ceiling == Badge::Measured {
        Badge::Measured
    } else {
        Badge::Estimated
    };
    let on_randomized = on_ceiling == Badge::Measured;
    let n_baseline = baseline.len();

    // Price-weighted "lasts N.N× longer" (estimated).
    let on_spend: Vec<f64> = full_on.iter().filter_map(|s| s.spend_rate()).collect();
    let off_spend: Vec<f64> = baseline.iter().filter_map(|s| s.spend_rate()).collect();
    let multiplier = {
        let mon = median(&on_spend);
        let moff = median(&off_spend);
        if mon > 0.0 && moff > 0.0 {
            Some(moff / mon)
        } else {
            None
        }
    };

    // Per-stream measured deltas (full-on vs baseline).
    let streams = Stream::ALL
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            let on_v = rate_vectors(s, full_on.iter());
            let off_v = rate_vectors(s, baseline.iter());
            stream_stat(
                s,
                &on_v,
                &off_v,
                ceiling,
                seed ^ ((i as u64 + 101) * 0x85EB_CA6B),
            )
        })
        .collect();

    Ok(Headline {
        baseline: baseline_kind,
        n_full_on: full_on.len(),
        n_baseline,
        on_randomized,
        multiplier,
        streams,
    })
}
