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
/// Smallest OFF-group median (tokens per turn) a **ratio** may be taken against.
///
/// `delta` is `1 - median_on/median_off`, so a stream the baseline barely uses
/// turns a rounding difference into a headline percentage. Real profile: with
/// every prompt served from cache the input stream medians at ~2 tokens/turn,
/// and a 400-token/turn ON median printed `-20071%`. Guarding only `== 0.0` is
/// not enough — the failure is continuous, not a special case at zero.
///
/// A stream this quiet has no percentage worth showing: 10 tokens/turn is under
/// a thousandth of the streams that actually carry traffic here (3k-100k), and
/// halving it saves 5 tokens a turn. Below the floor the stream reports
/// `measuring` rather than a number.
// ponytail: absolute floor, not a share of the other streams. Revisit if a
// workload ever runs every stream this thin, where the floor would mute a real
// saving instead of noise.
pub const MIN_RATE_DENOM: f64 = 10.0;
/// The same floor for the **turns** arm, in that arm's own units.
///
/// [`MIN_RATE_DENOM`] is calibrated on tokens per turn, where the streams that
/// carry traffic sit in the thousands and 10 is noise. Turns per session is a
/// different scale entirely: a normal session is single digits to low tens, so
/// applying the token floor here muted the arm on ordinary data. Worse,
/// [`SaverAttribution::is_negligible`] reads "both arms under the floor" as a
/// proven null, which certified a saver that doubled the turn count from 4 to 9
/// as having done nothing.
///
/// `turn_vectors` drops zero-turn sessions, so the smallest median possible is
/// 1, where the ratio can only move in whole-session jumps (1 -> 2 turns reads
/// as -100%). Two turns is the first denominator a ratio has any resolution
/// against, and below it there is no multi-turn behaviour to compare.
pub const MIN_TURNS_DENOM: f64 = 2.0;
/// Half-width of the band a saver's effect must sit **entirely inside** before
/// its eras may be folded into the headline's ON arm (see
/// [`SaverAttribution::is_negligible`]).
///
/// This is an equivalence bound, not a significance test, and the difference is
/// the whole point. "The CI includes zero" means *we did not detect an effect*,
/// which is also what a 3-session sample says; folding eras together on that
/// basis would pool on ignorance. Requiring the entire interval to fall within
/// ±5% says something stronger: we looked, we had the resolution to see a real
/// effect, and there was not one bigger than this.
///
/// 5% is chosen against what the headline actually prints. The multiplier is
/// shown to one decimal ("1.4× longer"), so a saver that could only ever have
/// moved a stream by a twentieth cannot move the digit the user reads.
pub const NULL_BAND: f64 = 0.05;
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
    /// Assistant turns per session. Deliberately NOT in [`Stream::ALL`]: it is
    /// the denominator the four token streams divide by, not a fifth stream.
    /// It gets its own arm because a per-turn metric cannot see a saver that
    /// buys cheaper turns by needing more of them.
    Turns,
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
            Stream::Turns => "turns per session",
        }
    }

    /// The floor this arm's OFF median must clear before a ratio against it is
    /// worth taking, in the arm's OWN units. One constant cannot gate both: the
    /// token streams are tokens per turn (thousands when in use), turns is turns
    /// per session (single digits), and a floor set for the first silences the
    /// second on every ordinary workload.
    pub fn min_denom(&self) -> f64 {
        match self {
            Stream::Turns => MIN_TURNS_DENOM,
            _ => MIN_RATE_DENOM,
        }
    }

    fn tokens_of(&self, r: &SessionRates) -> u64 {
        match self {
            Stream::Input => r.input,
            Stream::Output => r.output,
            Stream::CacheCreate => r.cache_create,
            Stream::CacheRead => r.cache_read,
            // Never divided by anything: see `turn_vectors`.
            Stream::Turns => r.turns,
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
    /// Rotation holdout, and a clean one: every saver really was off.
    Holdout,
    /// A holdout slot that had at least one saver still running. `controlled_savers`
    /// drops manually-toggled savers from rotation, so a saver the user pinned on
    /// rides straight through the "all off" slot. The contrast is still randomized
    /// for the savers that DO rotate, but the headline's counterfactual (no savers
    /// at all) was never actually observed, so it can only back an `estimated`
    /// figure.
    HoldoutContaminated,
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

    /// What this stream actually tells the reader. See [`Reading`].
    pub fn reading(&self) -> Reading {
        if self.badge.shows_number() {
            return Reading::Delta;
        }
        if self.n_on < MIN_GROUP || self.n_off < MIN_GROUP {
            return Reading::Waiting {
                need_on: MIN_GROUP.saturating_sub(self.n_on),
                need_off: MIN_GROUP.saturating_sub(self.n_off),
            };
        }
        // No ratio at all: the OFF median sat under this arm's `min_denom`,
        // which is the case those floors exist for.
        if self.delta.is_none() {
            return Reading::Quiet;
        }
        match self.ci {
            Some((lo, hi)) if lo <= 0.0 && hi >= 0.0 => {
                let bound = lo.abs().max(hi.abs());
                if bound <= NULL_BAND {
                    Reading::NoChange { bound }
                } else {
                    Reading::Inconclusive
                }
            }
            // An interval that excludes zero yet earned no badge was withheld by
            // the family-corrected gate (or the plausibility check). Quoting it
            // here would hand back the number that gate just refused.
            _ => Reading::Inconclusive,
        }
    }

    /// The row's sentence for the states where no number may be shown. `None`
    /// when the badge shows a delta: there the number is the sentence.
    pub fn note(&self) -> Option<String> {
        match self.reading() {
            Reading::Delta => None,
            Reading::Waiting { need_on, need_off } => Some(match (need_on, need_off) {
                (0, off) => format!("needs {off} more {} with it off", sessions(off)),
                (on, 0) => format!("needs {on} more {} with it on", sessions(on)),
                (on, off) => format!(
                    "needs {on} more {} with it on and {off} with it off",
                    sessions(on)
                ),
            }),
            Reading::Quiet => Some(match self.stream {
                Stream::Turns => "too few turns per session to compare".to_string(),
                _ => format!(
                    "under {} tokens a turn on both sides, too small to compare",
                    MIN_RATE_DENOM as u64
                ),
            }),
            // Rounded AWAY from zero, so the sentence claims less than the
            // interval supports rather than more.
            Reading::NoChange { bound } => Some(format!(
                "measured, and there is no change bigger than {}% either way",
                ((bound * 100.0).ceil() as u64).max(1)
            )),
            Reading::Inconclusive => {
                Some("compared, but the result is still too noisy to call".to_string())
            }
        }
    }
}

fn sessions(n: usize) -> &'static str {
    if n == 1 {
        "session"
    } else {
        "sessions"
    }
}

/// What a stream's comparison tells the reader, once the badge has decided
/// whether a number may be shown.
///
/// [`Badge::Measuring`] covers three situations the UI used to render with one
/// word, and they call for opposite things from the reader. "Not enough
/// sessions yet" is a wait. "Both sides are too small to divide" is a
/// permanent no. "We compared and found nothing bigger than 1%" is a **result**
/// — the most common honest outcome in the catalogue, and the one that reads as
/// a broken progress bar when it is labelled the same as the other two.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reading {
    /// The badge shows a delta; that number is the reading.
    Delta,
    /// An arm is still short of [`MIN_GROUP`].
    Waiting { need_on: usize, need_off: usize },
    /// The OFF median is under the arm's [`Stream::min_denom`], so no ratio is
    /// worth taking.
    Quiet,
    /// Both arms are full, and the whole 90% interval lies within `±bound`.
    NoChange { bound: f64 },
    /// Both arms are full, but the interval is too wide (or was withheld by the
    /// badge gate) to conclude anything from.
    Inconclusive,
}

impl Reading {
    /// Stable key for the wire and for UI switches. Never derived from the
    /// sentence: prose is for reading, this is for branching.
    pub fn key(&self) -> &'static str {
        match self {
            Reading::Delta => "delta",
            Reading::Waiting { .. } => "waiting",
            Reading::Quiet => "quiet",
            Reading::NoChange { .. } => "no_change",
            Reading::Inconclusive => "inconclusive",
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
    /// Turns per session with this saver on vs off. Not one of `streams` for the
    /// same reason it is not one of [`Headline::streams`]: it is the denominator
    /// they divide by, and a saver that buys cheaper turns by needing more of
    /// them looks green on every stream above while costing more overall.
    pub turns: StreamStat,
}

impl SaverAttribution {
    /// The output-stream stat (the headline per-saver number).
    pub fn output(&self) -> Option<&StreamStat> {
        self.streams.iter().find(|s| s.stream == Stream::Output)
    }

    /// Every arm of the comparison, the denominator included. Turns is not a
    /// fifth stream, but it is a fifth thing that was compared, and a reader
    /// asking "what did this saver do" is owed it in the same breath.
    pub fn arms(&self) -> impl Iterator<Item = &StreamStat> {
        self.streams.iter().chain(std::iter::once(&self.turns))
    }

    /// The one-line learning: what this saver's comparison has shown, in the
    /// reader's terms rather than the badge's.
    ///
    /// Ordered by what the reader can act on. A settled figure outranks
    /// everything (it is the answer they came for); a full comparison that
    /// found nothing is the next most useful thing to know and is stated as a
    /// result, not as a wait; only then does the sentence talk about sessions
    /// still to gather.
    pub fn summary(&self) -> String {
        let moved: Vec<String> = self
            .arms()
            .filter_map(|s| Some((s, s.shown_pct()?)))
            .map(|(s, pct)| {
                let mag = pct.abs().round() as i64;
                match (s.stream, pct > 0.0) {
                    (Stream::Turns, true) => format!("{mag}% fewer turns per session"),
                    (Stream::Turns, false) => format!("{mag}% more turns per session"),
                    (st, true) => format!("{mag}% less {}", st.label()),
                    (st, false) => format!("{mag}% more {}", st.label()),
                }
            })
            .collect();
        if !moved.is_empty() {
            return format!("{} with it on", join(&moved));
        }

        // Nothing settled. Say which of the two silences this is: a comparison
        // that ran and found nothing, or one that has not run yet.
        let full = self
            .arms()
            .all(|s| matches!(s.reading(), Reading::NoChange { .. } | Reading::Quiet));
        if full {
            return format!(
                "no change worth measuring on any stream, over {} sessions with it on and {} with it off",
                self.n_on, self.n_off
            );
        }
        if let Some(need) = self
            .arms()
            .filter_map(|s| match s.reading() {
                Reading::Waiting { need_on, need_off } => Some(need_on.max(need_off)),
                _ => None,
            })
            .max()
        {
            return format!(
                "still measuring: needs about {need} more {} on the thinner side",
                sessions(need)
            );
        }

        // Mixed: every arm was compared, some settled on "no change" and some
        // are too noisy. Lead with what was established. "Nothing has settled"
        // would throw away the half of the comparison that did.
        let mut bound: f64 = 0.0;
        let mut flat: Vec<String> = Vec::new();
        let mut noisy: Vec<String> = Vec::new();
        for s in self.arms() {
            match s.reading() {
                Reading::NoChange { bound: b } => {
                    bound = bound.max(b);
                    flat.push(s.stream.label().to_string());
                }
                Reading::Inconclusive => noisy.push(s.stream.label().to_string()),
                _ => {}
            }
        }
        let mut parts = Vec::new();
        if !flat.is_empty() {
            parts.push(format!(
                "no change bigger than {}% on {}",
                ((bound * 100.0).ceil() as u64).max(1),
                join(&flat)
            ));
        }
        if !noisy.is_empty() {
            parts.push(format!("{} still too noisy to call", join(&noisy)));
        }
        if parts.is_empty() {
            // Every arm quiet: there was never anything here to divide.
            return "every stream is too small to compare on this workload".to_string();
        }
        parts.join("; ")
    }
}

impl SaverAttribution {
    /// The thing about this saver's measurement a reader would otherwise miss.
    ///
    /// [`Self::summary`] says what the comparison found. This says what the
    /// finding does not cover, and it is the half that changes decisions: a
    /// per-turn saving on a saver that needs more turns is not a saving, and a
    /// figure resting on a handful of sessions on one side is not the same
    /// claim as one resting on thousands.
    ///
    /// `None` is the common and correct answer for a settled saver with a full
    /// comparison: there is nothing the summary is hiding.
    pub fn caveat(&self) -> Option<String> {
        let settled = self.streams.iter().any(|s| s.shown_pct().is_some());
        match self.turns.reading() {
            // Fewer tokens per turn, more turns. Every other figure on this
            // saver divides by the number that went up.
            Reading::Delta if self.turns.delta.is_some_and(|d| d < 0.0) => {
                return Some(format!(
                    "each session took about {}% more turns with it on, and every other figure here is per turn",
                    self.turns.shown_pct().map(|p| p.abs().round()).unwrap_or(0.0)
                ));
            }
            // The failure mode a live 4B walked into twice: an unmeasurable
            // turn count read as "no increase in turns".
            Reading::Delta | Reading::NoChange { .. } => {}
            _ if settled => {
                return Some(
                    "the turn count could not be compared, so a per-turn saving here is not proof of a saving overall"
                        .to_string(),
                )
            }
            _ => {}
        }
        // Ratios this lopsided are still honest (both arms clear MIN_GROUP), but
        // "thousands of sessions" and "eleven sessions" are not the same
        // sentence, and the badge does not distinguish them.
        let (weak, strong) = (self.n_on.min(self.n_off), self.n_on.max(self.n_off));
        if settled && weak * 20 < strong {
            let side = if self.n_on < self.n_off { "on" } else { "off" };
            return Some(format!(
                "one side of the comparison is thin: {weak} {} with it {side}, against {strong} the other way",
                sessions(weak)
            ));
        }
        None
    }
}

/// "a", "a and b", "a, b and c" — the list voice the summaries are written in.
fn join(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

impl SaverAttribution {

    /// Whether this saver has been **shown to do nothing worth measuring** —
    /// as opposed to merely not having been shown to do something.
    ///
    /// The distinction is the entire safety argument for folding one saver set's
    /// sessions in with another's ([`headline_with_map`]). A saver with no OFF
    /// sessions has an unknown effect, and unknown is not null: pooling across
    /// an era that differs by it would let that saver's real effect land on the
    /// headline. So this demands three things:
    ///
    /// * **Power.** [`MIN_GROUP`] sessions on each arm, the same bar every other
    ///   claim in this module clears.
    /// * **Equivalence, not non-significance.** The whole 90% interval inside
    ///   ±[`NULL_BAND`], so "no effect" is a measurement rather than an absence
    ///   of one.
    /// * **Every stream the multiplier is built from.** Input, output, cache
    ///   write and *turns*. Cache read is excluded deliberately, matching the
    ///   price-weighted spend the `×` is computed on (`docs/measurement.md`);
    ///   turns is included because it is that spend's denominator.
    ///
    /// A stream too quiet to have a ratio at all counts as null rather than as
    /// unknown, but only when **both** arms are under that arm's
    /// [`Stream::min_denom`]. That is the case the floors exist for: at 2 tokens
    /// a turn (or one turn a session) there is no percentage worth computing
    /// and halving it saves five tokens. A tiny OFF median against a large ON
    /// one is the opposite situation - the one that printed `-20071%` - and
    /// stays disqualifying.
    pub fn is_negligible(&self) -> bool {
        if self.n_on < MIN_GROUP || self.n_off < MIN_GROUP {
            return false;
        }
        // Turns first: it is the one that can invert every other stream's sign.
        let mut checked = vec![&self.turns];
        for want in [Stream::Input, Stream::Output, Stream::CacheCreate] {
            match self.streams.iter().find(|s| s.stream == want) {
                Some(s) => checked.push(s),
                // A stream the caller never computed is not a stream we may
                // assume nothing happened on.
                None => return false,
            }
        }
        checked.iter().all(|s| {
            // The floor has to be the arm's own: `MIN_RATE_DENOM` applied to
            // turns per session is above a normal session's turn count, so every
            // turn regression short of ten turns a side passed as proven-null.
            let floor = s.stream.min_denom();
            if s.median_off < floor && s.median_on < floor {
                return true;
            }
            match s.ci {
                Some((lo, hi)) => lo >= -NULL_BAND && hi <= NULL_BAND,
                None => false,
            }
        })
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

/// One session's tag for a single saver, with what it takes to know whether it
/// is comparable to another one.
#[derive(Debug, Clone)]
struct SaverRow {
    enabled: bool,
    source: String,
    /// Every OTHER saver the SCHEDULER controls in this session was on. Savers
    /// the user pinned off by hand don't count against it: they are off in both
    /// arms of this saver's contrast, so they are a constant, not a confounder.
    /// Vacuously true when this is the only saver installed, which is why the
    /// single-saver case is unaffected by the isolation rule.
    others_on: bool,
    rates: SessionRates,
}

/// One session, classified, with what it takes to know whether it is comparable
/// to another one.
#[derive(Debug, Clone)]
struct ClassifiedSession {
    group: SessionGroup,
    rates: SessionRates,
    /// The savers that were ON, canonical (`"caveman+rtk"`). Two full-on sessions
    /// only describe the same treatment when this matches. Nothing else in a
    /// session records which savers existed at the time, so without it a
    /// "rtk + caveman on" era and a later "rtk on" era pool into one median that
    /// describes neither.
    on_set: String,
    /// For picking the CURRENT set. `None` sorts oldest, which is NOT inherently
    /// the safe end: it means an undated era loses to a dated one, so if the
    /// user's live era were the undated one, an abandoned setup would win the
    /// vote. That is only harmless under an assumption worth stating, namely that
    /// sessions do carry timestamps: `started_at` is `None` only when not one
    /// line of the log was dated, which Claude Code and Codex never produce.
    started_at: Option<String>,
}

/// Why the headline carries no multiplier, when it doesn't. A bare
/// `Option<f64>` cannot tell "still gathering data" from "the data is in, but the
/// estimate is not trustworthy enough to publish", and the two want different
/// words in the UI: the first is a session count, the second is a reason. Carried
/// so the sub-line stops always blaming sample size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiplierState {
    /// A multiplier was computed and lives in [`Headline::multiplier`].
    Shown,
    /// Neither side had a priced spend rate to compare yet: nothing to divide.
    NoData,
    /// A rate was computable, but the savers came out *behind* a non-measured
    /// (observational) baseline (`m < 1`). Withheld on purpose: heavier recent
    /// work is a likelier cause than a real regression, and a randomized
    /// comparison is exempt. The data is in; the number is deliberately hidden.
    WithheldCostMore,
}

/// How fast one arm of the experiment is filling up.
///
/// Exists because "measuring" with no end in sight is indistinguishable from
/// broken. Both arms have a hard reason they are where they are - the ON arm
/// restarts whenever the saver set changes, the baseline only fills on holdout
/// slots - and a user can only judge whether to wait if they are told when the
/// count started and roughly how much longer it has to run.
///
/// The clock is the **data's own**: first to last session in the arm, never
/// `Utc::now()`. That keeps this a pure function of the store, so the same
/// database gives the same answer in a test, in the CLI, and in the app, and a
/// machine left idle for a week does not report a pace that silently decayed.
#[derive(Debug, Clone, PartialEq)]
pub struct Pace {
    /// RFC3339 timestamp of the arm's first session — when this count started.
    pub since: String,
    /// Sessions per day across the arm's span. `None` when the arm spans no
    /// measurable time (one session, or several inside the same instant): a
    /// rate over a zero window is a division by zero dressed up as an estimate.
    pub per_day: Option<f64>,
}

impl Pace {
    /// Days still to run before this arm reaches `target`, at the pace observed
    /// so far. `None` when the target is already met or there is no pace to
    /// extrapolate from — the caller then says "not yet" without inventing a
    /// date.
    pub fn days_to(&self, have: usize, target: usize) -> Option<f64> {
        let per_day = self.per_day?;
        if have >= target || per_day <= 0.0 {
            return None;
        }
        Some((target - have) as f64 / per_day)
    }
}

/// Build a [`Pace`] from an arm's session timestamps.
///
/// `n - 1` intervals over `n` sessions, not `n`: five sessions spread over four
/// days is a pace of one a day, and dividing by the count instead would report
/// 1.25 and promise a finish line that arrives late every time.
fn pace_of<'a>(dates: impl Iterator<Item = &'a str>) -> Option<Pace> {
    let mut sorted: Vec<&str> = dates.collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_unstable();
    // Lexicographic ordering is the real ordering here only because these are
    // RFC3339 UTC strings straight out of the session logs; parse to compare.
    let first = chrono::DateTime::parse_from_rfc3339(sorted[0]).ok()?;
    let last = chrono::DateTime::parse_from_rfc3339(sorted[sorted.len() - 1]).ok()?;
    let days = (last - first).num_seconds() as f64 / 86_400.0;
    Some(Pace {
        since: sorted[0].to_string(),
        per_day: if days > 0.0 && sorted.len() > 1 {
            Some((sorted.len() - 1) as f64 / days)
        } else {
            None
        },
    })
}

/// Which side of the experiment is still short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingArm {
    /// Sessions running the user's current saver set.
    On,
    /// All-off sessions to compare it against.
    Baseline,
}

/// What the headline is still waiting for, in enough detail for a surface to
/// tell the user why, what for, and roughly how much longer. One shared answer
/// so the CLI, the app and the report cannot drift into three different stories
/// about the same database.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    pub arm: WaitingArm,
    pub have: usize,
    pub need: usize,
    /// When this arm's count started. For [`WaitingArm::On`] that is the moment
    /// the current saver set came together — the restart the user never sees.
    pub since: Option<String>,
    /// Days left at the pace observed so far, `None` when there is no pace to
    /// extrapolate from (a single session, or a fixed pre-install baseline).
    pub days_left: Option<f64>,
}

/// The dashboard headline.
#[derive(Debug, Clone)]
pub struct Headline {
    pub baseline: HeadlineBaseline,
    pub n_full_on: usize,
    pub n_baseline: usize,
    /// The best badge this headline can back, both sides considered. **This is
    /// the authority on the label**: `measured` requires
    /// `ceiling == Badge::Measured` on top of the usual sample bar. Callers must
    /// not re-derive it from `baseline` alone, which is how the manual-on era bug
    /// reached the dashboard even though the core had already computed the right
    /// answer.
    pub ceiling: Badge,
    /// Whether the full-on side is backed by **randomized** sessions (every
    /// saver on because Piggy's scheduler said so). False once the ON group has
    /// to lean on manually-pinned sessions, which are observational however many
    /// of them there are. Carried so the CLI can say *why* a figure is only
    /// estimated.
    pub on_randomized: bool,
    /// How many of the ON arm's sessions are the randomized ones: current saver
    /// set, every saver on because the scheduler said so, before any
    /// observational pooling.
    ///
    /// Carried because `on_randomized` alone cannot tell a surface which of two
    /// very different situations it is in, and they want opposite words. Zero
    /// means nothing is rotating and waiting fixes nothing. Non-zero but under
    /// [`MIN_GROUP`] means rotation IS running and this is the count that is
    /// still filling — the only honest progress figure for that case, since
    /// `n_full_on` is the pooled total and can sit in the thousands while the
    /// arm that decides the badge holds five.
    pub n_full_on_randomized: usize,
    /// Whether the baseline is a **clean** all-off holdout. False when the only
    /// holdouts available had a pinned saver running through them, so the
    /// "no savers at all" counterfactual was never actually observed.
    pub baseline_clean: bool,
    /// `median(baseline spend rate) / median(full_on spend rate)` — "lasts N.N×
    /// longer". Price-weighted, hence `estimated`. `None` if not computable.
    pub multiplier: Option<f64>,
    /// Why `multiplier` is `None`, when it is (`Shown` when it is `Some`). Lets a
    /// caller distinguish "not enough sessions yet" from "enough, but the estimate
    /// was withheld as implausible" without re-deriving the gate.
    pub multiplier_state: MultiplierState,
    /// Per-stream measured deltas (full-on vs baseline), shown before the ×.
    pub streams: Vec<StreamStat>,
    /// Turns per session, on vs off. Not one of `streams`: it is the thing they
    /// are divided by. A negative delta here means the savers made the agent
    /// take MORE turns, which every per-turn figure above is blind to.
    pub turns: StreamStat,
    /// When the ON arm's count started and how fast it is filling.
    ///
    /// `since` is the load-bearing half: the ON arm is scoped to the saver set
    /// the user runs *now* (see `live_set`), so installing, removing, or
    /// hand-toggling one saver silently restarts it at zero. Without this the
    /// screen could only ever say "4 of 10" and never "…since your set changed
    /// on Tuesday", which is the difference between an experiment running and
    /// one that looks stuck.
    pub on_pace: Option<Pace>,
    /// When the baseline arm's first session landed and how fast it is filling.
    /// Holdouts are ~1 session in 10 by design, so a new user's baseline is the
    /// slow arm and deserves the same honesty about how long it will take.
    pub baseline_pace: Option<Pace>,
    /// Sessions the ON arm gained from earlier saver sets that differ from the
    /// current one only by savers measured as null. Zero when the live era stood
    /// on its own, which is the normal case and the preferred one.
    pub n_carried: usize,
    /// The null savers that made that fold-in legal, so a surface can name them.
    /// Empty whenever `n_carried` is zero.
    pub carried_savers: Vec<String>,
}

impl Headline {
    /// The arm still short of [`MIN_GROUP`], if either is.
    ///
    /// Reports the arm that needs the most sessions, ties going to the baseline:
    /// that is the one the user cannot hurry, so it is the honest thing to quote
    /// a wait against. `None` once both arms are full — at which point the
    /// headline is held up by something other than sample size, and saying
    /// "still gathering" would be a lie the caller must not tell.
    pub fn waiting(&self) -> Option<Waiting> {
        let on_short = MIN_GROUP.saturating_sub(self.n_full_on);
        let base_short = MIN_GROUP.saturating_sub(self.n_baseline);
        if on_short == 0 && base_short == 0 {
            return None;
        }
        let (arm, have, pace) = if on_short > base_short {
            (WaitingArm::On, self.n_full_on, self.on_pace.as_ref())
        } else {
            (WaitingArm::Baseline, self.n_baseline, self.baseline_pace.as_ref())
        };
        Some(Waiting {
            arm,
            have,
            need: MIN_GROUP,
            since: pace.map(|p| p.since.clone()),
            days_left: pace.and_then(|p| p.days_to(have, MIN_GROUP)),
        })
    }
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

    /// Every session's snapshot for `saver_id`, paired with its rates and with
    /// whether every OTHER saver in that session was on. Sessions with no tag for
    /// this saver are omitted.
    ///
    /// `others_on` is what keeps the comparison about THIS saver. Rotation turns
    /// X off in two different kinds of slot: the single-off slot (X off,
    /// everything else still running) and the holdout (X off and everything else
    /// off too). They are different treatments, and only the first isolates X.
    ///
    /// A saver the USER switched off is a different thing again, and the same
    /// exception [`Store::classified_sessions`] already makes for
    /// `any_scheduler_disabled` applies here: `rotation::controlled_savers` drops
    /// a hand-toggled saver from rotation, so it is off in every session of that
    /// era — X's ON arm and X's OFF arm alike. Holding it against isolation
    /// doesn't protect the contrast, it deletes it: one saver pinned off by hand
    /// made `others_on` false for every session and every saver's group went
    /// empty, so the per-saver table read "not enough data yet" forever at any
    /// session count. Measured on a real profile: sweep had 245 on / 45 off
    /// randomized sessions and the table showed 52 / 0.
    fn saver_group_rows(
        &self,
        saver_id: &str,
        rate_map: &std::collections::HashMap<String, SessionRates>,
    ) -> Result<Vec<SaverRow>> {
        // Every row, not just this saver's: the other savers' states in the same
        // session are exactly the thing we need.
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, saver_id, enabled, source FROM session_savers")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
                r.get::<_, String>(3)?,
            ))
        })?;
        // (this saver's tag, whether every other saver was on)
        let mut per_session: std::collections::HashMap<String, (Option<(bool, String)>, bool)> =
            std::collections::HashMap::new();
        for row in rows {
            let (sid, sid_saver, enabled, src) = row?;
            let e = per_session.entry(sid).or_insert((None, true));
            if sid_saver == saver_id {
                e.0 = Some((enabled, src));
            } else if !enabled && src != source::MANUAL {
                // Scheduler-driven off only. A `manual` off is constant across
                // both arms; a `rotation`/`holdout`/`pre_install` off is not.
                e.1 = false;
            }
        }
        let mut out = Vec::new();
        for (sid, (tag, others_on)) in per_session {
            let (Some((enabled, source)), Some(rates)) = (tag, rate_map.get(&sid)) else {
                continue;
            };
            out.push(SaverRow {
                enabled,
                source,
                others_on,
                rates: rates.clone(),
            });
        }
        // Grouping happens in a HashMap, whose order Rust re-randomizes per
        // instance, and `bootstrap_deltas` resamples BY INDEX: leave the order to
        // the map and the same seed produces a different CI on every call. Sort
        // by session id so the bootstrap is reproducible, which is the whole
        // point of seeding it.
        out.sort_by(|a, b| a.rates.session_id.cmp(&b.rates.session_id));
        Ok(out)
    }

    /// The session-level A/B classification for every tagged, non-subagent
    /// session, paired with its rates.
    fn classified_sessions(
        &self,
        rate_map: &std::collections::HashMap<String, SessionRates>,
    ) -> Result<Vec<ClassifiedSession>> {
        // Pull every tag, group by session in Rust. `started_at` comes along so
        // the caller can tell which saver set is the CURRENT one: "everything on"
        // means different things before and after an install or a toggle.
        let mut stmt = self.conn.prepare(
            "SELECT ss.session_id, ss.saver_id, ss.enabled, ss.source, s.started_at
             FROM session_savers ss
             LEFT JOIN sessions s ON s.session_id = ss.session_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        /// What one session's saver tags add up to. Was a tuple; grew a field per
        /// bug until nobody could read it.
        struct Facts {
            any_holdout: bool,
            any_pre_install: bool,
            any_enabled: bool,
            /// The savers that were ON. Two full-on sessions only describe the
            /// same treatment if this matches: a session carries no other record
            /// of which savers existed at the time, so without it the ON group
            /// silently pools "rtk + caveman on" with "rtk on" as though they
            /// were one thing. Kept in step with `any_enabled` by construction,
            /// since both come off the same row's `enabled`.
            on_set: std::collections::BTreeSet<String>,
            started_at: Option<String>,
            /// A saver was off because the SCHEDULER turned it off, i.e. this is a
            /// single-off rotation slot. Deliberately not "any saver was off": a
            /// saver the user switched off by hand is off in every session
            /// forever, and treating that as a single-off slot classified every
            /// one of their sessions Mixed and killed the headline for good.
            any_scheduler_disabled: bool,
            /// Every saver that was actually ON came from Piggy's scheduler
            /// rather than the user. Deliberately scoped to the ON set, for the
            /// same reason `any_scheduler_disabled` is: a saver the user
            /// switched off by hand is not in the contrast at all (it is off in
            /// both arms), so its provenance cannot decide whether the contrast
            /// was randomized. Reading every row instead meant one saver
            /// hand-disabled once, ever, made every later session observational
            /// for good and put `measured` permanently out of reach no matter
            /// how long rotation ran.
            on_set_randomized: bool,
        }
        impl Facts {
            fn new() -> Self {
                Facts {
                    any_holdout: false,
                    any_pre_install: false,
                    any_enabled: false,
                    on_set: std::collections::BTreeSet::new(),
                    started_at: None,
                    any_scheduler_disabled: false,
                    on_set_randomized: true,
                }
            }
        }

        let mut per_session: std::collections::HashMap<String, Facts> =
            std::collections::HashMap::new();
        for row in rows {
            let (sid, saver_id, enabled, src, started_at) = row?;
            let f = per_session.entry(sid).or_insert_with(Facts::new);
            f.any_holdout |= src == source::HOLDOUT;
            f.any_pre_install |= src == source::PRE_INSTALL;
            if enabled {
                f.any_enabled = true;
                f.on_set.insert(saver_id);
                f.on_set_randomized &= is_randomized(&src);
            } else if is_randomized(&src) {
                f.any_scheduler_disabled = true;
            }
            if f.started_at.is_none() {
                f.started_at = started_at;
            }
        }
        let mut out = Vec::new();
        for (sid, f) in per_session {
            let Some(rates) = rate_map.get(&sid) else {
                continue;
            };
            let group = if f.any_holdout {
                // A holdout is only the all-off counterfactual if it was actually
                // all off. `controlled_savers` drops manually-toggled savers from
                // rotation, so a pinned saver rides straight through the holdout
                // slot and the "every saver off" baseline still has one running.
                if f.any_enabled {
                    SessionGroup::HoldoutContaminated
                } else {
                    SessionGroup::Holdout
                }
            } else if f.any_pre_install {
                SessionGroup::PreInstall
            } else if f.any_enabled && !f.any_scheduler_disabled {
                // Full-on means every saver the SCHEDULER is running is on. A
                // saver the user switched off by hand is not one of those: they
                // opted it out of their setup, the same as never installing it,
                // and it is off in the holdout too, so it drops out of the
                // contrast rather than poisoning it. Testing "no saver is off at
                // all" instead meant one hand-switched-off saver classified every
                // session Mixed and the headline read "measuring" forever, at any
                // session count.
                //
                // `any_enabled` is load-bearing, not a formality. Switch EVERY
                // saver off by hand and there are no scheduler-disabled rows at
                // all, so `!any_scheduler_disabled` is vacuously true and a
                // session running NOTHING would count as full-on. The headline
                // would then publish a multiplier off pure pre/post drift, to a
                // user with every saver off. "Everything else on" needs there to
                // be an everything else.
                //
                // Provenance still governs the claim. A saver the user pinned ON
                // makes this observational: Piggy is not rotating it, and the
                // toggle splits history into a before and an after that the
                // scheduler did not randomize across. A saver pinned OFF does
                // not, on the same grounds that keep it out of the full-on test
                // two paragraphs up - it is off in the holdout too, so it is a
                // constant across the contrast rather than a confounder in it.
                if f.on_set_randomized {
                    SessionGroup::FullOn
                } else {
                    SessionGroup::FullOnObservational
                }
            } else {
                SessionGroup::Mixed
            };
            out.push(ClassifiedSession {
                group,
                rates: rates.clone(),
                on_set: f.on_set.into_iter().collect::<Vec<_>>().join("+"),
                started_at: f.started_at,
            });
        }
        // Same reason as `saver_group_rows`: this is grouped in a HashMap and
        // `bootstrap_deltas` resamples by index, so an unsorted vector makes the
        // headline's CI differ between two calls with the same seed.
        out.sort_by(|a, b| a.rates.session_id.cmp(&b.rates.session_id));
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

/// The delta `1 - median(on)/median(off)`, or `None` when there is nothing to
/// compare: an empty ON group, or an OFF group whose median is below `floor`
/// (a stream the baseline doesn't meaningfully use). `floor` is the arm's own
/// [`Stream::min_denom`], never a shared constant: see [`MIN_TURNS_DENOM`].
fn delta_of(on: &[f64], off: &[f64], floor: f64) -> Option<f64> {
    // `median(&[])` is 0.0, so an empty ON group computes `1 - 0/mo == 1.0`: a
    // nominal 100% saving conjured out of no data at all. Downstream gates do
    // already suppress it (MIN_GROUP in `stream_stat`, and the bootstrap, which
    // cannot resample an empty group), so this is defense in depth rather than a
    // live leak. It is still worth refusing at the source: the worst number this
    // app could print should not exist as a value at all, waiting for a refactor
    // to move a gate.
    //
    // Note the guard is emptiness, NOT `median(on) == 0.0`: ON sessions that
    // genuinely used none of a stream really are a 100% reduction, and that is a
    // real figure worth showing.
    if on.is_empty() {
        return None;
    }
    let mo = median(off);
    if mo < floor {
        return None;
    }
    Some(1.0 - median(on) / mo)
}

/// Bootstrap the sorted delta distribution by resampling both groups with
/// replacement. Deterministic given `seed`. Returns `None` if either group is
/// empty or every resample hit a degenerate off-median.
fn bootstrap_deltas(on: &[f64], off: &[f64], seed: u64, floor: f64) -> Option<Vec<f64>> {
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
        // Same floor as `delta_of`: a resample that lands on a near-zero
        // denominator would widen the CI with ratios the point estimate refuses
        // to compute.
        if mo < floor {
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
    let floor = stream.min_denom();
    let delta = delta_of(on, off, floor);
    let deltas = bootstrap_deltas(on, off, seed, floor);
    // Displayed CI: the spec-mandated 90%.
    let ci = deltas.as_ref().map(|d| ci_at(d, CI_ALPHA));
    // Gate CI: family-corrected, so the *family-wise* rate stays near nominal.
    let gate_ci = deltas
        .as_ref()
        .map(|d| ci_at(d, CI_ALPHA / STREAM_FAMILY as f64));
    let enough = on.len() >= MIN_GROUP && off.len() >= MIN_GROUP;
    let significant = gate_ci.map(ci_is_significant).unwrap_or(false);
    // A saver can cut a stream by at most 100% (delta -> 1). An *observational*
    // (estimated) comparison showing the treatment using >100% MORE (delta <= -1)
    // is the pre-install workload mix talking, not the saver, so it must not wear
    // the "estimated" badge - it stays "measuring" until a real contrast exists.
    // The randomized (measured) path is exempt: a genuine holdout may show a large
    // increase and has to surface it honestly.
    let plausible = ceiling == Badge::Measured || delta.is_some_and(|d| d > -1.0);
    let badge = if enough && delta.is_some() && significant && plausible {
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

/// Raw turn counts, deliberately **unnormalised**.
///
/// Every other figure in this module is tokens per assistant turn, which makes
/// the turn count a free denominator: a saver that needs more turns to finish
/// the same job divides its tokens by a bigger number and scores as a win on
/// all four streams while costing the user more in total. Terser output and
/// compressed tool results both invite exactly that ("say less" and "show less"
/// are both invitations to ask again), so it is the likeliest failure mode in
/// the catalogue rather than a hypothetical one.
///
/// Sign convention matches the streams: `1 - median_on/median_off`, so more
/// turns with the saver on comes out negative and reads as a regression.
fn turn_vectors<'a>(sessions: impl Iterator<Item = &'a SessionRates>) -> Vec<f64> {
    sessions
        .filter(|s| s.turns > 0)
        .map(|s| s.turns as f64)
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
    // Nothing randomized to stand on, so whatever comes back is observational at
    // best. Without this, `(empty, empty)` satisfies `observational.is_empty()`
    // and reports `Measured` over zero sessions, which prints a "measured"
    // heading above an empty table.
    if randomized.is_empty() {
        return (observational, Badge::Estimated);
    }
    if randomized.len() >= MIN_GROUP || observational.is_empty() {
        (randomized, Badge::Measured)
    } else {
        let mut pooled = randomized;
        pooled.extend(observational);
        (pooled, Badge::Estimated)
    }
}

/// Pick the headline's baseline from the clean and contaminated holdouts.
///
/// Deliberately NOT `pick_group`. That helper pools its two groups when the
/// preferred one is thin, which is sound because its callers hand it groups that
/// are the same treatment state and differ only in provenance: pooling buys
/// sample size for one estimand, and the `estimated` cap prices the confounding.
/// (The full-on side upholds that by scoping to one saver set before splitting;
/// see `live_set` in `headline_with_map`.)
///
/// The two groups here are not the same treatment state. They are the presence
/// or absence of the very counterfactual the headline names. A clean holdout is
/// "every
/// saver off"; a contaminated one is "every saver off except the one you pinned,
/// which kept running". A median across the union estimates neither population:
/// it tracks whichever arm is larger, so the headline would move with the mix
/// rather than with the savers. Worse, pooling would manufacture MIN_GROUP
/// eligibility out of a second population: 5 clean holdouts alone correctly show
/// no number at all, and 5 clean + 15 contaminated would show one, which is the
/// exact gate MIN_GROUP exists to enforce.
///
/// So: prefer the clean holdouts when they can stand alone, otherwise use the
/// contaminated ones alone and cap at `estimated`. Nothing is discarded, the
/// number still shows, and it always describes one coherent population. Note the
/// contaminated arm answers a narrower question than the headline's "versus no
/// savers at all", which is why it can never be `measured`.
fn pick_baseline(
    clean: Vec<SessionRates>,
    contaminated: Vec<SessionRates>,
) -> (Vec<SessionRates>, Badge) {
    if clean.len() >= MIN_GROUP || contaminated.is_empty() {
        (clean, Badge::Measured)
    } else {
        (contaminated, Badge::Estimated)
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

    // Isolate this saver: both arms hold every OTHER saver on, so the only thing
    // that differs between them is this one.
    //
    // Rotation turns X off in two different slots, and they are not the same
    // treatment: the single-off slot (X off, everything else running) and the
    // holdout (X off and everything else off too). Pooling them compared
    // "X on, others on" against a 50/50 mix of "others on" and "others off", so
    // the other savers' savings landed on X. At shipping defaults that mix is
    // exactly 50/50 for every user by construction, and its weight was set by
    // `holdout_fraction`: a measurement-cadence dial silently moved every saver's
    // reported percentage. Measured: a saver whose true effect was 50% reported
    // 71% once 30 holdouts existed, badged Measured.
    //
    // `others_on` is vacuously true when this is the only saver installed, where
    // holdout and single-off really are the same state, so the single-saver case
    // is unchanged.
    let isolated = |r: &&SaverRow| r.others_on;
    let on_randomized: Vec<SessionRates> = rows
        .iter()
        .filter(isolated)
        .filter(|r| r.enabled && is_randomized(&r.source))
        .map(|r| r.rates.clone())
        .collect();
    let on_observational: Vec<SessionRates> = rows
        .iter()
        .filter(isolated)
        .filter(|r| r.enabled && !is_randomized(&r.source))
        .map(|r| r.rates.clone())
        .collect();
    let off_randomized: Vec<SessionRates> = rows
        .iter()
        .filter(isolated)
        .filter(|r| !r.enabled && is_randomized(&r.source))
        .map(|r| r.rates.clone())
        .collect();
    let off_observational: Vec<SessionRates> = rows
        .iter()
        .filter(isolated)
        .filter(|r| !r.enabled && !is_randomized(&r.source))
        .map(|r| r.rates.clone())
        .collect();
    // Counted over the rows this saver's number could actually rest on, so the
    // footnote cannot advertise sessions the comparison excluded.
    let mut off_by_source: BTreeMap<String, usize> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.others_on) {
        if !r.enabled {
            *off_by_source.entry(r.source.clone()).or_insert(0) += 1;
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

    // The denominator, as its own arm. Same machinery and same gate as the
    // headline's: a saver that needs more turns to do the same job has to be
    // able to say so somewhere, and until now the per-saver path had no slot
    // for it at all.
    let turns = stream_stat(
        Stream::Turns,
        &turn_vectors(on.iter()),
        &turn_vectors(off_used.iter()),
        ceiling,
        seed ^ 0x3C6E_F372,
    );

    Ok(SaverAttribution {
        saver_id: saver_id.to_string(),
        n_on: on.len(),
        n_off: off_used.len(),
        off_by_source,
        streams,
        turns,
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

    let is_full_on =
        |g: SessionGroup| g == SessionGroup::FullOn || g == SessionGroup::FullOnObservational;

    // Which saver set is the user actually running? The one their most recent
    // full-on session used. A session records no saver set of its own, so without
    // this the ON group pools every era the setup has ever been in: install a
    // saver, uninstall one, hand-toggle one, and "everything on" quietly means
    // something different on either side of that moment. The pooled median then
    // tracks the era MIX rather than the savers, and the headline swings with it
    // even when no saver's behaviour changed at all.
    //
    // Recency, not majority: "your plan lasts N.N x longer" is a claim about the
    // setup you have now, so a bigger pile of sessions from a configuration you
    // abandoned should not outvote it. Deliberately not read from PiggyState:
    // the classification stays a pure function of what the sessions recorded.
    // The `session_id` tiebreak is not decoration. `max_by` returns the LAST
    // maximum and `classified` comes out of a HashMap, whose order Rust
    // re-randomizes per instance, so on equal timestamps the winner changed
    // between two refreshes in one process: the same database backing two
    // different `measured` numbers. Real logs carry millisecond timestamps and do
    // not collide, but "unreachable today" is not a reason to leave the headline
    // deciding itself by hash order.
    let live_set: Option<&str> = classified
        .iter()
        .filter(|c| is_full_on(c.group))
        .max_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.rates.session_id.cmp(&b.rates.session_id))
        })
        .map(|c| c.on_set.as_str());

    // Which savers separate the live set from an abandoned one.
    //
    // Scoping the ON arm to one saver set is right, and it has a cost nobody was
    // paying attention to: trying a saver restarts the count at zero, so a user
    // who evaluates savers - the entire point of this app - can sit at "4 of 10"
    // indefinitely while thousands of usable sessions go unread. This recovers
    // them in the one case where it is sound: when the saver that separates two
    // eras has been MEASURED to do nothing (`is_negligible`), those eras are the
    // same treatment wearing different names.
    let members = |set: &str| -> std::collections::BTreeSet<String> {
        set.split('+').filter(|s| !s.is_empty()).map(str::to_string).collect()
    };
    let live_members = members(live_set.unwrap_or(""));
    let mut candidates: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for c in classified.iter().filter(|c| is_full_on(c.group)) {
        if Some(c.on_set.as_str()) == live_set {
            continue;
        }
        candidates.extend(
            live_members
                .symmetric_difference(&members(&c.on_set))
                .cloned(),
        );
    }
    // Tested once each, and only for savers that actually separate two eras -
    // usually one or two, not the whole catalogue. A saver with no OFF sessions
    // fails this, which is what keeps an uninstalled saver of unknown effect from
    // dragging its era in behind it.
    let negligible: std::collections::BTreeSet<String> = candidates
        .iter()
        .filter(|id| {
            attribute_with_map(store, rate_map, id, seed ^ 0x5DEE_CE66)
                .map(|a| a.is_negligible())
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let foldable = |on_set: &str| -> bool {
        !negligible.is_empty()
            && live_members
                .symmetric_difference(&members(on_set))
                .all(|s| negligible.contains(s))
    };

    // `live` is the era the user runs now; `carried` is every other era that
    // differs from it only by a proven-null saver.
    let full_on_of = |want: SessionGroup, live: bool| -> Vec<SessionRates> {
        classified
            .iter()
            .filter(|c| c.group == want)
            .filter(|c| {
                let is_live = Some(c.on_set.as_str()) == live_set;
                if live {
                    is_live
                } else {
                    !is_live && foldable(&c.on_set)
                }
            })
            .map(|c| c.rates.clone())
            .collect()
    };
    let full_on_randomized = full_on_of(SessionGroup::FullOn, true);
    // Captured before `pick_group` pools this side with the observational rows:
    // afterwards the count is gone, and it is the only number that says how far
    // the measured-eligible arm has actually got.
    let n_full_on_randomized = full_on_randomized.len();
    let full_on_observational = full_on_of(SessionGroup::FullOnObservational, true);
    let carried_randomized = full_on_of(SessionGroup::FullOn, false);
    let carried_observational = full_on_of(SessionGroup::FullOnObservational, false);

    // Both flavours, because the pace is only ever read while the arm is short,
    // and a short arm is exactly the case where `pick_group` pools the two.
    let on_pace = pace_of(
        classified
            .iter()
            .filter(|c| is_full_on(c.group) && Some(c.on_set.as_str()) == live_set)
            .filter_map(|c| c.started_at.as_deref()),
    );

    // The baseline is deliberately NOT scoped the same way, and the reason is a
    // trade rather than a symmetry. Every holdout is "nothing on", so the
    // treatment really is identical across eras, but that answers the wrong
    // question: the hazard here is era drift, which randomization only balances
    // WITHIN an era. A baseline spanning eras therefore carries the same drift
    // the ON side was just scoped against, and the multiplier moves with the
    // holdout era mix.
    //
    // It stays unscoped anyway because holdouts are ~1 in 10 sessions: scope them
    // to one era and MIN_GROUP becomes unreachable for most users, so the honest
    // headline would be "measuring" more or less permanently. Sample viability
    // beats era purity on this arm. Filed rather than hidden, and said out loud
    // here rather than papered over with "there is nothing to scope".
    let of_group = |want: SessionGroup| -> Vec<SessionRates> {
        classified
            .iter()
            .filter(|c| c.group == want)
            .map(|c| c.rates.clone())
            .collect()
    };
    let holdout_clean = of_group(SessionGroup::Holdout);
    let holdout_contaminated = of_group(SessionGroup::HoldoutContaminated);
    let pre_install = of_group(SessionGroup::PreInstall);

    // Prefer a live holdout; fall back to observational pre-install history.
    // A clean holdout is the randomized all-off counterfactual the "N.N× longer"
    // claim is about. A contaminated one (a pinned saver rode through it) is
    // still useful evidence and still gets shown, but the counterfactual it
    // describes was never observed, so it can only back an `estimated` figure.
    // The pre-install baseline is observational for the older reason: it predates
    // Piggy and was never randomized at all.
    // `pick_baseline` chooses the holdout arm (clean, else contaminated) and its
    // ceiling. A holdout only *wins* the baseline once that arm can clear the
    // sample bar (`>= MIN_GROUP`) - the same gate the headline applies downstream,
    // so keying off it here means the holdout takes over exactly when it can back
    // a number, not the instant the first holdout row lands.
    //
    // Until then, keep the observational pre-install estimate instead of dropping
    // to "measuring". One holdout session used to evict a MIN_GROUP-strong
    // pre-install baseline and blank the figure for the whole 1..MIN_GROUP warm-up,
    // so the displayed estimate went backwards (a number, then nothing) as data
    // accrued. The pre-install baseline is only ever `estimated`, never measured,
    // so nothing is over-claimed by showing it a little longer. Only when there is
    // no pre-install history to stand on does the thin holdout carry the headline,
    // so its "N of MIN_GROUP sessions so far" warm-up progress still shows.
    let (used_holdout, holdout_ceiling) = pick_baseline(holdout_clean, holdout_contaminated);
    let (baseline_kind, baseline, baseline_ceiling, baseline_clean) =
        if used_holdout.len() >= MIN_GROUP {
            // `pick_baseline` returns Measured exactly when it took the clean-only
            // branch, so deriving the flag from the ceiling keeps the two from
            // drifting apart later.
            let clean = holdout_ceiling == Badge::Measured;
            (HeadlineBaseline::Holdout, used_holdout, holdout_ceiling, clean)
        } else if !pre_install.is_empty() {
            // Not a holdout at all: "clean holdout" does not apply, and claiming
            // it does would be a field that quietly means something other than
            // its name.
            (HeadlineBaseline::PreInstall, pre_install, Badge::Estimated, false)
        } else if !used_holdout.is_empty() {
            // A thin holdout with no pre-install history to fall back on: carry it
            // so the sub-line shows real warm-up progress rather than an empty
            // state. It is below MIN_GROUP, so the downstream gate keeps the figure
            // at "measuring" - this only preserves the honest session count.
            let clean = holdout_ceiling == Badge::Measured;
            (HeadlineBaseline::Holdout, used_holdout, holdout_ceiling, clean)
        } else {
            (HeadlineBaseline::None, Vec::new(), Badge::Estimated, false)
        };

    // Only a holdout baseline has a pace worth quoting. A pre-install baseline is
    // fixed history: it is as big as it will ever be, and extrapolating it would
    // promise a wait that finishes nothing. The honest answer there is no
    // estimate, which is what `None` says.
    let baseline_pace = match (baseline_kind, baseline_clean) {
        (HeadlineBaseline::Holdout, clean) => {
            let want = if clean {
                SessionGroup::Holdout
            } else {
                SessionGroup::HoldoutContaminated
            };
            pace_of(
                classified
                    .iter()
                    .filter(|c| c.group == want)
                    .filter_map(|c| c.started_at.as_deref()),
            )
        }
        _ => None,
    };

    // The ON side gets the same treatment as the baseline, and as the per-saver
    // path in `attribute_with_map`. A randomized holdout on one side cannot make
    // a manual-on era on the other side measured: randomization is a property of
    // the contrast, not of the off-switch.
    let (live_on, live_ceiling) = pick_group(full_on_randomized, full_on_observational);
    // The live era stands alone whenever it can, exactly like `pick_group`'s
    // randomized side: folding in another era is a fallback for a thin arm, never
    // an upgrade to a healthy one.
    //
    // And when it IS needed, the badge drops to `estimated`. Proving the
    // differing saver null closes one hole - the treatments really are the same -
    // but not the other: two eras are two stretches of calendar, and the work
    // itself drifts between them in ways no saver measurement speaks to. That is
    // the same trade `pick_group` makes for observational rows, priced the same
    // way, so a carried-forward headline shows a number and never calls it
    // measured.
    // Captured before the merge: whether the carried sessions were themselves
    // scheduler-chosen is a fact about their provenance, and it must not get
    // read off the capped badge below (see `on_randomized`).
    let carried_all_randomized = carried_observational.is_empty();
    let carried: Vec<SessionRates> = carried_randomized
        .into_iter()
        .chain(carried_observational)
        .collect();
    let carried_savers: Vec<String> = negligible.iter().cloned().collect();
    let (full_on, on_ceiling, n_carried) = if live_on.len() >= MIN_GROUP || carried.is_empty() {
        (live_on, live_ceiling, 0)
    } else {
        let n = carried.len();
        let mut pooled = live_on;
        pooled.extend(carried);
        (pooled, Badge::Estimated, n)
    };
    // Named only when they actually did something, so a surface can say "counting
    // your older sessions too, because X measured as no change" without inventing
    // a reason on a headline that never needed one.
    let carried_savers = if n_carried > 0 { carried_savers } else { Vec::new() };
    let ceiling = if baseline_ceiling == Badge::Measured && on_ceiling == Badge::Measured {
        Badge::Measured
    } else {
        Badge::Estimated
    };
    // Provenance, NOT the badge. These used to be the same expression, which was
    // safe only while the badge could be capped for exactly one reason. The
    // carry-forward cap broke that: it lowers `on_ceiling` for a calendar
    // argument, not a randomization one, and reading this off it would tell every
    // surface "your savers are pinned by hand" about an arm the scheduler chose
    // every session of - complete with an un-pin button for savers that are not
    // pinned.
    let on_randomized = live_ceiling == Badge::Measured && (n_carried == 0 || carried_all_randomized);
    let n_baseline = baseline.len();

    // Price-weighted "lasts N.N× longer" (estimated).
    let on_spend: Vec<f64> = full_on.iter().filter_map(|s| s.spend_rate()).collect();
    let off_spend: Vec<f64> = baseline.iter().filter_map(|s| s.spend_rate()).collect();
    let (multiplier, multiplier_state) = {
        let mon = median(&on_spend);
        let moff = median(&off_spend);
        if mon > 0.0 && moff > 0.0 {
            let m = moff / mon;
            // An observational (estimated) baseline can suggest a saving but cannot
            // credibly attribute a *cost increase* to the savers: a full-on set that
            // spends more than pre-install history (m < 1) is far likelier heavier
            // recent work than a real regression, and this app fakes no number. Such
            // an estimate stays "measuring" (None -> the headline shows progress, not
            // a figure) until a randomized holdout can prove the sign. A real
            // regression still surfaces once `ceiling == Measured`, which is exempt.
            //
            // The reason is carried out in `multiplier_state`: "withheld as
            // implausible" is a different thing to tell the user than "still
            // gathering", and a bare `None` cannot tell them apart.
            if ceiling != Badge::Measured && m < 1.0 {
                (None, MultiplierState::WithheldCostMore)
            } else {
                (Some(m), MultiplierState::Shown)
            }
        } else {
            (None, MultiplierState::NoData)
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

    // The denominator, measured as its own arm. Same machinery, same CI gate,
    // same badge rules: this is a claim like any other and has to earn itself.
    let turns = stream_stat(
        Stream::Turns,
        &turn_vectors(full_on.iter()),
        &turn_vectors(baseline.iter()),
        ceiling,
        seed ^ 0x7A5C_9E31,
    );

    Ok(Headline {
        baseline: baseline_kind,
        n_full_on: full_on.len(),
        n_baseline,
        ceiling,
        on_randomized,
        n_full_on_randomized,
        baseline_clean,
        multiplier,
        multiplier_state,
        streams,
        turns,
        on_pace,
        baseline_pace,
        n_carried,
        carried_savers,
    })
}
