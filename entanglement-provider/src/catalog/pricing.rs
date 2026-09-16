//! Per-model pricing (`ModelEntry::pricing`) and the cost formula over a
//! normalized [`Usage`](crate::Usage). Split out of `catalog.rs` for the
//! 400-line file cap.

use serde::Deserialize;

/// USD per million tokens. Every field is optional; providers without a given
/// billing dimension (e.g. no separate cache-write charge) omit it.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cached_input: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
}

impl ModelPricing {
    /// USD cost for a normalized [`Usage`] tally (#192). Each token dimension is
    /// multiplied by its per-million rate; a rate the provider doesn't bill (an
    /// unset field) contributes nothing. Because [`Usage::input_tokens`] is the
    /// *uncached* input, the cached/cache-write dimensions never double-count.
    ///
    /// [`Usage`]: crate::Usage
    /// [`Usage::input_tokens`]: crate::Usage::input_tokens
    pub fn cost_usd(&self, usage: &crate::Usage) -> f64 {
        let bill = |tokens: Option<u64>, rate: Option<f64>| {
            rate.map_or(0.0, |r| tokens.unwrap_or(0) as f64 * r / 1_000_000.0)
        };
        bill(usage.input_tokens, self.input)
            + bill(usage.output_tokens, self.output)
            + bill(usage.cached_input_tokens, self.cached_input)
            + bill(usage.cache_write_tokens, self.cache_write)
    }
}
