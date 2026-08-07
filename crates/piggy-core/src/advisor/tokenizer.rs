//! A real tokenization for [`crate::probe`], from the advisor's own weights.
//!
//! [`crate::probe::SchemaTokenizer`] exists so that nothing in `probe.rs` (or in
//! the default build) links llama.cpp. This is the implementation that seam was
//! written for, and it is the only file in the advisor that both halves touch.

use anyhow::{Context, Result};
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};

use crate::probe::{BytesEstimate, SchemaTokenizer};

use super::AdvisorModel;

/// The advisor's tokenizer, loaded without the advisor.
///
/// Loaded `vocab_only`, so this reads the vocabulary out of the GGUF and nothing
/// else: no tensor data, no Metal allocation, no KV cache. The alternative is
/// roughly three gigabytes resident and a second and a half of load to measure
/// the length of a string.
///
/// **The honest caveat, which belongs wherever this count is described:** a Qwen
/// or Gemma tokenizer is not Claude's tokenizer. The count is measured in the
/// sense that it is a real tokenization of real bytes; it is not the count the
/// API would bill. That is exactly the framing the spec asks for, "measured
/// manifest, estimated session impact", and it is why a probe row keeps a
/// separate label for how the number was arrived at.
pub struct ModelTokenizer {
    model: LlamaModel,
    /// [`AdvisorModel::id`], which is what a manifest row is labelled with.
    id: &'static str,
}

impl ModelTokenizer {
    /// Load `spec`'s vocabulary.
    ///
    /// Fails when the weights are absent, which is the caller's cue to keep the
    /// [`BytesEstimate`] default rather than to report an error: a probe with an
    /// estimated count is the shipped behaviour, not a degraded one.
    pub fn load(spec: &'static AdvisorModel) -> Result<ModelTokenizer> {
        let backend = super::llama::backend()?;
        let path = spec.path();
        if !path.exists() {
            anyhow::bail!("{} is not downloaded", spec.file);
        }
        let params = LlamaModelParams::default()
            .with_vocab_only(true)
            // Nothing to offload: there are no tensors in a vocab-only load.
            .with_n_gpu_layers(0);
        let model = LlamaModel::load_from_file(backend, &path, &params)
            .with_context(|| format!("loading the vocabulary from {}", path.display()))?;
        Ok(ModelTokenizer { model, id: spec.id })
    }
}

impl SchemaTokenizer for ModelTokenizer {
    fn count(&self, text: &str) -> i64 {
        // `AddBos::Never`: a schema fragment is not the start of a sequence, and
        // `Always` would inflate every manifest by exactly one token.
        self.model
            .str_to_token(text, AddBos::Never)
            .map(|t| t.len() as i64)
            // Unreachable for the input this actually sees: `str_to_token`
            // only refuses a string carrying a real NUL byte, and serde_json
            // writes one as a six-character escape rather than as a byte.
            // Kept because the alternative is an unwrap in a process that
            // must not panic, and losing a measurement is not worth that.
            .unwrap_or_else(|_| BytesEstimate.count(text))
    }

    fn label(&self) -> String {
        self.id.to_string()
    }
}
