use anyhow::Result;
use entanglement_core::Holly;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::App;
use super::attention::Attention;
use super::event::Event;
use super::keybindings::LeaderResult;
use super::modal_events::{
    handle_command_palette_event, handle_inspect_event, handle_key_dialog_event,
    handle_mode_picker_event, handle_model_picker_event, handle_mouse, handle_profile_picker_event,
    handle_question_event, handle_resume_modal_event, handle_session_tools_dialog_event,
    handle_sessions_modal_event, handle_tools_view_event, DIALOG_PAGE_SIZE,
};
use super::session_view::ApprovalMode;

// Key handling per approval mode, and the slash-command senders the Normal
// mode's submit path routes to.
mod approval_keys;
mod normal_keys;
mod slash_send;

/// Shared input-edit keys for any `SimpleInput`-driven field: Ctrl+Left/Right
/// word jumps, plain + Ctrl Home/End, Alt+Enter newline. Returns whether the key
/// was consumed so callers can fall back to their own bindings (e.g. the Normal
/// `Enter` = send). Kept free of `holly`/mention side effects; the Normal path
/// re-runs `update_popups` after a mutation, the reject/answer paths don't need it.
pub(super) fn apply_input_edit_key(
    app: &mut App,
    key: &ratatui::crossterm::event::KeyEvent,
) -> bool {
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};
    let mods = key.modifiers;
    match key.code {
        // Alt+Enter newline (D1): universally detected via the ESC Alt prefix.
        KeyCode::Enter if mods.contains(KeyModifiers::ALT) => {
            app.input().insert_newline();
            app.update_popups();
            true
        }
        // Ctrl+Left/Right word jumps.
        KeyCode::Left if mods.contains(KeyModifiers::CONTROL) => {
            app.input().move_word_left();
            app.update_popups();
            true
        }
        KeyCode::Right if mods.contains(KeyModifiers::CONTROL) => {
            app.input().move_word_right();
            app.update_popups();
            true
        }
        // Ctrl+Home/End document jumps; plain Home/End line jumps.
        KeyCode::Home if mods.contains(KeyModifiers::CONTROL) => {
            app.input().move_to_doc_home();
            app.update_popups();
            true
        }
        KeyCode::End if mods.contains(KeyModifiers::CONTROL) => {
            app.input().move_to_doc_end();
            app.update_popups();
            true
        }
        KeyCode::Home => {
            app.input().move_cursor_to_head();
            app.update_popups();
            true
        }
        KeyCode::End => {
            app.input().move_cursor_to_end();
            app.update_popups();
            true
        }
        _ => false,
    }
}

pub(super) async fn handle_event(
    app: &mut App,
    holly: &Holly,
    attention: &mut Attention,
    ev: Event,
) -> Result<bool> {
    app.mark_dirty();
    match ev {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Press {
                return handle_key(app, holly, key).await;
            }
        }
        Event::Mouse(mouse_event) => handle_mouse(app, holly, mouse_event).await,
        Event::Resize => {}
        Event::FocusGained => attention.set_focused(true),
        Event::FocusLost => attention.set_focused(false),
        Event::Paste(s) => match app.approval_mode() {
            ApprovalMode::Normal => {
                app.input().insert_str(&s);
                app.update_popups();
            }
            // The reject reason surfaces as a single status line
            // (`record_rejected`'s `✗ rejected {tool} — {reason}`), so a
            // paste's newlines collapse to spaces instead of splitting into
            // buffer rows (unlike the Normal-mode multi-row paste above).
            ApprovalMode::EnteringRejectReason { .. } => {
                let collapsed = s.replace("\r\n", " ").replace(['\n', '\r'], " ");
                app.input().insert_str(&collapsed);
            }
            ApprovalMode::WaitingForApproval { .. } => {}
        },
        // External SIGINT (ADR-0087): route through the same two-stage path as
        // an in-app Ctrl+C so an out-of-band signal never leaves the terminal
        // in raw mode (the "half killed" state).
        Event::Interrupt => {
            if app.handle_quit_key() {
                return Ok(true);
            }
        }
        // The background-built `@file` index landing (#678).
        Event::FileIndexReady(index) => app.set_file_index(index),
    }
    Ok(false)
}

/// One key press: global keys first, then the topmost open modal, then the
/// active approval mode. `Ok(true)` asks the event loop to quit.
async fn handle_key(app: &mut App, holly: &Holly, key: KeyEvent) -> Result<bool> {
    // Two-stage Ctrl+C (ADR-0087): intercepted once here, before any
    // modal/approval routing, so it behaves identically in every
    // context and the eleven duplicate `Char('c')` arms are gone.
    // Ctrl+Q stays an unconditional immediate quit (the escape hatch);
    // any other key disarms a pending quit.
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return Ok(app.handle_quit_key());
    }
    app.clear_quit_pending();
    if app.showing_sessions_modal() {
        return handle_sessions_modal_event(app, holly, key).await;
    }
    if app.showing_settings_dialog() {
        return crate::tui::settings_events::handle_settings_key(app, holly, key).await;
    }
    // Bare `/enable`'s session-tools checklist (#539).
    if app.showing_session_tools_dialog() {
        return handle_session_tools_dialog_event(app, holly, key).await;
    }
    // `/tools`' read-heavy browser (#560 P9, ADR-0199 part 3).
    if app.showing_tools_view() {
        return handle_tools_view_event(app, holly, key).await;
    }
    if app.showing_profile_picker() {
        return handle_profile_picker_event(app, key).await;
    }
    if app.showing_model_picker() {
        return handle_model_picker_event(app, holly, key).await;
    }
    if app.showing_mode_picker() {
        return handle_mode_picker_event(app, holly, key).await;
    }
    if app.showing_key_dialog() {
        return handle_key_dialog_event(app, key).await;
    }
    if app.showing_help() {
        match key.code {
            KeyCode::Esc => app.close_help(),
            KeyCode::Up | KeyCode::Char('k') => {
                let s = app.help_scroll();
                app.set_help_scroll(s.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let s = app.help_scroll();
                app.set_help_scroll(s.saturating_add(1));
            }
            KeyCode::PageUp => {
                let s = app.help_scroll();
                app.set_help_scroll(s.saturating_sub(DIALOG_PAGE_SIZE as u16));
            }
            KeyCode::PageDown => {
                let s = app.help_scroll();
                app.set_help_scroll(s.saturating_add(DIALOG_PAGE_SIZE as u16));
            }
            _ => {}
        }
        return Ok(false);
    }
    // `/mcp list` panel (#373, selectable since #539):
    // `Up`/`Down`/`j`/`k` move the server selection, `e`/`d`
    // enable/disable the highlighted server for the active session
    // (a `mcp__<name>__*` tool-overlay entry), `c`/`t` run the
    // OAuth connect/check for it (ADR-0153),
    // `PageUp`/`PageDown` scroll, `Esc` closes.
    if app.showing_mcp_panel() {
        match key.code {
            KeyCode::Esc => app.close_mcp_panel(),
            KeyCode::Up | KeyCode::Char('k') => app.mcp_select_prev(),
            KeyCode::Down | KeyCode::Char('j') => app.mcp_select_next(),
            KeyCode::Char('e') => {
                if let Some(name) = app.mcp_selected_server() {
                    crate::tui::enable_command::upsert_enable(
                        app,
                        holly,
                        format!("mcp__{name}__*"),
                        false,
                        None,
                    )
                    .await;
                }
            }
            KeyCode::Char('d') => {
                if let Some(name) = app.mcp_selected_server() {
                    crate::tui::enable_command::upsert_deny(app, holly, format!("mcp__{name}__*"))
                        .await;
                }
            }
            // OAuth row actions (ADR-0153). The panel is closed
            // first: `connect` parks on the browser and reports
            // through transcript status lines, which the panel
            // would otherwise cover.
            KeyCode::Char('c') => {
                if let Some(name) = app.mcp_selected_server() {
                    let name = name.to_string();
                    app.close_mcp_panel();
                    crate::tui::mcp_command::send_mcp_auth(
                        app,
                        holly,
                        name,
                        entanglement_core::McpAuthAction::Connect,
                    )
                    .await;
                }
            }
            KeyCode::Char('t') => {
                if let Some(name) = app.mcp_selected_server() {
                    let name = name.to_string();
                    app.close_mcp_panel();
                    crate::tui::mcp_command::send_mcp_auth(
                        app,
                        holly,
                        name,
                        entanglement_core::McpAuthAction::Check,
                    )
                    .await;
                }
            }
            KeyCode::PageUp => {
                let s = app.mcp_scroll();
                app.set_mcp_scroll(s.saturating_sub(DIALOG_PAGE_SIZE as u16));
            }
            KeyCode::PageDown => {
                let s = app.mcp_scroll();
                app.set_mcp_scroll(s.saturating_add(DIALOG_PAGE_SIZE as u16));
            }
            _ => {}
        }
        return Ok(false);
    }
    if app.showing_command_palette() {
        return handle_command_palette_event(app, holly, key).await;
    }
    if app.showing_resume_modal() {
        return handle_resume_modal_event(app, holly, key).await;
    }
    if app.showing_inspect() {
        return handle_inspect_event(app, key).await;
    }
    // Global attention jump: Ctrl+G switches to the oldest
    // background session waiting on an approval/question (the
    // attention panel's target). Intercepted ahead of the
    // question/approval routing so it works while the *active*
    // session is itself parked — the reject-reason and free-text
    // arms match bare `Char(c)` with no modifier check, so
    // without this Ctrl+G would type a literal `g` there.
    if key.code == KeyCode::Char('g') && key.modifiers == KeyModifiers::CONTROL {
        app.jump_to_next_attention();
        return Ok(false);
    }
    // A model-driven `ask_user` question takes over input until
    // answered (ADR-0027), just like an approval prompt.
    if app.is_asking() {
        return handle_question_event(app, holly, key).await;
    }

    let current_mode = app.approval_mode().clone();

    if key.code == KeyCode::Char('l')
        && key.modifiers == KeyModifiers::CONTROL
        && !matches!(current_mode, ApprovalMode::EnteringRejectReason { .. })
    {
        app.toggle_sessions_modal();
        return Ok(false);
    }

    if matches!(current_mode, ApprovalMode::Normal) {
        match app.leader_handler().handle_key(&key) {
            LeaderResult::Action(action) => {
                if app.dispatch_action(action) {
                    return Ok(true);
                }
                return Ok(false);
            }
            // Arming the leader or extending/cancelling a chord must
            // not fall through to the generic Ctrl-char arm (#326).
            LeaderResult::Consumed => return Ok(false),
            LeaderResult::NotMine => {}
        }
    }

    match current_mode {
        ApprovalMode::WaitingForApproval { request_id } => {
            approval_keys::handle_waiting_for_approval(app, holly, key, request_id).await;
            Ok(false)
        }
        ApprovalMode::EnteringRejectReason { request_id } => {
            approval_keys::handle_reject_reason(app, holly, key, request_id).await;
            Ok(false)
        }
        ApprovalMode::Normal => normal_keys::handle_normal_key(app, holly, key).await,
    }
}

#[cfg(test)]
mod tests {
    use super::approval_keys::{record_rejected, send_approval};
    use super::*;
    use crate::tui::session_view::TranscriptEntry;
    use entanglement_core::{ApprovalScope, EngineConfig, InMsg, OutEvent, SessionId};

    fn engine() -> Holly {
        Holly::spawn(EngineConfig::default())
    }

    fn park_request(app: &mut App, sid: &SessionId, request_id: &str, tool: &str, input: &str) {
        app.handle_out_event(OutEvent::ToolRequest {
            session: sid.clone(),
            seq: 1,
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            input: input.to_string(),
        });
    }

    #[tokio::test]
    async fn approving_records_a_transcript_decision_line() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"echo hi"}"#);

        send_approval(&mut app, &holly, "t1".to_string(), ApprovalScope::Session).await;

        let recorded = app.transcript().iter().any(|e| {
            matches!(e, TranscriptEntry::ToolOutput { tool: Some(t), output }
                if t == "approval" && output.contains("approved bash") && output.contains("session"))
        });
        assert!(
            recorded,
            "expected an approval decision line in the transcript: {:?}",
            app.transcript()
        );
    }

    #[test]
    fn rejecting_records_a_decision_line_with_its_reason() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        record_rejected(&mut app, "bash", &Some("looks risky".to_string()));

        let recorded = app.transcript().iter().any(|e| {
            matches!(e, TranscriptEntry::ToolOutput { tool: Some(t), output }
                if t == "approval" && output.contains("rejected bash") && output.contains("looks risky"))
        });
        assert!(
            recorded,
            "expected a rejection decision line with its reason: {:?}",
            app.transcript()
        );
    }

    // --- A2: bracketed paste (Event::Paste) mode-gating -----------------

    #[tokio::test]
    async fn paste_with_embedded_newlines_creates_multiple_rows_and_sends_nothing() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Paste("foo\nbar\nbaz".to_string()),
        )
        .await
        .unwrap();

        // One buffer, three rows — not a single row holding a literal `\n`
        // (the `SimpleInput::insert_str` half of the A2 fix).
        assert_eq!(
            app.input().lines(),
            &["foo".to_string(), "bar".to_string(), "baz".to_string()]
        );
        // A paste never synthesizes an Enter, so it must never submit — the
        // actual A2 bug (N submits from N embedded newlines) came from a
        // paste arriving as a flood of *key* events without bracketed paste
        // enabled, not from anything the `Event::Paste` handler itself does.
        let sent = drain_inbound(&mut rx).await;
        assert!(
            sent.is_empty(),
            "pasting must not submit anything: {sent:?}"
        );
    }

    #[tokio::test]
    async fn paste_in_reject_reason_collapses_newlines_to_spaces() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut attention = Attention::from_env();
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"ls"}"#);
        app.set_approval_mode(ApprovalMode::EnteringRejectReason {
            request_id: "t1".to_string(),
        });

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Paste("looks\r\nrisky\rto me".to_string()),
        )
        .await
        .unwrap();

        // The reject reason renders as one status line (`record_rejected`'s
        // `✗ rejected {tool} — {reason}`), so newlines collapse to spaces
        // here instead of splitting into buffer rows like the Normal-mode
        // paste above.
        assert_eq!(app.input().lines(), &["looks risky to me".to_string()]);
    }

    #[tokio::test]
    async fn paste_while_waiting_for_approval_is_a_noop() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut attention = Attention::from_env();
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"ls"}"#);

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Paste("ignored".to_string()),
        )
        .await
        .unwrap();

        assert_eq!(app.input_text(), "", "no input box is active while parked");
    }

    // --- B2: full-body approval pager ------------------------------------

    #[tokio::test]
    async fn v_opens_the_pager_and_esc_closes_it_without_interrupting() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"ls"}"#);

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Char('v'))),
        )
        .await
        .unwrap();
        assert!(app.showing_approval_pager(), "v should open the pager");

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Esc)),
        )
        .await
        .unwrap();
        assert!(
            !app.showing_approval_pager(),
            "Esc should close the pager, not the approval"
        );
        assert!(
            matches!(app.approval_mode(), ApprovalMode::WaitingForApproval { .. }),
            "closing the pager must leave the approval parked, not send Stop"
        );
        assert!(
            drain_inbound(&mut rx).await.is_empty(),
            "no Stop should have been sent for the pager's own Esc"
        );
    }

    #[tokio::test]
    async fn pager_scroll_keys_move_the_scroll_offset() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut attention = Attention::from_env();
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"ls"}"#);
        app.open_approval_pager();

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::PageDown)),
        )
        .await
        .unwrap();
        assert_eq!(app.approval_pager_scroll(), 10, "PageDown pages by 10");

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Char('j'))),
        )
        .await
        .unwrap();
        assert_eq!(app.approval_pager_scroll(), 11, "j steps by 1");

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Char('k'))),
        )
        .await
        .unwrap();
        assert_eq!(app.approval_pager_scroll(), 10, "k steps back by 1");

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::PageUp)),
        )
        .await
        .unwrap();
        assert_eq!(app.approval_pager_scroll(), 0, "PageUp pages back by 10");
    }

    #[tokio::test]
    async fn y_resolves_the_approval_and_closes_the_pager_without_reopening_it() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"ls"}"#);
        app.open_approval_pager();

        // The decision keys must not require closing the pager first (#B2).
        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Char('y'))),
        )
        .await
        .unwrap();

        assert!(
            matches!(app.approval_mode(), ApprovalMode::Normal),
            "y must resolve the approval even while the pager is open"
        );
        assert!(
            !app.showing_approval_pager(),
            "resolving the approval must close the now-stale pager"
        );
        let sent = drain_inbound(&mut rx).await;
        assert!(
            sent.iter()
                .any(|m| matches!(m, InMsg::Approve { request_id, .. } if request_id == "t1")),
            "expected an Approve for t1: {sent:?}"
        );
    }

    // --- B1: PageUp/PageDown scroll the transcript while parked ----------

    #[tokio::test]
    async fn pageup_scrolls_the_transcript_while_waiting_for_approval() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut attention = Attention::from_env();
        app.set_viewport_metrics(20, 5);
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"ls"}"#);
        assert!(app.auto_follow());

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::PageUp)),
        )
        .await
        .unwrap();

        assert!(
            !app.auto_follow(),
            "PageUp should freeze/scroll the transcript like the Normal arm"
        );
    }

    #[tokio::test]
    async fn pageup_scrolls_the_transcript_while_entering_reject_reason() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut attention = Attention::from_env();
        app.set_viewport_metrics(20, 5);
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"ls"}"#);
        app.set_approval_mode(ApprovalMode::EnteringRejectReason {
            request_id: "t1".to_string(),
        });
        assert!(app.auto_follow());

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::PageUp)),
        )
        .await
        .unwrap();

        assert!(
            !app.auto_follow(),
            "PageUp should freeze/scroll the transcript while typing a reason too"
        );
    }

    #[tokio::test]
    async fn ctrl_g_jumps_to_the_waiting_background_session() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active.clone());
        let holly = engine();
        let bg = SessionId::new("bg-1");
        park_request(&mut app, &bg, "t1", "bash", r#"{"command":"ls"}"#);
        assert_eq!(app.active_session_id(), &active);

        let key =
            ratatui::crossterm::event::KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        let mut attention = Attention::from_env();
        handle_event(&mut app, &holly, &mut attention, Event::Key(key))
            .await
            .unwrap();

        assert_eq!(app.active_session_id(), &bg, "jumped to the parked session");
        assert!(
            matches!(app.approval_mode(), ApprovalMode::WaitingForApproval { .. }),
            "the existing approval UI takes over after the jump"
        );
    }

    #[tokio::test]
    async fn ctrl_g_never_types_a_g_into_the_reject_reason() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        park_request(&mut app, &sid, "t1", "bash", r#"{"command":"ls"}"#);
        app.set_approval_mode(ApprovalMode::EnteringRejectReason {
            request_id: "t1".to_string(),
        });

        let key =
            ratatui::crossterm::event::KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        let mut attention = Attention::from_env();
        handle_event(&mut app, &holly, &mut attention, Event::Key(key))
            .await
            .unwrap();

        assert_eq!(
            app.input().lines().join(""),
            "",
            "the bare Char('g') arm must not swallow the chord"
        );
    }

    #[test]
    fn rejecting_without_a_reason_still_records_a_decision_line() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        record_rejected(&mut app, "bash", &None);

        let recorded = app.transcript().iter().any(|e| {
            matches!(e, TranscriptEntry::ToolOutput { tool: Some(t), output }
                if t == "approval" && output == "✗ rejected bash")
        });
        assert!(
            recorded,
            "expected a bare rejection decision line: {:?}",
            app.transcript()
        );
    }

    // --- Issue #6: Esc stops the turn; /stop, /pause, /continue commands ----
    //
    // These cover the control-flow invariants the feature depends on: bare Esc
    // no longer quits (it stops the active turn), the mention-popup and
    // multiline-collapse layers still win over stop, and the three slash
    // commands route through their interceptors without quitting. The fan-out
    // of `--all` is asserted by counting inbound `InMsg`s on
    // `holly.subscribe_inbound()` — deterministic, no engine-timing dependency.

    fn key(code: KeyCode) -> ratatui::crossterm::event::KeyEvent {
        ratatui::crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Drains every inbound `InMsg` the supervisor has fanned out since `rx`
    /// was created. `Holly::send` only awaits the mpsc hand-off to the
    /// supervisor; the inbound broadcast happens on the supervisor's own task,
    /// so this polls with a short quiet-window terminator instead of a
    /// synchronous `try_recv` — deterministic without racing the supervisor's
    /// wake-up. The overall deadline is a backstop; in practice every message
    /// arrives within the first quiet window.
    async fn drain_inbound(rx: &mut tokio::sync::broadcast::Receiver<InMsg>) -> Vec<InMsg> {
        use std::time::Duration;
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            // A 30ms quiet window with no message means the supervisor has
            // fanned out everything we sent.
            match tokio::time::timeout(Duration::from_millis(30), rx.recv()).await {
                Ok(Ok(msg)) => seen.push(msg),
                Ok(Err(_)) => break, // channel closed
                Err(_) => break,     // quiet window elapsed
            }
        }
        seen
    }

    #[tokio::test]
    async fn bare_esc_in_normal_mode_stops_the_turn_and_does_not_quit() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();

        // Empty, single-line input → the Esc fallthrough sends `InMsg::Stop`
        // for the active session and returns `Ok(false)` (not quit). Esc used
        // to return `Ok(true)` here (#6 regression).
        let quit = handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Esc)),
        )
        .await
        .expect("handle_event is infallible for Esc");
        assert!(!quit, "bare Esc no longer quits the app");

        let inbox = drain_inbound(&mut rx).await;
        assert_eq!(
            inbox,
            vec![InMsg::Stop {
                session: sid.clone()
            }],
            "bare Esc stops the active session's turn"
        );
    }

    #[tokio::test]
    async fn esc_collapses_multiline_instead_of_stopping() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid);
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();
        app.set_input_multiline(true);
        assert!(app.is_input_multiline());

        let quit = handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Esc)),
        )
        .await
        .unwrap();

        // The multiline-collapse layer must win over the stop layer: no
        // `InMsg::Stop` is sent, the buffer collapses to single-line, and the
        // app stays alive.
        assert!(!quit);
        assert!(
            !app.is_input_multiline(),
            "Esc collapsed the multiline buffer"
        );
        assert!(
            drain_inbound(&mut rx).await.is_empty(),
            "no Stop sent while collapsing a multiline buffer"
        );
    }

    #[tokio::test]
    async fn esc_closes_a_mention_popup_before_stopping() {
        // Wire a real (temp) working dir with one file so typing `@` opens the
        // popup, then Esc must close it rather than send `InMsg::Stop`.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("alpha.txt"), "x").expect("write file");
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid);
        app.init_head_context(dir.path().to_path_buf());
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();

        // The index arrives via the background-build event (#678), not
        // init_head_context — deliver it through the same arm the TUI uses.
        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::FileIndexReady(crate::tui::mention::FileIndex::build(dir.path())),
        )
        .await
        .unwrap();

        // Type `@` → `update_mention` opens the popup (the indexed file matches).
        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Char('@'))),
        )
        .await
        .unwrap();
        assert!(app.mention_visible(), "typing @ opened the popup");

        // Esc closes the popup and must NOT send Stop (the mention layer wins).
        let quit = handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Esc)),
        )
        .await
        .unwrap();
        assert!(!quit);
        assert!(!app.mention_visible(), "Esc closed the mention popup");
        assert!(
            drain_inbound(&mut rx).await.is_empty(),
            "no Stop sent while a mention popup was open"
        );
    }

    #[tokio::test]
    async fn slash_stop_bare_sends_one_stop_and_does_not_quit() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();
        app.set_input_text("/stop".to_string());

        let quit = handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Enter)),
        )
        .await
        .unwrap();
        assert!(!quit, "/stop does not quit");

        assert_eq!(
            drain_inbound(&mut rx).await,
            vec![InMsg::Stop { session: sid }],
            "bare /stop sends exactly one Stop for the active session"
        );
    }

    #[tokio::test]
    async fn slash_stop_all_fans_out_one_stop_per_live_session() {
        let sid = SessionId::new("root");
        let mut app = App::new_for_test(sid.clone());
        let s2 = app.create_session();
        let s3 = app.create_session();
        // `create_session` switches active to the newest; the active session
        // is still "live" and counted in `app.sessions()`, so `/stop --all`
        // must fan out to all three.
        assert_eq!(app.sessions().len(), 3);
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();
        app.set_input_text("/stop --all".to_string());

        let quit = handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Enter)),
        )
        .await
        .unwrap();
        assert!(!quit, "/stop --all does not quit");

        let inbox = drain_inbound(&mut rx).await;
        let stops = inbox
            .iter()
            .filter(|m| matches!(m, InMsg::Stop { .. }))
            .count();
        assert_eq!(
            stops, 3,
            "/stop --all emits one Stop per live session (got {inbox:?})"
        );
        // Each live session id appears exactly once.
        for id in [&sid, &s2, &s3] {
            assert_eq!(
                inbox
                    .iter()
                    .filter(|m| matches!(m, InMsg::Stop { session } if session == id))
                    .count(),
                1,
                "session {id} stopped exactly once"
            );
        }
    }

    #[tokio::test]
    async fn slash_pause_and_continue_each_send_one_message_and_do_not_quit() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut attention = Attention::from_env();

        // /pause → one PauseSession for the active session.
        let mut rx_pause = holly.subscribe_inbound();
        app.set_input_text("/pause".to_string());
        let quit = handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Enter)),
        )
        .await
        .unwrap();
        assert!(!quit, "/pause does not quit");
        assert_eq!(
            drain_inbound(&mut rx_pause).await,
            vec![InMsg::PauseSession {
                session: sid.clone()
            }],
            "bare /pause sends exactly one PauseSession"
        );

        // /continue → one ResumeSession for the active session.
        let mut rx_resume = holly.subscribe_inbound();
        app.set_input_text("/continue".to_string());
        let quit = handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Enter)),
        )
        .await
        .unwrap();
        assert!(!quit, "/continue does not quit");
        assert_eq!(
            drain_inbound(&mut rx_resume).await,
            vec![InMsg::ResumeSession { session: sid }],
            "bare /continue sends exactly one ResumeSession"
        );
    }

    #[tokio::test]
    async fn ctrl_space_pause_resume_toggle_does_not_quit() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();

        // An idle session is not Paused, so the toggle sends PauseSession.
        let toggle =
            ratatui::crossterm::event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        let quit = handle_event(&mut app, &holly, &mut attention, Event::Key(toggle))
            .await
            .unwrap();
        assert!(!quit, "Ctrl+Space does not quit");
        assert_eq!(
            drain_inbound(&mut rx).await,
            vec![InMsg::PauseSession { session: sid }],
            "Ctrl+Space on a non-paused session pauses it"
        );
    }

    // --- Issue 2: `/help <cmd>` and `/cmd -h`/`--help` ----------------------
    //
    // Both surface the clap-generated help as a transcript status line (never
    // the keybindings dialog). The help text comes from `Command::help_text()`,
    // which renders the clap struct for arg-bearing commands.

    fn transcript_has_notice(app: &App, label: &str, needle: &str) -> bool {
        app.transcript().iter().any(|e| {
            matches!(e, TranscriptEntry::ToolOutput { tool: Some(t), output }
                if t == label && output.contains(needle))
        })
    }

    #[tokio::test]
    async fn help_with_command_arg_renders_clap_help() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        let mut attention = Attention::from_env();
        app.set_input_text("/help compact".to_string());

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Enter)),
        )
        .await
        .unwrap();

        // The clap-rendered /compact help names the --keep flag.
        assert!(
            transcript_has_notice(&app, "help", "--keep"),
            "expected --keep in /help compact output: {:?}",
            app.transcript()
        );
    }

    #[tokio::test]
    async fn cmd_dash_h_renders_clap_help() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        let mut attention = Attention::from_env();
        app.set_input_text("/set -h".to_string());

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Enter)),
        )
        .await
        .unwrap();

        // The clap-rendered /set help names the KEY positional.
        assert!(
            transcript_has_notice(&app, "set", "KEY") || transcript_has_notice(&app, "set", "key"),
            "expected KEY/key in /set -h output: {:?}",
            app.transcript()
        );
    }

    #[tokio::test]
    async fn help_with_unknown_command_falls_through_to_keybindings() {
        // `/help bogus` (no matching command) falls through to the default
        // `/help` behavior (toggle the keybindings dialog), not a status line.
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        let mut attention = Attention::from_env();
        app.set_input_text("/help bogus".to_string());

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Enter)),
        )
        .await
        .unwrap();

        assert!(
            app.showing_help(),
            "/help <unknown> opens the keybindings dialog"
        );
    }

    // --- Issue 2: slash popup (Tab/Up/Down/Enter) ---------------------------
    //
    // The live prefix-filter popup mirrors the mention popup: typing `/co`
    // opens it, Tab/Enter inserts the selected command, Up/Down navigate.

    #[tokio::test]
    async fn typing_slash_opens_the_popup() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        let mut attention = Attention::from_env();

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Char('/'))),
        )
        .await
        .unwrap();
        assert!(app.slash_visible(), "typing / opened the slash popup");

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Char('c'))),
        )
        .await
        .unwrap();
        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Char('o'))),
        )
        .await
        .unwrap();
        assert!(app.slash_visible(), "popup stays open while typing /co");
        // `/co` narrows to compact/continue (both name-prefix matches).
        let names: Vec<&str> = app.slash().matches().iter().map(|c| c.name()).collect();
        assert!(names.contains(&"compact"), "names={names:?}");
    }

    #[tokio::test]
    async fn tab_accepts_the_selected_slash_command() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        let mut attention = Attention::from_env();
        // Type `/comp` → first match is Compact.
        app.set_input_text("/comp".to_string());
        app.update_popups();
        assert!(app.slash_visible());

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Tab)),
        )
        .await
        .unwrap();

        assert!(!app.slash_visible(), "Tab closed the popup");
        assert_eq!(
            app.input_text(),
            "/compact ",
            "Tab inserted the selected command with a trailing space"
        );
    }

    #[tokio::test]
    async fn esc_closes_the_slash_popup() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        let mut attention = Attention::from_env();
        app.set_input_text("/comp".to_string());
        app.update_popups();
        assert!(app.slash_visible());

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Esc)),
        )
        .await
        .unwrap();

        assert!(!app.slash_visible(), "Esc closed the slash popup");
    }

    #[tokio::test]
    async fn down_arrow_navigates_the_slash_popup() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        let mut attention = Attention::from_env();
        app.set_input_text("/".to_string());
        app.update_popups();
        assert!(app.slash_visible());
        let first = app.slash().selected().cloned();

        handle_event(
            &mut app,
            &holly,
            &mut attention,
            Event::Key(key(KeyCode::Down)),
        )
        .await
        .unwrap();

        assert_ne!(
            app.slash().selected().cloned(),
            first,
            "Down moved the selection"
        );
    }
}
