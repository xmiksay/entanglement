use anyhow::Result;
use entanglement_core::{ApprovalScope, Holly, InMsg, SessionId};
use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use tracing::debug;

use super::app::App;
use super::attention::Attention;
use super::event::Event;
use super::keybindings::LeaderResult;
use super::modal_events::{
    handle_command_palette_event, handle_inspect_event, handle_key_dialog_event,
    handle_model_picker_event, handle_mouse, handle_profile_picker_event, handle_question_event,
    handle_resume_modal_event, handle_sessions_modal_event, handle_tools_dialog_event,
};
use super::session_view::ApprovalMode;

/// Shared input-edit keys for any `SimpleInput`-driven field: Ctrl+Left/Right
/// word jumps, plain + Ctrl Home/End, Alt+Enter newline. Returns whether the key
/// was consumed so callers can fall back to their own bindings (e.g. the Normal
/// `Enter` = send). Kept free of `holly`/mention side effects; the Normal path
/// re-runs `update_mention` after a mutation, the reject/answer paths don't need it.
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
            app.update_mention();
            true
        }
        // Ctrl+Left/Right word jumps.
        KeyCode::Left if mods.contains(KeyModifiers::CONTROL) => {
            app.input().move_word_left();
            app.update_mention();
            true
        }
        KeyCode::Right if mods.contains(KeyModifiers::CONTROL) => {
            app.input().move_word_right();
            app.update_mention();
            true
        }
        // Ctrl+Home/End document jumps; plain Home/End line jumps.
        KeyCode::Home if mods.contains(KeyModifiers::CONTROL) => {
            app.input().move_to_doc_home();
            app.update_mention();
            true
        }
        KeyCode::End if mods.contains(KeyModifiers::CONTROL) => {
            app.input().move_to_doc_end();
            app.update_mention();
            true
        }
        KeyCode::Home => {
            app.input().move_cursor_to_head();
            app.update_mention();
            true
        }
        KeyCode::End => {
            app.input().move_cursor_to_end();
            app.update_mention();
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
                    return handle_sessions_modal_event(app, key).await;
                }
                // Checked before the profile picker: `e` opens the tools dialog
                // *over* the picker without closing it (#330), so it must win the
                // routing while both are marked open.
                if app.showing_tools_dialog() {
                    return handle_tools_dialog_event(app, key).await;
                }
                if app.showing_profile_picker() {
                    return handle_profile_picker_event(app, holly, key).await;
                }
                if app.showing_model_picker() {
                    return handle_model_picker_event(app, holly, key).await;
                }
                if app.showing_key_dialog() {
                    return handle_key_dialog_event(app, key).await;
                }
                if app.showing_help() {
                    if key.code == KeyCode::Esc {
                        app.close_help();
                    }
                    return Ok(false);
                }
                // `/mcp list` panel (#373): read-only, `Esc` is the only key it
                // consumes — mirrors the help dialog's shape.
                if app.showing_mcp_panel() {
                    if key.code == KeyCode::Esc {
                        app.close_mcp_panel();
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
                        // Allow the call's directory for the rest of the session
                        // (#486, ADR-0126) — only offered while the pending call
                        // is one of the read-only triad (`read`/`grep`/`glob`);
                        // any other tool has no `[d]` to press.
                        KeyCode::Char('d')
                            if app.pending_tool_request().is_some_and(|(_, tool, _)| {
                                crate::tool_names::is_read_capability_member(tool)
                            }) =>
                        {
                            send_approval(
                                app,
                                holly,
                                request_id.clone(),
                                ApprovalScope::SessionDir,
                            )
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
                    ApprovalMode::EnteringRejectReason { request_id } => {
                        // Shared input-edit keys (Ctrl+arrows, Home/End, doc
                        // jumps, Alt+Enter newline) — Enter stays = send.
                        if apply_input_edit_key(app, &key) {
                            return Ok(false);
                        }
                        match key.code {
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
                                let tool =
                                    app.pending_tool_request().map(|(_, tool, _)| tool.clone());
                                let reason = if text.is_empty() { None } else { Some(text) };
                                let _ = holly
                                    .send(InMsg::Reject {
                                        session: app.active_session_id().clone(),
                                        request_id: request_id.clone(),
                                        reason: reason.clone(),
                                    })
                                    .await;
                                // Rejecting answers only this request — parked ones
                                // still need their own decision (#273).
                                app.advance_approval();
                                if let Some(tool) = tool {
                                    record_rejected(app, &tool, &reason);
                                }
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
                        }
                    }
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
                        // crossterm reports Shift+Tab as `BackTab` (the SHIFT
                        // modifier is not guaranteed), so match the key code, not
                        // a modifier. Mirrors the Tab arm in reverse (#322).
                        KeyCode::BackTab if app.mention_visible() => {
                            app.accept_mention();
                        }
                        KeyCode::BackTab => {
                            let input_text = app.input().lines().join("\n");
                            if input_text.starts_with('/') && input_text.chars().count() == 1 {
                                app.toggle_command_palette();
                            } else if let Some(agent_name) = app.cycle_primary_profile_back() {
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
                        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
                            return Ok(true);
                        }
                        KeyCode::PageUp => {
                            app.scroll_up(5);
                        }
                        KeyCode::PageDown => {
                            app.scroll_down(5);
                        }
                        KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().move_word_left();
                            app.update_mention();
                        }
                        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().move_word_right();
                            app.update_mention();
                        }
                        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().move_to_doc_home();
                            app.update_mention();
                        }
                        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().move_to_doc_end();
                            app.update_mention();
                        }
                        KeyCode::Home => {
                            app.input().move_cursor_to_head();
                            app.update_mention();
                        }
                        KeyCode::End => {
                            app.input().move_cursor_to_end();
                            app.update_mention();
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
                            // Alt+Enter / Shift+Enter insert a newline (D1):
                            // Alt prefixes an ESC on virtually all vt100+
                            // terminals (universally detected), Shift works on
                            // kitty-protocol terminals — both fall through to
                            // the shared newline path.
                            if key.modifiers.contains(KeyModifiers::ALT)
                                || key.modifiers.contains(KeyModifiers::SHIFT)
                            {
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
                                            // `/compact` needs both the trailing
                                            // free text (→ `args.instructions`)
                                            // and `holly` to send the oneshot op
                                            // — neither is available to the sync
                                            // `execute_command` dispatch other
                                            // commands use, so it's handled here.
                                            if cmd == crate::tui::commands::Command::Compact {
                                                send_compact(app, holly, &text).await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::Set {
                                                send_set(app, holly, &text).await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::Show {
                                                send_show(app, holly).await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::Mcp {
                                                crate::tui::mcp_command::send_mcp(
                                                    app, holly, &text,
                                                )
                                                .await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::Allow {
                                                crate::tui::allow_command::send_allow(app, &text);
                                                return Ok(false);
                                            }
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
                                        .send(InMsg::prompt(app.active_session_id().clone(), text))
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
        // External SIGINT (ADR-0087): route through the same two-stage path as
        // an in-app Ctrl+C so an out-of-band signal never leaves the terminal
        // in raw mode (the "half killed" state).
        Event::Interrupt => {
            if app.handle_quit_key() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Send `/compact [--keep N] [instructions]` as an [`InMsg::Oneshot`]
/// `"compact"` op (#324, ADR-0082; `--keep`, #397/ADR-0102): an optional
/// leading `--keep N` becomes `args.kept`, any remaining text becomes
/// `args.instructions`. A parse error (bad `--keep` value) is rendered as a
/// status line instead — no engine traffic. The reducer renders the result
/// (`Compacted`) as a transcript notice once it arrives; nothing is recorded
/// here — unlike a prompt, a oneshot op has no user-authored message to echo
/// locally.
async fn send_compact(app: &mut App, holly: &Holly, text: &str) {
    let (kept, instructions) = match crate::tui::commands::parse_compact_args(text) {
        Ok(parsed) => parsed,
        Err(e) => {
            app.record_compact_error(e);
            return;
        }
    };
    let mut args = serde_json::Map::new();
    if kept > 0 {
        args.insert("kept".to_string(), serde_json::Value::from(kept));
    }
    if let Some(instructions) = instructions {
        args.insert(
            "instructions".to_string(),
            serde_json::Value::String(instructions),
        );
    }
    let _ = holly
        .send(InMsg::Oneshot {
            session: app.active_session_id().clone(),
            op: "compact".to_string(),
            args: serde_json::Value::Object(args),
        })
        .await;
}

/// Send `/set <key> <value>` as an [`InMsg::SetGeneration`] (#376): parses the
/// raw text into a partial [`entanglement_core::GenerationParams`] override
/// (same raw-text re-parse pattern as [`send_compact`], since `parse_command`
/// dropped the trailing args), records it as a pending persist so the
/// confirming `GenerationChanged` writes it to `agent-generation.yml`, then
/// sends the change. A parse error (unknown key, malformed value) is rendered
/// as a status line instead — no engine traffic, and no pending persist.
async fn send_set(app: &mut App, holly: &Holly, text: &str) {
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

/// Send `/show` as a no-override [`InMsg::SetGeneration`] query (#376): the
/// engine's merge is a no-op for an all-`None` override but still emits
/// [`OutEvent::GenerationChanged`][entanglement_core::OutEvent::GenerationChanged]
/// with the current effective params, which `App::handle_generation_changed`
/// renders as a status line — no pending persist is recorded, so this can never
/// be mistaken for a `/set` confirmation.
async fn send_show(app: &App, holly: &Holly) {
    let _ = holly
        .send(InMsg::SetGeneration {
            session: app.active_session_id().clone(),
            overrides: entanglement_core::GenerationParams::default(),
        })
        .await;
}

/// Send an [`InMsg::Approve`] with the chosen [`ApprovalScope`] (#174) and clear
/// the prompt. Captures a `propose_plan` handoff *before* approving clears the
/// pending request — accepting a plan mints a fresh root `build` session whose
/// first message is the plan (#141, ADR-0042; zero new protocol surface, the
/// handoff is head policy). Scope is inert for `propose_plan` (the runtime
/// records grants only on the generic tool path), so all three keys route here.
async fn send_approval(app: &mut App, holly: &Holly, request_id: String, scope: ApprovalScope) {
    let pending = app.pending_tool_request().cloned();
    let handoff = pending.as_ref().and_then(|(_, tool, input)| {
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
    // Pop the answered request and surface the next parked one, if any (#273).
    app.advance_approval();
    // Leave a one-line trace of the decision (#487) — the approval tail itself
    // clears once answered, so without this the scrollback shows no evidence a
    // call was ever approved.
    if let Some((_, tool, _)) = &pending {
        record_approved(app, tool, scope);
    }
    if let Some(plan) = handoff {
        handoff_accepted_plan(app, holly, plan).await;
    }
}

/// Records an approval as a one-line transcript entry (#487) — the same
/// out-of-band-notice idiom `App::record_status` uses elsewhere (reducer.rs) —
/// so a scrollback through the transcript shows what was decided, not just a
/// tail that silently vanished. Mirrors [`record_rejected`].
fn record_approved(app: &mut App, tool: &str, scope: ApprovalScope) {
    let scope_label = match scope {
        ApprovalScope::Once => "once",
        ApprovalScope::Session => "session",
        ApprovalScope::Always => "always",
        ApprovalScope::SessionDir => "session, dir",
    };
    app.record_status("approval", format!("✓ approved {tool} ({scope_label})"));
}

/// Records a rejection (and its optional reason) as a one-line transcript
/// entry (#487). Mirrors [`record_approved`].
fn record_rejected(app: &mut App, tool: &str, reason: &Option<String>) {
    let message = match reason {
        Some(r) => format!("✗ rejected {tool} — {r}"),
        None => format!("✗ rejected {tool}"),
    };
    app.record_status("approval", message);
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
        .send(InMsg::prompt(new_session.clone(), text.clone()))
        .await;
    // Adopt the fresh id head-side and switch to it, then mirror the first user
    // message locally (the engine never echoes `InMsg::Prompt` as an `OutEvent`).
    app.adopt_session(new_session);
    app.record_user_message(text);
}

/// Runs a `!bash` passthrough command head-side and injects the output into the
/// transcript (ADR-0030). Gated on `ENTANGLEMENT_ENABLE_BASH` — the same opt-in
/// as the model-facing `bash` tool (ADR-0010), since it runs unsandboxed by
/// default. When disabled, a hint is recorded instead of running anything.
/// Honors the same `ENTANGLEMENT_SANDBOX` opt-in as the model-facing tool
/// (#399, ADR-0104) so a passthrough command gets the same confinement.
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
    let tool = crate::host::bash::BashTool::new(app.root().to_path_buf())
        .with_sandbox(crate::host::sandbox::SandboxPolicy::from_env());
    let input = serde_json::json!({ "command": command }).to_string();
    let output = match tool.run(&input).await {
        Ok(out) => out,
        Err(e) => format!("[bash error] {e:#}"),
    };
    app.record_bash_passthrough(command.to_string(), output);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::session_view::TranscriptEntry;
    use entanglement_core::{EngineConfig, OutEvent};

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
}
