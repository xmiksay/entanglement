//! `/aux-model` subcommand parsing + persistence dispatch (Issue 5): kept in
//! its own sibling module rather than folded into `tui/commands.rs` or
//! `tui/event_loop.rs` — both already at/over the 400-line cap — mirroring how
//! `/mcp` was split into `mcp_command.rs` (#373).
//!
//! The command persists a per-purpose `(provider, model)` pin into the managed
//! `aux-models.yml` and the installed [`AuxModelStore`], so the next auxiliary
//! LLM call (session-title generator; compaction summary once wired) resolves
//! it instead of the primary model. No wire traffic — this is head-side
//! persistence, not an engine op.

use crate::aux_llm::{AuxModelStore, Purpose};

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
    match app.persist_aux_pin(purpose, &provider, &model) {
        Ok(()) => app.confirm_aux_pin(purpose, &provider, &model),
        Err(e) => {
            tracing::warn!("could not persist aux-model pin: {e}");
            app.record_aux_error(e);
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

    use std::sync::{Arc, Mutex};

    use entanglement_core::SessionId;

    use crate::config::aux_models::AuxModelsPath;
    use crate::tui::session_view::TranscriptEntry;

    #[test]
    fn pin_label_falls_back_when_a_purpose_is_unpinned() {
        let store = AuxModelStore::default();
        assert_eq!(pin_label(&store, Purpose::Summarize), "(primary model)");
        assert_eq!(pin_label(&store, Purpose::SessionTitle), "(primary model)");
        assert_eq!(pin_label(&store, Purpose::Narrate), "(primary model)");
    }

    fn last_aux_error(app: &App) -> Option<&str> {
        app.transcript().iter().rev().find_map(|e| match e {
            TranscriptEntry::ToolOutput { tool, output }
                if tool.as_deref() == Some("aux-model") =>
            {
                output.strip_prefix("error: ")
            }
            _ => None,
        })
    }

    #[test]
    fn a_path_less_store_records_an_error_and_keeps_the_store() {
        let mut app = App::new_for_test(SessionId::new("t"));
        let store = Arc::new(Mutex::new(AuxModelStore::default()));
        app.set_aux_models(store.clone(), AuxModelsPath::at(None));

        send_aux_model(&mut app, "/aux-model summarize p/m");

        let err = last_aux_error(&app).expect("an aux-model error line");
        assert!(err.contains("ENTANGLEMENT_STATE_DIR"), "{err}");
        assert_eq!(store.lock().unwrap().get(Purpose::Summarize), None);
        assert_eq!(app.toast(), None);
    }

    #[test]
    fn no_installed_store_records_an_error() {
        let mut app = App::new_for_test(SessionId::new("t"));
        send_aux_model(&mut app, "/aux-model summarize p/m");
        assert!(last_aux_error(&app).is_some());
    }

    #[test]
    fn a_pin_lands_in_the_file_and_the_shared_store() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("aux-models.yml");
        let mut app = App::new_for_test(SessionId::new("t"));
        let store = Arc::new(Mutex::new(AuxModelStore::default()));
        app.set_aux_models(store.clone(), AuxModelsPath::at(Some(file.clone())));

        send_aux_model(&mut app, "/aux-model summarize p/m");

        assert_eq!(last_aux_error(&app), None);
        assert_eq!(
            store.lock().unwrap().get(Purpose::Summarize),
            Some(("p", "m"))
        );
        let on_disk = AuxModelsPath::at(Some(file)).load();
        assert_eq!(on_disk.get(Purpose::Summarize), Some(("p", "m")));
        assert!(app.toast().is_some_and(|t| t.contains("p/m")));
    }

    #[test]
    fn a_poisoned_store_lock_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new_for_test(SessionId::new("t"));
        let store = Arc::new(Mutex::new(AuxModelStore::default()));
        let poisoner = store.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the aux-models lock");
        })
        .join();
        app.set_aux_models(
            store,
            AuxModelsPath::at(Some(dir.path().join("aux-models.yml"))),
        );

        let err = app
            .persist_aux_pin(Purpose::Summarize, "p", "m")
            .unwrap_err();
        assert!(err.contains("poisoned"), "{err}");
    }
}
