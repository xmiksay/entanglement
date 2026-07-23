//! `App` surface for the `/bash` command (#498): folds the
//! `OutEvent::BashChanged` reply to the `InMsg::BashEnable`/`BashDisable`
//! wire ops `event_loop` sends. No panel like `/mcp list` — there is nothing
//! to list — so both a parse error and a confirmation render as a transcript
//! status line, mirroring `App::record_mcp_error`/`handle_mcp_changed`.

use entanglement_core::BashGrade;

use super::App;

impl App {
    /// Records a `/bash` parse error (unknown subcommand/flag) as a transcript
    /// status line — no engine traffic, so nothing else to fold.
    pub fn record_bash_error(&mut self, message: String) {
        self.sessions
            .active_view_mut()
            .record_status("bash", format!("error: {message}"));
        self.mark_dirty();
    }

    /// Folds an `OutEvent::BashChanged` reply (#498) into the active session's
    /// transcript as a status line.
    pub(super) fn handle_bash_changed(&mut self, enabled: bool, grade: Option<&BashGrade>) {
        let message = match (enabled, grade) {
            (true, Some(BashGrade::Ask)) => "enabled (ask)".to_string(),
            (true, Some(BashGrade::Allow { pattern: None })) => "enabled (allow)".to_string(),
            (true, Some(BashGrade::Allow { pattern: Some(p) })) => format!("enabled (allow {p})"),
            (true, None) => "enabled".to_string(),
            (false, _) => "disabled".to_string(),
        };
        self.sessions
            .active_view_mut()
            .record_status("bash", message);
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use entanglement_core::SessionId;

    use super::*;

    #[test]
    fn handle_bash_changed_renders_the_grade() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_bash_changed(true, Some(&BashGrade::Allow { pattern: None }));
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("enabled (allow)"));
        assert!(rendered, "expected a transcript entry noting bash enabled");
    }

    #[test]
    fn handle_bash_changed_renders_disabled() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.handle_bash_changed(false, None);
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("disabled"));
        assert!(rendered, "expected a transcript entry noting bash disabled");
    }

    #[test]
    fn record_bash_error_renders_the_message() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.record_bash_error("boom".to_string());
        let rendered = app
            .transcript()
            .iter()
            .any(|e| format!("{e:?}").contains("boom"));
        assert!(rendered, "expected a transcript entry noting the error");
    }
}
