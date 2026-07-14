use anyhow::Result;
use entanglement_core::{ApprovalScope, Holly, InMsg, SessionId};
use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use tracing::debug;

use super::app::App;
use super::attention::Attention;
use super::event::Event;
use super::modal_events::{
    handle_command_palette_event, handle_inspect_event, handle_model_picker_event, handle_mouse,
    handle_profile_picker_event, handle_question_event, handle_resume_modal_event,
    handle_sessions_modal_event,
};
use super::session_view::ApprovalMode;

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
                if app.showing_sessions_modal() {
                    return handle_sessions_modal_event(app, key).await;
                }
                if app.showing_profile_picker() {
                    return handle_profile_picker_event(app, holly, key).await;
                }
                if app.showing_model_picker() {
                    return handle_model_picker_event(app, key).await;
                }
                if app.showing_help() {
                    if key.code == KeyCode::Esc {
                        app.close_help();
                    }
                    return Ok(false);
                }
                if app.showing_command_palette() {
                    return handle_command_palette_event(app, key).await;
                }
                if app.showing_resume_modal() {
                    return handle_resume_modal_event(app, holly, key).await;
                }
                if app.showing_inspect() {
                    return handle_inspect_event(app, key).await;
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
                    if let Some(action) = app.leader_handler().handle_key(&key) {
                        if app.dispatch_action(action) {
                            return Ok(true);
                        }
                        return Ok(false);
                    }
                }

                match current_mode {
                    ApprovalMode::WaitingForApproval { request_id } => match key.code {
                        // Approve scopes (#174): `y` this once, `s` for the rest of
                        // the session, `a` always (persisted). All three share the
                        // plan-handoff path — scope is inert for `propose_plan`.
                        KeyCode::Char('y') => {
                            send_approval(app, holly, request_id.clone(), ApprovalScope::Once)
                                .await;
                        }
                        KeyCode::Char('s') => {
                            send_approval(app, holly, request_id.clone(), ApprovalScope::Session)
                                .await;
                        }
                        KeyCode::Char('a') => {
                            send_approval(app, holly, request_id.clone(), ApprovalScope::Always)
                                .await;
                        }
                        KeyCode::Char('n') => {
                            app.set_approval_mode(ApprovalMode::EnteringRejectReason {
                                request_id: request_id.clone(),
                            });
                        }
                        KeyCode::Char('e') => {
                            app.set_approval_mode(ApprovalMode::EnteringRejectReason {
                                request_id: request_id.clone(),
                            });
                        }
                        KeyCode::Esc => {
                            let _ = holly
                                .send(InMsg::Stop {
                                    session: app.active_session_id().clone(),
                                })
                                .await;
                            app.clear_approval();
                        }
                        _ => {}
                    },
                    ApprovalMode::EnteringRejectReason { request_id } => match key.code {
                        KeyCode::Esc => {
                            app.set_approval_mode(ApprovalMode::WaitingForApproval {
                                request_id: request_id.clone(),
                            });
                            let text = app.take_input_text();
                            if !text.is_empty() {
                                app.input().insert_str(&text);
                            }
                        }
                        KeyCode::Enter => {
                            let text = app.take_input_text();
                            let _ = holly
                                .send(InMsg::Reject {
                                    session: app.active_session_id().clone(),
                                    request_id: request_id.clone(),
                                    reason: if text.is_empty() { None } else { Some(text) },
                                })
                                .await;
                            app.clear_approval();
                        }
                        KeyCode::Char(c) => {
                            app.input().insert_char(c);
                        }
                        KeyCode::Backspace => {
                            app.input().delete_char();
                        }
                        KeyCode::Left => {
                            app.input().move_cursor_left();
                        }
                        KeyCode::Right => {
                            app.input().move_cursor_right();
                        }
                        _ => {}
                    },
                    ApprovalMode::Normal => match key.code {
                        KeyCode::Tab if app.mention_visible() => {
                            app.accept_mention();
                        }
                        KeyCode::Tab => {
                            let input_text = app.input().lines().join("\n");
                            if input_text.starts_with('/') && input_text.chars().count() == 1 {
                                app.toggle_command_palette();
                            } else if let Some(agent_name) = app.cycle_primary_profile() {
                                let _ = holly
                                    .send(entanglement_core::InMsg::SetAgent {
                                        session: app.active_session_id().clone(),
                                        agent: agent_name,
                                    })
                                    .await;
                            }
                        }
                        KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                            app.toggle_profile_picker();
                        }
                        KeyCode::Char('p') if key.modifiers == KeyModifiers::CONTROL => {
                            app.toggle_command_palette();
                        }
                        KeyCode::Char('q') | KeyCode::Char('c')
                            if key.modifiers == KeyModifiers::CONTROL =>
                        {
                            return Ok(true);
                        }
                        KeyCode::PageUp => {
                            app.scroll_up(5);
                        }
                        KeyCode::PageDown => {
                            app.scroll_down(5);
                        }
                        KeyCode::End => {
                            app.scroll_to_bottom();
                        }
                        KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            app.scroll_left(10);
                        }
                        KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            app.scroll_right(10);
                        }
                        KeyCode::Esc => {
                            if app.mention_visible() {
                                app.hide_mention();
                            } else if app.is_input_multiline() {
                                app.set_input_multiline(false);
                            } else {
                                return Ok(true);
                            }
                        }
                        KeyCode::Enter => {
                            if key.modifiers.contains(KeyModifiers::SHIFT) {
                                app.input().insert_newline();
                                app.update_mention();
                            } else if app.mention_visible() {
                                app.accept_mention();
                            } else {
                                let text = app.take_input_text();
                                if !text.is_empty() {
                                    if text.starts_with('/') {
                                        if let Some(cmd) =
                                            crate::tui::commands::parse_command(&text)
                                        {
                                            if app.execute_command(cmd) {
                                                return Ok(true);
                                            }
                                            return Ok(false);
                                        }
                                    }
                                    // `!bash` passthrough (ADR-0030): run head-side,
                                    // inject output locally — never sent to the engine.
                                    if let Some(cmd) = text.strip_prefix('!') {
                                        let cmd = cmd.trim().to_string();
                                        if !cmd.is_empty() {
                                            run_bash_passthrough(app, &cmd).await;
                                        }
                                        return Ok(false);
                                    }
                                    app.record_user_message(text.clone());
                                    if let Err(e) = holly
                                        .send(InMsg::Prompt {
                                            session: app.active_session_id().clone(),
                                            text,
                                        })
                                        .await
                                    {
                                        debug!("Failed to send prompt: {}", e);
                                    }
                                }
                            }
                        }
                        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().insert_newline();
                            app.update_mention();
                        }
                        KeyCode::Up => {
                            if app.mention_visible() {
                                app.mention_select_prev();
                            } else if app.input().cursor() == (0, 0) {
                                app.history_up();
                            } else {
                                app.input().move_cursor_up();
                            }
                        }
                        KeyCode::Down => {
                            if app.mention_visible() {
                                app.mention_select_next();
                            } else if app.input().cursor() == (0, 0) {
                                app.history_down();
                            } else {
                                app.input().move_cursor_down();
                            }
                        }
                        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            if !app.handle_readline_key(c) {
                                match c {
                                    'a' => app.input().move_cursor_to_head(),
                                    'e' => app.input().move_cursor_to_end(),
                                    'k' => app.input().delete_line_by_end(),
                                    'u' => app.input().delete_line_by_head(),
                                    'w' => app.input().delete_word(),
                                    _ => app.input().insert_char(c),
                                }
                            }
                            app.update_mention();
                        }
                        KeyCode::Char(c) => {
                            app.input().insert_char(c);
                            app.update_mention();
                        }
                        KeyCode::Backspace => {
                            app.input().delete_char();
                            app.update_mention();
                        }
                        KeyCode::Left => {
                            app.input().move_cursor_left();
                            app.update_mention();
                        }
                        KeyCode::Right => {
                            app.input().move_cursor_right();
                            app.update_mention();
                        }
                        _ => {}
                    },
                }
            }
        }
        Event::Mouse(mouse_event) => handle_mouse(app, mouse_event),
        Event::Resize => {}
        Event::FocusGained => attention.set_focused(true),
        Event::FocusLost => attention.set_focused(false),
        Event::Paste(s) => {
            if matches!(app.approval_mode(), ApprovalMode::Normal) {
                app.input().insert_str(&s);
                app.update_mention();
            }
        }
    }
    Ok(false)
}

/// Send an [`InMsg::Approve`] with the chosen [`ApprovalScope`] (#174) and clear
/// the prompt. Captures a `propose_plan` handoff *before* approving clears the
/// pending request — accepting a plan mints a fresh root `build` session whose
/// first message is the plan (#141, ADR-0042; zero new protocol surface, the
/// handoff is head policy). Scope is inert for `propose_plan` (the runtime
/// records grants only on the generic tool path), so all three keys route here.
async fn send_approval(app: &mut App, holly: &Holly, request_id: String, scope: ApprovalScope) {
    let handoff = app.pending_tool_request().and_then(|(_, tool, input)| {
        (tool == crate::tool_names::PROPOSE_PLAN_TOOL)
            .then(|| crate::propose_plan::parse_plan(input))
    });
    let _ = holly
        .send(InMsg::Approve {
            session: app.active_session_id().clone(),
            request_id,
            scope,
        })
        .await;
    app.set_approval_mode(ApprovalMode::Normal);
    if let Some(plan) = handoff {
        handoff_accepted_plan(app, holly, plan).await;
    }
}

/// Perform the plan-acceptance handoff (#141, ADR-0042): mint a fresh **root**
/// `build` session whose first user message is the accepted plan, then switch the
/// view to it. Modelled as head policy — no new protocol surface — so pipe/WS
/// heads implement the identical recipe. The build session is a root (not a child
/// of the plan session) so it is never clamped to plan's read-only tool set nor
/// charged against the plan root's spawn budget; accept is a transfer of authority
/// *from the user*.
async fn handoff_accepted_plan(app: &mut App, holly: &Holly, plan: String) {
    let new_session = SessionId::new_uuid();
    // Lazy session creation: SetAgent on a fresh id starts a root session under
    // the requested profile (holly.rs); the Prompt then runs its first turn.
    let _ = holly
        .send(InMsg::SetAgent {
            session: new_session.clone(),
            agent: crate::propose_plan::HANDOFF_PROFILE.to_string(),
        })
        .await;
    let text = crate::propose_plan::wrap_plan(&plan);
    let _ = holly
        .send(InMsg::Prompt {
            session: new_session.clone(),
            text: text.clone(),
        })
        .await;
    // Adopt the fresh id head-side and switch to it, then mirror the first user
    // message locally (the engine never echoes `InMsg::Prompt` as an `OutEvent`).
    app.adopt_session(new_session);
    app.record_user_message(text);
}

/// Runs a `!bash` passthrough command head-side and injects the output into the
/// transcript (ADR-0030). Gated on `ENTANGLEMENT_ENABLE_BASH` — the same opt-in
/// as the model-facing `bash` tool (ADR-0010), since it runs unsandboxed. When
/// disabled, a hint is recorded instead of running anything.
async fn run_bash_passthrough(app: &mut App, command: &str) {
    if !app.bash_enabled() {
        app.record_bash_passthrough(
            command.to_string(),
            "[bash passthrough disabled] set ENTANGLEMENT_ENABLE_BASH=1 to run `!` commands"
                .to_string(),
        );
        return;
    }
    use entanglement_runtime::Tool;
    let tool = crate::host::bash::BashTool::new(app.root().to_path_buf());
    let input = serde_json::json!({ "command": command }).to_string();
    let output = match tool.run(&input).await {
        Ok(out) => out,
        Err(e) => format!("[bash error] {e:#}"),
    };
    app.record_bash_passthrough(command.to_string(), output);
}
