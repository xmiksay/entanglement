//! `skutter inspect aux-models` (#558).
//!
//! Surfaces the persisted per-purpose auxiliary LLM pins (`aux-models.yml`,
//! [`crate::config::aux_models`]) — set via `/aux-model` and otherwise
//! invisible outside that one TUI command. A purpose with no pin falls back to
//! the session's primary model at call time; this view shows exactly which
//! purposes are pinned and to what, without spawning the engine.

use std::fmt::Write as _;

use anyhow::Result;

use crate::config::aux_models::{AuxModelStore, Purpose};

/// Print every known auxiliary-LLM purpose with its persisted pin, or
/// "(unset — falls back to the session's primary model)" when none is set.
pub fn inspect_aux_models() -> Result<()> {
    let store = AuxModelStore::load();
    print!("{}", render_aux_models(&store));
    Ok(())
}

fn render_aux_models(store: &AuxModelStore) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "auxiliary LLM pins (aux-models.yml):");
    for purpose in [Purpose::Summarize, Purpose::SessionTitle] {
        match store.get(purpose) {
            Some((provider, model)) => {
                let _ = writeln!(out, "  {}: {provider}/{model}", purpose.as_str());
            }
            None => {
                let _ = writeln!(
                    out,
                    "  {}: (unset — falls back to the session's primary model)",
                    purpose.as_str()
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_unset_purposes_with_the_fallback_note() {
        let store = AuxModelStore::default();
        let rendered = render_aux_models(&store);
        assert!(rendered.contains("summarize: (unset"));
        assert!(rendered.contains("session_title: (unset"));
    }

    #[test]
    fn renders_a_pinned_purpose_and_leaves_the_other_unset() {
        let store = AuxModelStore::for_test(&[(Purpose::Summarize, "zai", "glm-4.5-flash")]);
        let rendered = render_aux_models(&store);
        assert!(rendered.contains("summarize: zai/glm-4.5-flash"));
        assert!(rendered.contains("session_title: (unset"));
    }
}
