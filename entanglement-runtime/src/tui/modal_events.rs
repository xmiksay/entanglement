use anyhow::Result;
use entanglement_core::{Holly, InMsg};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::app::App;

/// Routes a mouse event. The wheel prefers an open modal's selection (mirroring
/// `j`/`k`), else scrolls the chat transcript; a left click hit-tests the chat
/// area and toggles the reasoning block it lands on.
pub(super) fn handle_mouse(app: &mut App, ev: MouseEvent) {
    match ev.kind {
        MouseEventKind::ScrollUp => {
            if !wheel_modal_prev(app) {
                app.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if !wheel_modal_next(app) {
                app.scroll_down(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) if !any_modal_open(app) => {
            if let Some(id) = app.reasoning_block_at(ev.column, ev.row) {
                app.toggle_reasoning_block(id);
            }
        }
        _ => {}
    }
}

fn any_modal_open(app: &App) -> bool {
    app.showing_sessions_modal()
        || app.showing_profile_picker()
        || app.showing_model_picker()
        || app.showing_key_dialog()
        || app.showing_command_palette()
        || app.showing_resume_modal()
        || app.showing_help()
        || app.showing_inspect()
}

/// Moves the open modal's selection forward for a wheel-down; returns whether a
/// modal consumed the event (so the chat isn't scrolled underneath it).
fn wheel_modal_next(app: &mut App) -> bool {
    if app.showing_sessions_modal() {
        app.sessions_modal_next();
    } else if app.showing_profile_picker() {
        app.profile_picker_next();
    } else if app.showing_model_picker() {
        app.model_picker_next();
    } else if app.showing_key_dialog() {
        app.key_dialog_next();
    } else if app.showing_command_palette() {
        app.command_palette().select_next();
    } else if app.showing_resume_modal() {
        app.resume_next();
    } else if app.showing_inspect() {
        app.inspect_scroll_down(3);
    } else if app.showing_help() {
        // Consume without acting — the help dialog has no selection.
    } else {
        return false;
    }
    true
}

fn wheel_modal_prev(app: &mut App) -> bool {
    if app.showing_sessions_modal() {
        app.sessions_modal_prev();
    } else if app.showing_profile_picker() {
        app.profile_picker_prev();
    } else if app.showing_model_picker() {
        app.model_picker_prev();
    } else if app.showing_key_dialog() {
        app.key_dialog_prev();
    } else if app.showing_command_palette() {
        app.command_palette().select_prev();
    } else if app.showing_resume_modal() {
        app.resume_prev();
    } else if app.showing_inspect() {
        app.inspect_scroll_up(3);
    } else if app.showing_help() {
    } else {
        return false;
    }
    true
}

pub(super) async fn handle_profile_picker_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_profile_picker();
        }
        KeyCode::Enter => {
            if let Some(agent_name) = app.select_profile_picker() {
                let _ = holly
                    .send(entanglement_core::InMsg::SetAgent {
                        session: app.active_session_id().clone(),
                        agent: agent_name,
                    })
                    .await;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.profile_picker_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.profile_picker_prev();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

pub(super) async fn handle_model_picker_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_model_picker();
        }
        KeyCode::Enter => {
            // Realtime switch (#218): send the picked `(provider, model)` to the
            // live engine; the resulting `ModelChanged` updates the context bar.
            // Record it as a pending persist for the active agent (#323) so the
            // confirming `ModelChanged` writes it to `agent-models.yml`.
            if let Some((provider, model)) = app.select_model_picker() {
                app.record_pending_model_persist(provider.clone(), model.clone());
                let _ = holly
                    .send(InMsg::SetModel {
                        session: app.active_session_id().clone(),
                        provider,
                        model,
                    })
                    .await;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.model_picker_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.model_picker_prev();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

/// Drive the two-stage `/key` dialog (#304). Stage 1 picks a provider; stage 2
/// reads the key into a masked buffer and, on Enter, persists it (writer + prime
/// process env + transcript status). No engine traffic — the write is head-side.
pub(super) async fn handle_key_dialog_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    use crate::tui::key_dialog::KeyStage;
    match app.key_dialog_stage() {
        KeyStage::PickProvider => match key.code {
            KeyCode::Esc => app.close_key_dialog(),
            KeyCode::Enter => app.key_dialog_confirm_provider(),
            KeyCode::Down | KeyCode::Char('j') => app.key_dialog_next(),
            KeyCode::Up | KeyCode::Char('k') => app.key_dialog_prev(),
            KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                return Ok(true);
            }
            _ => {}
        },
        KeyStage::EnterKey => match key.code {
            // Esc wipes the buffer and returns to the provider list, never
            // leaving a typed key lingering.
            KeyCode::Esc => app.key_dialog_back(),
            KeyCode::Enter => {
                let _ = app.submit_key_dialog();
            }
            KeyCode::Backspace => app.key_dialog_pop_char(),
            // Ctrl-c/q still quits; other control combos are ignored so they
            // don't land in the key buffer.
            KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                return Ok(true);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.key_dialog_push_char(c);
            }
            _ => {}
        },
    }
    Ok(false)
}

pub(super) async fn handle_sessions_modal_event(app: &mut App, key: KeyEvent) -> Result<bool> {
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
        KeyCode::Char('n') => {
            app.create_session();
            app.close_sessions_modal();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

pub(super) async fn handle_command_palette_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_command_palette();
        }
        KeyCode::Enter => {
            if let Some(cmd) = app.command_palette().execute_selected() {
                if app.execute_command(cmd) {
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
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
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

/// Drives the read-only inspection overlay (#214): `Tab`/`←`/`→` switch tabs,
/// arrows/`j`/`k`/`PgUp`/`PgDn` scroll the current pane, `Esc` closes. No engine
/// traffic — it's a pure view over already-resolved state.
pub(super) async fn handle_inspect_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_inspect();
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            app.inspect_next_tab();
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            app.inspect_prev_tab();
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
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
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
        KeyCode::Enter => {
            if let Some(meta) = app.selected_resume_session() {
                let id = meta.id.clone();
                let cwd = std::env::current_dir().unwrap_or_default();
                match crate::session_store::read(&cwd, &id) {
                    Ok(records) => {
                        // A gap tombstone means the log lost a contiguous run of
                        // events (#104); replaying it would silently rebuild a
                        // wrong context, so refuse rather than resume.
                        if let Some(dropped) = crate::session_store::integrity_gap(&records) {
                            tracing::error!(
                                "Refusing to resume session {}: log is missing {} dropped record(s)",
                                id,
                                dropped
                            );
                        } else {
                            // Visible transcript first, then engine context.
                            app.restore_session(id.clone(), &records);
                            let paired = crate::session_store::pair_records(&records);
                            if let Err(e) = holly.resume(id.clone(), paired).await {
                                tracing::error!("Failed to resume session {}: {}", id, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to read session {}: {}", id, e);
                    }
                }
            }
            app.close_resume_modal();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.resume_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.resume_prev();
        }
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

/// Drive a pending `ask_user` question (ADR-0027): arrow/number selection over
/// the labelled options plus an "Other" entry that opens the shared input box
/// for a free-text answer. The picked label or typed text returns as
/// [`InMsg::AnswerQuestion`]; `Esc` interrupts the turn like an approval.
pub(super) async fn handle_question_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    let Some(q) = app.pending_question() else {
        return Ok(false);
    };
    let request_id = q.request_id.clone();
    let entering = q.entering_free_form;
    let free_form_selected = q.free_form_selected();
    let selected_label = q.options.get(q.selected).map(|o| o.label.clone());

    let session = app.active_session_id().clone();
    let answer = |text: String| InMsg::AnswerQuestion {
        session: session.clone(),
        request_id: request_id.clone(),
        answer: text,
    };

    if entering {
        match key.code {
            KeyCode::Esc => {
                let _ = app.take_input_text();
                app.question_cancel_free_form();
            }
            KeyCode::Enter => {
                let text = app.take_input_text();
                if !text.is_empty() {
                    let _ = holly.send(answer(text)).await;
                    app.advance_question();
                }
            }
            KeyCode::Char(c) => app.input().insert_char(c),
            KeyCode::Backspace => app.input().delete_char(),
            KeyCode::Left => app.input().move_cursor_left(),
            KeyCode::Right => app.input().move_cursor_right(),
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        KeyCode::Up | KeyCode::Char('k') => app.question_move(-1),
        KeyCode::Down | KeyCode::Char('j') => app.question_move(1),
        // Quick-pick by number: options are 1-based; the "Other" entry follows.
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as u8 - b'1') as usize;
            let (opt_count, allow_free_form) = app
                .pending_question()
                .map(|q| (q.options.len(), q.allow_free_form))
                .unwrap_or((0, false));
            if idx < opt_count {
                if let Some(label) = app
                    .pending_question()
                    .and_then(|q| q.options.get(idx).map(|o| o.label.clone()))
                {
                    let _ = holly.send(answer(label)).await;
                    app.advance_question();
                }
            } else if allow_free_form && idx == opt_count {
                app.question_begin_free_form();
            }
        }
        KeyCode::Enter => {
            if free_form_selected {
                app.question_begin_free_form();
            } else if let Some(label) = selected_label {
                let _ = holly.send(answer(label)).await;
                // Answering pops only this question — the next queued one
                // (core batch-emits, #273) surfaces immediately.
                app.advance_question();
            }
        }
        KeyCode::Esc => {
            let _ = holly.send(InMsg::Stop { session }).await;
            app.clear_question();
        }
        _ => {}
    }
    Ok(false)
}
