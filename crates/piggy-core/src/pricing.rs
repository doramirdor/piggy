//! Per-model USD pricing with longest-prefix matching.
//!
//! An embedded table (compiled in via `include_str!`) is merged under an
//! optional user override at `<home>/pricing.json`. Only per-MTok `input` and
//! `output` rates are stored; the cache rates are derived by fixed multipliers:
//! cache read = 0.1x input, 5-minute cache write = 1.25x input, 1-hour cache
//! write = 2x input. A model with no matching entry prices to `None` (never
//! guessed).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::parser::ModelTokens;

const EMBEDDED: &str = include_str!("../data/pricing.json");

const CACHE_READ_MULT: f64 = 0.1;
const CACHE_WRITE_5M_MULT: f64 = 1.25;
const CACHE_WRITE_1H_MULT: f64 = 2.0;

/// Output rate as a multiple of input, used only when a model has no price
/// entry at all.
///
/// Unlike an absolute rate - which this module refuses to guess, on purpose -
/// the RATIO is stable: every Claude model in the embedded table prices output
/// at exactly 5x input, and every GPT model at 8x. That makes a cost-WEIGHTED
/// comparison computable for an unpriced model even though its dollar cost is
/// not, which is what lets the headroom multiplier stay correct while
/// `claude-opus-5` is missing from the table.
const DEFAULT_OUTPUT_RATIO_CLAUDE: f64 = 5.0;
const DEFAULT_OUTPUT_RATIO_OTHER: f64 = 8.0;
const PER_MTOK: f64 = 1_000_000.0;

/// Per-MTok input/output rates for one model.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
}

#[derive(Debug, Deserialize)]
struct PricingFile {
    models: BTreeMap<String, ModelPrice>,
}

/// A loaded, merged pricing table.
#[derive(Debug, Clone)]
pub struct Pricing {
    models: BTreeMap<String, ModelPrice>,
}

impl Pricing {
    /// The embedded table only (no user override). Panics only if the embedded
    /// JSON is malformed, which is a build-time invariant covered by tests.
    pub fn embedded() -> Self {
        let pf: PricingFile =
            serde_json::from_str(EMBEDDED).expect("embedded pricing.json must be valid");
        Pricing { models: pf.models }
    }

    /// Embedded table merged under `<home>/pricing.json` if present. A missing
    /// override file is fine; an unparseable one is reported on stderr and
    /// ignored (embedded prices are used).
    pub fn load(home: &Path) -> Self {
        let mut p = Self::embedded();
        let path = home.join("pricing.json");
        if !path.exists() {
            return p;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<PricingFile>(&s).ok())
        {
            Some(pf) => {
                for (k, v) in pf.models {
                    p.models.insert(k, v);
                }
            }
            None => {
                eprintln!(
                    "warning: {} exists but could not be parsed; using embedded pricing",
                    path.display()
                );
            }
        }
        p
    }

    /// Relative cost of one token of each stream, in **input-token
    /// equivalents**, summed for `t`.
    ///
    /// A unitless weight, not money: it needs no absolute rate, so it works for
    /// a model missing from the table. Use it to compare streams against each
    /// other (what share of spend is cache writes?), never to show dollars.
    pub fn cost_units(&self, model: &str, t: &ModelTokens) -> f64 {
        let ratio = self.output_ratio(model);
        let write_5m = t
            .cache_creation_tokens
            .saturating_sub(t.cache_creation_1h_tokens);
        t.input_tokens as f64
            + t.output_tokens as f64 * ratio
            + write_5m as f64 * CACHE_WRITE_5M_MULT
            + t.cache_creation_1h_tokens as f64 * CACHE_WRITE_1H_MULT
            + t.cache_read_tokens as f64 * CACHE_READ_MULT
    }

    /// Output-to-input price ratio for `model`: exact when priced, otherwise the
    /// family default.
    ///
    /// This module refuses to guess an absolute rate, and that stays true. A
    /// RATIO is a different thing: every Claude model in the table prices output
    /// at exactly 5x input and every GPT model at 8x, so a cost-WEIGHTED
    /// comparison survives an unpriced model even though its dollar cost does
    /// not. That is what keeps the headroom multiplier correct while
    /// `claude-opus-5` is missing from the table.
    pub fn output_ratio(&self, model: &str) -> f64 {
        if let Some(p) = self.price_for(model) {
            if p.input > 0.0 {
                return p.output / p.input;
            }
        }
        if model.starts_with("claude") {
            DEFAULT_OUTPUT_RATIO_CLAUDE
        } else {
            DEFAULT_OUTPUT_RATIO_OTHER
        }
    }

    /// Blended cost weight of one cache-write token for `t`, between the 1.25x
    /// 5-minute and 2.0x 1-hour rates. Falls back to the 5-minute rate when
    /// nothing was written.
    pub fn write_blend(t: &ModelTokens) -> f64 {
        let write_5m = t
            .cache_creation_tokens
            .saturating_sub(t.cache_creation_1h_tokens);
        let total = write_5m + t.cache_creation_1h_tokens;
        if total == 0 {
            return CACHE_WRITE_5M_MULT;
        }
        (write_5m as f64 * CACHE_WRITE_5M_MULT
            + t.cache_creation_1h_tokens as f64 * CACHE_WRITE_1H_MULT)
            / total as f64
    }


    /// Longest matching key that is a prefix of `model` (so date-suffixed ids
    /// like `claude-haiku-4-5-20251001` resolve to `claude-haiku-4-5`).
    pub fn price_for(&self, model: &str) -> Option<ModelPrice> {
        let mut best: Option<(usize, ModelPrice)> = None;
        for (k, v) in &self.models {
            if model.starts_with(k.as_str()) {
                let better = best.map(|(len, _)| k.len() > len).unwrap_or(true);
                if better {
                    best = Some((k.len(), *v));
                }
            }
        }
        best.map(|(_, v)| v)
    }

    /// Estimated USD cost for a model's token totals, or `None` if the model is
    /// not in the table.
    pub fn cost_usd(&self, model: &str, t: &ModelTokens) -> Option<f64> {
        let p = self.price_for(model)?;
        let write_5m = t
            .cache_creation_tokens
            .saturating_sub(t.cache_creation_1h_tokens);
        let dollars = t.input_tokens as f64 * p.input
            + t.output_tokens as f64 * p.output
            + t.cache_read_tokens as f64 * (CACHE_READ_MULT * p.input)
            + write_5m as f64 * (CACHE_WRITE_5M_MULT * p.input)
            + t.cache_creation_1h_tokens as f64 * (CACHE_WRITE_1H_MULT * p.input);
        Some(dollars / PER_MTOK)
    }

    /// Plan-metered spend for the streams that count against a usage plan:
    /// input + output + cache creation (5-minute and 1-hour writes at their
    /// multipliers). Cache **reads** are deliberately excluded - the measurement
    /// doc excludes the cheap read stream from the "spend" weighting used for the
    /// headline multiplier. `None` if the model has no price (never guessed).
    pub fn plan_metered_spend(&self, model: &str, t: &ModelTokens) -> Option<f64> {
        let p = self.price_for(model)?;
        let write_5m = t
            .cache_creation_tokens
            .saturating_sub(t.cache_creation_1h_tokens);
        let dollars = t.input_tokens as f64 * p.input
            + t.output_tokens as f64 * p.output
            + write_5m as f64 * (CACHE_WRITE_5M_MULT * p.input)
            + t.cache_creation_1h_tokens as f64 * (CACHE_WRITE_1H_MULT * p.input);
        Some(dollars / PER_MTOK)
    }

    /// Number of models in the table (used by diagnostics).
    pub fn model_count(&self) -> usize {
        self.models.len()
    }
}
