use anyhow::Result;
use entanglement_core::{Holly, InMsg};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::App;

/// Fixed page size for dialog navigation (`PageUp`/`PageDown`). Dialog
/// viewports are `centered_rect(_, ~40%)` ≈ 8 visible rows on a standard
/// terminal, so one page ≈ one viewport — the intuitive pager contract. A
/// dynamic size would require threading the rendered area into state
/// (over-coupling); the codebase already uses fixed page sizes (main view =
/// 5, inspect = 10).
pub(super) const DIALOG_PAGE_SIZE: usize = 8;

// Split by dialog family; re-exported so `event_loop`'s routing keeps one
// import path.
mod click;
mod mouse;
mod pickers;
mod question;

pub(super) use mouse::handle_mouse;
pub(super) use pickers::{
    handle_key_dialog_event, handle_mode_picker_event, handle_model_picker_event,
    handle_profile_picker_event, handle_session_tools_dialog_event, handle_tools_view_event,
};
pub(super) use question::handle_question_event;

pub(super) async fn handle_sessions_modal_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_sessions_modal();
        }
        KeyCode::Enter => {
            app.select_session_from_modal();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.sessions_modal_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.sessions_modal_prev();
        }
        KeyCode::PageDown => {
            app.sessions_modal_page_down(DIALOG_PAGE_SIZE);
        }
        KeyCode::PageUp => {
            app.sessions_modal_page_up(DIALOG_PAGE_SIZE);
        }
        KeyCode::Char('n') => {
            app.create_session();
            app.close_sessions_modal();
        }
        // Lifecycle quick keys (#6) act on the highlighted session: `s` stops
        // its in-flight turn, `p` pauses it, `r` resumes it. The modal stays
        // open so the user can act on several in a row. No-op on a session in
        // an incompatible state (idempotent server-side). `s` routes through
        // `request_stop` (#626): a highlighted plan session parked on a live
        // sponsored build child arms the cascade-vs-detach confirm (which
        // takes over key dispatch on the next event) instead of detaching
        // right away.
        KeyCode::Char('s') => {
            if let Some(id) = app.modal_selected_session_id() {
                crate::tui::stop_command::request_stop(app, holly, id).await;
            }
        }
        KeyCode::Char('p') => {
            if let Some(id) = app.modal_selected_session_id() {
                let _ = holly.send(InMsg::PauseSession { session: id }).await;
            }
        }
        KeyCode::Char('r') => {
            if let Some(id) = app.modal_selected_session_id() {
                let _ = holly.send(InMsg::ResumeSession { session: id }).await;
            }
        }
        // `d`/`Delete` (Issue 4, Phase 4.1): delete the highlighted session's
        // `.jsonl`. Refuses a live session with a status line — the modal lists
        // the live set, so deleting underneath one would orphan its view. The
        // modal stays open so several can be deleted in a row.
        KeyCode::Char('d') | KeyCode::Delete => {
            app.delete_session_from_modal();
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

pub(super) async fn handle_command_palette_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_command_palette();
        }
        KeyCode::Enter => {
            if let Some(cmd) = app.command_palette().execute_selected() {
                // The palette carries no trailing free text, so a picked
                // `/compact` always runs with empty `args` (#324) — unlike
                // typing `/compact <instructions>` and pressing Enter directly
                // (`event_loop::send_compact`), which this dispatch can't
                // reach since it needs `holly` + the raw input text together.
                if cmd == crate::tui::commands::Command::Compact {
                    let _ = holly
                        .send(InMsg::Oneshot {
                            session: app.active_session_id().clone(),
                            op: "compact".to_string(),
                            args: serde_json::Value::Object(serde_json::Map::new()),
                        })
                        .await;
                } else if cmd == crate::tui::commands::Command::Set {
                    // No trailing `key value` text — bare `/set` is the dialog.
                    app.open_settings_dialog();
                } else if cmd == crate::tui::commands::Command::Show {
                    let _ = holly
                        .send(InMsg::SetGeneration {
                            session: app.active_session_id().clone(),
                            overrides: entanglement_core::GenerationParams::default(),
                        })
                        .await;
                } else if cmd == crate::tui::commands::Command::Mcp {
                    // The palette carries no trailing `add`/`remove` args
                    // (#373), so a picked `/mcp` always runs `list` — the same
                    // default a bare typed `/mcp` falls back to.
                    super::mcp_command::send_mcp_list(app, holly).await;
                } else if cmd == crate::tui::commands::Command::Allow {
                    // A bare `/allow` has no path to grant (#486): prefill the
                    // input so Enter routes through `allow_command::send_allow`.
                    app.set_input_text("/allow ".to_string());
                } else if cmd == crate::tui::commands::Command::Enable {
                    // The palette carries no trailing args (#539), so a picked
                    // `/enable` opens the session-tools checklist — the same
                    // default a bare typed `/enable` falls back to.
                    app.open_session_tools_dialog();
                } else if cmd == crate::tui::commands::Command::Disable {
                    // A bare `/disable` clears the whole overlay — prefill the
                    // input instead so a palette pick can't wipe it by accident
                    // (the `/allow` "no sensible default" reasoning, #486).
                    app.set_input_text("/disable ".to_string());
                } else if cmd == crate::tui::commands::Command::Stop {
                    // Lifecycle commands (#6): the palette carries no `--all`
                    // flag, so a picked `/stop` acts on the active session only
                    // — the same default a bare typed `/stop` falls back to.
                    // Routed through `request_stop` (#626) like every other
                    // single-target `Stop` site.
                    let target = app.active_session_id().clone();
                    crate::tui::stop_command::request_stop(app, holly, target).await;
                } else if cmd == crate::tui::commands::Command::Pause {
                    let _ = holly
                        .send(InMsg::PauseSession {
                            session: app.active_session_id().clone(),
                        })
                        .await;
                } else if cmd == crate::tui::commands::Command::Continue {
                    let _ = holly
                        .send(InMsg::ResumeSession {
                            session: app.active_session_id().clone(),
                        })
                        .await;
                } else if app.execute_command(cmd) {
                    return Ok(true);
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.command_palette().select_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.command_palette().select_prev();
        }
        KeyCode::PageDown => {
            app.command_palette().page_down(DIALOG_PAGE_SIZE);
        }
        KeyCode::PageUp => {
            app.command_palette().page_up(DIALOG_PAGE_SIZE);
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        KeyCode::Char(c) => {
            let mut query = app.command_palette().query().to_string();
            query.push(c);
            app.command_palette().set_query(query);
        }
        KeyCode::Backspace => {
            let mut query = app.command_palette().query().to_string();
            query.pop();
            app.command_palette().set_query(query);
        }
        _ => {}
    }
    Ok(false)
}

/// Drives the read-only inspection overlay (#214, drill-down #331): `Tab`/`←`/
/// `→` switch tabs from either level; on the **list** level arrows/`j`/`k` move
/// the highlight, `Enter` opens the per-item detail, `Esc` closes; on the
/// **detail** level arrows/`j`/`k`/`PgUp`/`PgDn` scroll, `Esc`/`Backspace`
/// returns to the list (and a second `Esc` closes). The Prompt tab is always a
/// scroll-only document. No engine traffic — it's a pure view over
/// already-resolved state.
pub(super) async fn handle_inspect_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        // `Esc` is level-aware (#331): from the detail pane it returns to the
        // list; from the list (or the scroll-only Prompt tab) it closes the
        // overlay as before. `inspect_showing_list()` is true only on the list
        // level of a two-level tab, so its negation on a list-capable tab means
        // the detail pane is open.
        KeyCode::Esc => {
            if app.inspect_tab().list_tab().is_some() && !app.inspect_showing_list() {
                app.inspect_back_to_list();
            } else {
                app.close_inspect();
            }
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            app.inspect_next_tab();
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            app.inspect_prev_tab();
        }
        // On the list level, `Enter` opens the highlighted item's detail (#331);
        // on the detail/Prompt level `Enter` is a no-op (the scroll-only
        // document has nothing to drill into).
        KeyCode::Enter if app.inspect_showing_list() => {
            app.inspect_open_detail();
        }
        // `Backspace` returns from the detail pane to the list (#331); a no-op
        // on the list level (where `Esc` closes) and the Prompt tab.
        KeyCode::Backspace => {
            app.inspect_back_to_list();
        }
        // Vertical movement is level-aware: on the list level it moves the
        // highlight, on the detail/Prompt level it scrolls the document.
        KeyCode::Down | KeyCode::Char('j') if app.inspect_showing_list() => {
            app.inspect_list_down(1);
        }
        KeyCode::Up | KeyCode::Char('k') if app.inspect_showing_list() => {
            app.inspect_list_up(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.inspect_scroll_down(1);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.inspect_scroll_up(1);
        }
        KeyCode::PageDown => {
            app.inspect_scroll_down(10);
        }
        KeyCode::PageUp => {
            app.inspect_scroll_up(10);
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

/// Drives the resume modal: navigate the past-session list and, on Enter,
/// restore the picked session's full transcript into a fresh view and reseed the
/// engine's context from the same log (`Holly::resume`). Read/resume failures are
/// logged, not fatal — the modal simply closes.
pub(super) async fn handle_resume_modal_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_resume_modal();
        }
        KeyCode::Enter => click::resume_selected(app, holly).await,
        KeyCode::Down | KeyCode::Char('j') => {
            app.resume_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.resume_prev();
        }
        KeyCode::PageDown => {
            app.resume_page_down(DIALOG_PAGE_SIZE);
        }
        KeyCode::PageUp => {
            app.resume_page_up(DIALOG_PAGE_SIZE);
        }
        // `d`/`Delete` (Issue 4, Phase 4.1): delete the highlighted past
        // session's `.jsonl`. The resume modal only lists past (non-live)
        // sessions, so `d` here always deletes — no live-set guard. The modal
        // stays open; the entry is dropped from the list.
        KeyCode::Char('d') | KeyCode::Delete => {
            app.delete_resume_session();
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

#[cfg(test)]
mod tests {

    use super::*;
    use entanglement_core::{
        EngineConfig, OutEvent, Question, QuestionOption, Questions, SessionId,
    };
    use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{backend::TestBackend, Terminal};

    fn engine() -> Holly {
        Holly::spawn(EngineConfig::default())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn two_question_call(request_id: &str, session: &SessionId) -> OutEvent {
        OutEvent::UserQuestion {
            session: session.clone(),
            seq: 1,
            request_id: request_id.into(),
            questions: Questions(vec![
                Question {
                    question: "Which DB?".into(),
                    options: vec![
                        QuestionOption {
                            label: "Postgres".into(),
                            description: None,
                        },
                        QuestionOption {
                            label: "MySQL".into(),
                            description: None,
                        },
                    ],
                    multi_select: false,
                },
                Question {
                    question: "Which region?".into(),
                    options: vec![QuestionOption {
                        label: "us-east".into(),
                        description: None,
                    }],
                    multi_select: false,
                },
            ]),
        }
    }

    /// #518: answers are drafts across the batch — revising the first question
    /// after stepping back changes what the eventual `AnswerQuestion` carries,
    /// and nothing is sent to the engine until the explicit Submit at the
    /// review step. Neither `commit_current_answer` nor `question_retreat`
    /// take a `Holly` handle at all, so this isn't a timing race: no `send`
    /// call can have happened before the assertion.
    #[tokio::test]
    async fn draft_revise_then_submit_sends_only_the_final_answers() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut inbound = holly.subscribe_inbound();
        app.handle_out_event(two_question_call("q1", &sid));

        // Answer both questions, reaching the review step.
        handle_question_event(&mut app, &holly, key(KeyCode::Char('1')))
            .await
            .unwrap(); // Postgres
        handle_question_event(&mut app, &holly, key(KeyCode::Char('1')))
            .await
            .unwrap(); // us-east
        assert!(app.pending_question().unwrap().is_reviewing());
        assert!(
            inbound.try_recv().is_err(),
            "no AnswerQuestion before Submit"
        );

        // Step back to the first question and change the answer.
        handle_question_event(&mut app, &holly, key(KeyCode::Left))
            .await
            .unwrap(); // back to the review step's last question
        handle_question_event(&mut app, &holly, key(KeyCode::Left))
            .await
            .unwrap(); // back to the first question
        handle_question_event(&mut app, &holly, key(KeyCode::Char('2')))
            .await
            .unwrap(); // MySQL instead
        handle_question_event(&mut app, &holly, key(KeyCode::Char('1')))
            .await
            .unwrap(); // us-east again, reaching review
        assert!(app.pending_question().unwrap().is_reviewing());
        assert!(
            inbound.try_recv().is_err(),
            "still nothing sent — revising a draft never sends by itself"
        );

        // The explicit Submit sends the one AnswerQuestion frame, reflecting
        // the revised first answer.
        handle_question_event(&mut app, &holly, key(KeyCode::Enter))
            .await
            .unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), inbound.recv())
            .await
            .expect("AnswerQuestion arrives on the inbound fan-out")
            .unwrap();
        match msg {
            InMsg::AnswerQuestion {
                request_id,
                answers,
                ..
            } => {
                assert_eq!(request_id, "q1");
                assert_eq!(
                    answers,
                    vec![vec!["MySQL".to_string()], vec!["us-east".to_string()]]
                );
            }
            other => panic!("expected AnswerQuestion, got {other:?}"),
        }
        assert!(!app.is_asking(), "the call is popped once submitted");
    }

    /// #518: cancelling out of the review step (`Esc`) must not send anything
    /// and must leave the call parked, revisable — distinct from a mid-question
    /// `Esc`, which still interrupts the turn (unchanged by this issue).
    #[tokio::test]
    async fn esc_at_the_review_step_sends_nothing_and_keeps_the_call_parked() {
        let sid = SessionId::new("s1");
        let mut app = App::new_for_test(sid.clone());
        let holly = engine();
        let mut inbound = holly.subscribe_inbound();
        app.handle_out_event(two_question_call("q1", &sid));

        handle_question_event(&mut app, &holly, key(KeyCode::Char('1')))
            .await
            .unwrap();
        handle_question_event(&mut app, &holly, key(KeyCode::Char('1')))
            .await
            .unwrap();
        assert!(app.pending_question().unwrap().is_reviewing());

        handle_question_event(&mut app, &holly, key(KeyCode::Esc))
            .await
            .unwrap();

        assert!(
            inbound.try_recv().is_err(),
            "Esc at the review step sends nothing"
        );
        assert!(app.is_asking(), "the call stays parked, not cleared");
        assert!(
            !app.pending_question().unwrap().is_reviewing(),
            "Esc stepped back to revise rather than cancelling the whole call"
        );
    }

    /// A bare left click (down + up, no drag) on a sidebar session row must
    /// switch the active session; the attention panel must route a click to
    /// the jump. Rendered through the real `ui::draw` so the hit-test rects
    /// are the ones a live frame records.
    #[tokio::test]
    async fn click_on_sidebar_row_switches_session_and_panel_click_jumps() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active.clone());
        let holly = engine();
        let bg = SessionId::new("bg-session");
        app.handle_out_event(OutEvent::ToolRequest {
            session: bg.clone(),
            seq: 1,
            request_id: "r1".to_string(),
            tool: "bash".to_string(),
            input: r#"{"command":"ls"}"#.to_string(),
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| crate::tui::ui::draw(f, &mut app))
            .unwrap();

        // The background session's row is the second session line in the
        // sidebar; resolve its coordinates through the recorded hit-test map
        // rather than hardcoding layout math.
        let (col, row) = (0..80u16)
            .flat_map(|x| (0..24u16).map(move |y| (x, y)))
            .find(|(x, y)| app.session_at(*x, *y) == Some(bg.clone()))
            .expect("background session row is clickable");

        let click = |kind| MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(
            &mut app,
            &holly,
            click(MouseEventKind::Down(MouseButton::Left)),
        )
        .await;
        handle_mouse(
            &mut app,
            &holly,
            click(MouseEventKind::Up(MouseButton::Left)),
        )
        .await;
        assert_eq!(app.active_session_id(), &bg, "click switched the session");

        // Switch back, then click the attention panel: it jumps to the
        // waiting background session.
        app.switch_to_session(active.clone());
        terminal
            .draw(|f| crate::tui::ui::draw(f, &mut app))
            .unwrap();
        let (pcol, prow) = (0..80u16)
            .flat_map(|x| (0..24u16).map(move |y| (x, y)))
            .find(|(x, y)| app.attention_at(*x, *y))
            .expect("attention panel is visible while a background session waits");
        let click = |kind| MouseEvent {
            kind,
            column: pcol,
            row: prow,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(
            &mut app,
            &holly,
            click(MouseEventKind::Down(MouseButton::Left)),
        )
        .await;
        handle_mouse(
            &mut app,
            &holly,
            click(MouseEventKind::Up(MouseButton::Left)),
        )
        .await;
        assert_eq!(app.active_session_id(), &bg, "panel click jumped");
    }

    /// Issue 1: a left-click on a row inside the open sessions modal switches
    /// the active session to the clicked row (the same action `Enter` fires),
    /// and a click outside the modal's rect closes it. Rendered through the
    /// real `ui::draw` so the hit-test rect is the one a live frame records.
    #[tokio::test]
    async fn click_on_sessions_modal_row_switches_and_outside_closes() {
        let active = SessionId::new("active-1");
        let mut app = App::new_for_test(active.clone());
        let holly = engine();
        let other = app.create_session(); // a second live session to click
        app.toggle_sessions_modal();

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| crate::tui::ui::draw(f, &mut app))
            .unwrap();

        // Resolve the second session's row coordinates through the modal's
        // captured rect + the sessions-with-depth order, rather than
        // hardcoding layout math.
        let area = app.sessions_modal_rect();
        let len = app.sessions_with_depth().len();
        assert!(len >= 2, "expected at least two sessions in the modal");
        // The second item's row is the inner y + 1 (index 1).
        let inner_y = area.y + 1;
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 2,
            row: inner_y + 1,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, &holly, click).await;
        // `select_session_from_modal` closes the modal (same as Enter), so a
        // click that selects both switches the session and closes the modal.
        assert!(
            !app.showing_sessions_modal(),
            "a select click closes the modal (matches Enter)"
        );
        assert_eq!(
            app.active_session_id(),
            &other,
            "clicking the second row switched to that session"
        );

        // A click outside the modal's rect closes it (click-outside-to-close).
        app.switch_to_session(active);
        app.toggle_sessions_modal();
        terminal
            .draw(|f| crate::tui::ui::draw(f, &mut app))
            .unwrap();
        let area = app.sessions_modal_rect();
        // A click at the top-left corner of the screen is well outside a
        // centered 60%×40% modal.
        let outside = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        debug_assert!(
            !area.contains(ratatui::layout::Position::new(0, 0)),
            "sanity: (0,0) must be outside the centered modal"
        );
        handle_mouse(&mut app, &holly, outside).await;
        assert!(
            !app.showing_sessions_modal(),
            "a click outside the modal closes it"
        );
    }

    /// Issue 1: a left-click on a row inside the open command palette executes
    /// the clicked command (the same dispatch `Enter` fires). Rendered through
    /// the real `ui::draw` so the list-chunk rect is the one a live frame
    /// records.
    #[tokio::test]
    async fn click_on_command_palette_row_executes_it() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        // `/help` is the first filtered command in a fresh palette (see
        // `CommandPalette` tests); executing it opens the Help dialog — a
        // pure head-side effect, easy to assert.
        app.toggle_command_palette();
        assert_eq!(
            app.command_palette().filtered_commands().first(),
            Some(&crate::tui::commands::Command::Help)
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| crate::tui::ui::draw(f, &mut app))
            .unwrap();

        let area = app.command_palette_rect();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 2,
            row: area.y + 1, // first list row (border + 1)
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, &holly, click).await;
        assert!(
            !app.showing_command_palette(),
            "executing a palette command closes the palette"
        );
        assert!(
            app.showing_help(),
            "clicking the first row executed /help (opened the Help dialog)"
        );
    }

    /// Issue 1: a left-click on a row inside the `/cmd` slash-autocomplete
    /// popup inserts the clicked command (the same action Tab/Enter fires).
    #[tokio::test]
    async fn click_on_slash_popup_row_inserts_command() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        let holly = engine();
        // `/comp` narrows the popup to `compact` as the first match.
        app.input().insert_str("/comp");
        app.update_popups();
        assert!(app.slash_visible(), "sanity: slash popup is visible");
        assert_eq!(
            app.slash().matches().first(),
            Some(&crate::tui::commands::Command::Compact),
            "sanity: /comp narrows to compact first"
        );

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| crate::tui::ui::draw(f, &mut app))
            .unwrap();

        let area = app.slash_popup_rect();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 2,
            row: area.y + 1, // first match row
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse(&mut app, &holly, click).await;
        assert!(
            !app.slash_visible(),
            "accepting a slash command hides the popup"
        );
        let text = app.input().lines().join("\n");
        assert_eq!(
            text, "/compact ",
            "the clicked first match (/compact) was inserted"
        );
    }
}
