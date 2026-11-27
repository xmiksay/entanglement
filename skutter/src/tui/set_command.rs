//! `/set`: bare opens the tabbed settings dialog (`settings_dialog`);
//! `/set <key> <value>` stays the one-shot generation override (#376). Moved
//! out of `event_loop.rs`, which is over the file cap.

use entanglement_core::{Holly, InMsg};

use super::app::App;

/// Send `/set <key> <value>` as an [`InMsg::SetGeneration`] (#376): parses the
/// raw text into a partial [`entanglement_core::GenerationParams`] override
/// (the raw-text re-parse pattern, since `parse_command` dropped the trailing
/// args), records it as a pending persist so the confirming
/// `GenerationChanged` writes it to `agent-generation.yml`, then sends the
/// change. A parse error renders as a status line instead — no engine
/// traffic, and no pending persist.
pub(super) async fn send_set(app: &mut App, holly: &Holly, text: &str) {
    if text.trim() == "/set" {
        app.open_settings_dialog();
        return;
    }
    match crate::tui::commands::parse_set_args(text) {
        Ok(overrides) => {
            app.record_pending_generation_persist(overrides);
            let _ = holly
                .send(InMsg::SetGeneration {
                    session: app.active_session_id().clone(),
                    overrides,
                })
                .await;
        }
        Err(message) => app.record_set_error(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{EngineConfig, ReasoningEffort, SessionId};
    use std::time::Duration;

    async fn next_inbound(rx: &mut tokio::sync::broadcast::Receiver<InMsg>) -> Option<InMsg> {
        tokio::time::timeout(Duration::from_millis(150), rx.recv())
            .await
            .ok()
            .and_then(Result::ok)
    }

    #[tokio::test]
    async fn bare_set_opens_the_dialog_and_sends_nothing() {
        let holly = Holly::spawn(EngineConfig::default());
        let mut inbound = holly.subscribe_inbound();
        let mut app = App::new_for_test(SessionId::new("s1"));
        send_set(&mut app, &holly, "/set").await;
        assert!(app.showing_settings_dialog());
        assert!(next_inbound(&mut inbound).await.is_none());
    }

    #[tokio::test]
    async fn set_with_arguments_still_applies_directly() {
        let holly = Holly::spawn(EngineConfig::default());
        let mut inbound = holly.subscribe_inbound();
        let mut app = App::new_for_test(SessionId::new("s1"));
        send_set(&mut app, &holly, "/set effort high").await;
        assert!(!app.showing_settings_dialog());
        let msg = next_inbound(&mut inbound)
            .await
            .expect("SetGeneration sent");
        assert!(matches!(
            msg,
            InMsg::SetGeneration { overrides, .. }
                if overrides.reasoning_effort == Some(ReasoningEffort::High)
        ));
    }
}
