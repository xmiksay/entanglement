//! `/aux-model` subcommand parsing + persistence dispatch (Issue 5): kept in
//! its own sibling module rather than folded into `tui/commands.rs` or
//! `tui/event_loop.rs` — both already at/over the 400-line cap — mirroring how
//! `/allow` was split into `allow_command.rs` (#486) and `/mcp` into
//! `mcp_command.rs` (#373).
//!
//! The command persists a per-purpose `(provider, model)` pin into the managed
//! `aux-models.yml` via the installed [`AuxModelStore`], so the next auxiliary
//! LLM call (session-title generator; compaction summary once wired) resolves
//! it instead of the primary model. No wire traffic — this is head-side
//! persistence, not an engine op (exactly like `/allow`'s direct store write).

use crate::config::aux_models::{AuxModelStore, Purpose};

use super::app::App;

/// Send `/aux-model <purpose> <provider>/<model>` (Issue 5): parse the trailing
/// text, persist the pin through the installed [`AuxModelStore`], and render a
/// transient status line (toast) as the confirmation. A parse error (unknown
/// purpose, missing slash, empty model) renders as a status line instead.
///
/// `/aux-model` with no args (or `/aux-model list`) renders the current pins as
/// a status line rather than erroring — the discoverability affordance the
/// `/show` command uses for generation parameters.
pub(super) fn send_aux_model(app: &mut App, text: &str) {
    let parsed = match crate::tui::commands::parse_aux_model_args(text) {
        Ok(parsed) => parsed,
        Err(message) => {
            app.record_aux_error(message);
            return;
        }
    };
    let Some((purpose, provider, model)) = parsed else {
        // Bare `/aux-model` or `/aux-model list`: render the current pins.
        app.show_aux_pins();
        return;
    };
    // Clone the Arc handle out of `app` before locking + mutating, so the
    // subsequent `app.*` calls don't alias an immutable borrow held across the
    // mutex guard's drop — a `set` that errors must still render via `app`.
    let Some(store) = app.aux_models().cloned() else {
        app.record_aux_error(
            "no config directory for the managed aux-models file; \
             set ENTANGLEMENT_AUX_MODELS_FILE to a path first"
                .to_string(),
        );
        return;
    };
    let result = store.lock().unwrap().set(purpose, &provider, &model);
    match result {
        Ok(()) => app.confirm_aux_pin(purpose, &provider, &model),
        Err(e) => {
            tracing::warn!("could not persist aux-model pin: {e:#}");
            app.record_aux_error(format!("persist failed: {e}"));
        }
    }
}

/// Render a purpose pin as `<provider>/<model>` for the status line, or
/// `(primary model)` when the purpose has no pin.
pub(crate) fn pin_label(store: &AuxModelStore, purpose: Purpose) -> String {
    match store.get(purpose) {
        Some((p, m)) => format!("{p}/{m}"),
        None => "(primary model)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_label_falls_back_when_a_purpose_is_unpinned() {
        // An empty store (no path) can't `set`, so only the unpinned label is
        // exercised here — the set path is covered by the integration tests.
        let store = AuxModelStore::default();
        assert_eq!(pin_label(&store, Purpose::Summarize), "(primary model)");
        assert_eq!(pin_label(&store, Purpose::SessionTitle), "(primary model)");
    }
}
