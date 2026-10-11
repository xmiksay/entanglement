//! [`ModelEntry`]'s derived views: its display label and the generation
//! knobs its capability flags license (#191). Split out of `catalog.rs`
//! to keep it under the file cap.

use super::ModelEntry;

impl ModelEntry {
    /// Human-facing label, falling back to the id when `display_name` is unset.
    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.id)
    }

    /// The generation knobs this model's catalog capabilities license (#191). A
    /// flag off means the knob is dropped rather than sent: no temperature when
    /// `supports_temperature: false`, no thinking budget when `supports_thinking:
    /// false`. This is where the previously write-only capability metadata becomes
    /// load-bearing — the head threads the result onto every [`crate::LlmRequest`].
    pub fn generation_params(&self) -> crate::GenerationParams {
        crate::GenerationParams {
            temperature: if self.supports_temperature {
                self.default_temperature
            } else {
                None
            },
            max_output_tokens: self.max_output_tokens,
            thinking_budget_tokens: if self.supports_thinking {
                self.thinking_budget_tokens
            } else {
                None
            },
            reasoning_effort: self.default_reasoning_effort,
        }
    }
}
