use anyhow::Result;
use entanglement_core::{ApprovalScope, Holly, InMsg};
use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use tracing::debug;

use super::app::App;
use super::attention::Attention;
use super::event::Event;
use super::keybindings::LeaderResult;
use super::modal_events::{
    handle_command_palette_event, handle_inspect_event, handle_key_dialog_event,
    handle_model_picker_event, handle_mouse, handle_profile_picker_event, handle_question_event,
    handle_resume_modal_event, handle_session_tools_dialog_event, handle_sessions_modal_event,
    handle_tools_dialog_event, DIALOG_PAGE_SIZE,
};
use super::session_view::ApprovalMode;

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
                // Checked before the profile picker: `e` opens the tools dialog
                // *over* the picker without closing it (#330), so it must win the
                // routing while both are marked open.
                if app.showing_tools_dialog() {
                    return handle_tools_dialog_event(app, key).await;
                }
                // Bare `/enable`'s session-tools checklist (#539).
                if app.showing_session_tools_dialog() {
                    return handle_session_tools_dialog_event(app, holly, key).await;
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
                                )
                                .await;
                            }
                        }
                        KeyCode::Char('d') => {
                            if let Some(name) = app.mcp_selected_server() {
                                crate::tui::enable_command::upsert_deny(
                                    app,
                                    holly,
                                    format!("mcp__{name}__*"),
                                )
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
                        // Mention popup (ADR-0030) wins first — it's the most
                        // specific (`@token` in flight). Then the slash popup
                        // (Issue 2): Tab accepts the selected `/command`.
                        KeyCode::Tab if app.mention_visible() => {
                            app.accept_mention();
                        }
                        KeyCode::Tab if app.slash_visible() => {
                            app.accept_slash();
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
                        KeyCode::BackTab if app.slash_visible() => {
                            app.accept_slash();
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
                            app.update_popups();
                        }
                        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().move_word_right();
                            app.update_popups();
                        }
                        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().move_to_doc_home();
                            app.update_popups();
                        }
                        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            app.input().move_to_doc_end();
                            app.update_popups();
                        }
                        KeyCode::Home => {
                            app.input().move_cursor_to_head();
                            app.update_popups();
                        }
                        KeyCode::End => {
                            app.input().move_cursor_to_end();
                            app.update_popups();
                        }
                        KeyCode::Esc => {
                            // Esc is "cancel the current turn", not "quit" (#6):
                            // close a mention popup, then collapse a multiline
                            // buffer, and only then (single-line, empty input)
                            // stop the active session's in-flight turn — the
                            // same `InMsg::Stop` Esc already sends in approval
                            // mode. The app no longer quits on Esc; `/exit` and
                            // the two-stage Ctrl+C remain the quit paths.
                            if app.mention_visible() {
                                app.hide_mention();
                            } else if app.slash_visible() {
                                app.hide_slash();
                            } else if app.is_input_multiline() {
                                app.set_input_multiline(false);
                            } else {
                                let _ = holly
                                    .send(InMsg::Stop {
                                        session: app.active_session_id().clone(),
                                    })
                                    .await;
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
                                app.update_popups();
                            } else if app.mention_visible() {
                                app.accept_mention();
                            } else if app.slash_visible() {
                                app.accept_slash();
                            } else {
                                let text = app.take_input_text();
                                if !text.is_empty() {
                                    if text.starts_with('/') {
                                        if let Some(cmd) =
                                            crate::tui::commands::parse_command(&text)
                                        {
                                            // Issue 2: `/cmd -h`/`--help` renders the
                                            // clap-generated help for any arg-bearing
                                            // command, and `/help <cmd>` renders it for
                                            // `<cmd>`. Both surface as a transcript status
                                            // line (never the keybindings dialog).
                                            if send_help_if_requested(app, &text, &cmd).await {
                                                return Ok(false);
                                            }
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
                                            if cmd == crate::tui::commands::Command::Bash {
                                                crate::tui::bash_command::send_bash(
                                                    app, holly, &text,
                                                )
                                                .await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::Enable
                                                || cmd == crate::tui::commands::Command::Disable
                                            {
                                                let enabling =
                                                    cmd == crate::tui::commands::Command::Enable;
                                                crate::tui::enable_command::send_enable(
                                                    app, holly, &text, enabling,
                                                )
                                                .await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::Name {
                                                send_name(app, holly, &text).await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::AuxModel {
                                                crate::tui::aux_command::send_aux_model(app, &text);
                                                return Ok(false);
                                            }
                                            // Lifecycle commands (#6): `/stop`,
                                            // `/pause`, `/continue` need `holly`
                                            // and the optional `--all` text.
                                            if cmd == crate::tui::commands::Command::Stop {
                                                send_stop(app, holly, &text).await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::Pause {
                                                send_pause(app, holly, &text).await;
                                                return Ok(false);
                                            }
                                            if cmd == crate::tui::commands::Command::Continue {
                                                send_resume(app, holly, &text).await;
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
                            app.update_popups();
                        }
                        KeyCode::Up => {
                            if app.mention_visible() {
                                app.mention_select_prev();
                            } else if app.slash_visible() {
                                app.slash_select_prev();
                            } else if app.input().cursor() == (0, 0) {
                                app.history_up();
                            } else {
                                app.input().move_cursor_up();
                            }
                        }
                        KeyCode::Down => {
                            if app.mention_visible() {
                                app.mention_select_next();
                            } else if app.slash_visible() {
                                app.slash_select_next();
                            } else if app.input().cursor() == (0, 0) {
                                app.history_down();
                            } else {
                                app.input().move_cursor_down();
                            }
                        }
                        // Ctrl+Space toggles pause/resume on the active session
                        // (#6): Esc now stops the turn, so pause/resume need
                        // their own dedicated key. Idempotent server-side.
                        KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            send_pause_resume_toggle(app, holly).await;
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
                            app.update_popups();
                        }
                        KeyCode::Char(c) => {
                            app.input().insert_char(c);
                            app.update_popups();
                        }
                        KeyCode::Backspace => {
                            app.input().delete_char();
                            app.update_popups();
                        }
                        KeyCode::Left => {
                            app.input().move_cursor_left();
                            app.update_popups();
                        }
                        KeyCode::Right => {
                            app.input().move_cursor_right();
                            app.update_popups();
                        }
                        _ => {}
                    },
                }
            }
        }
        Event::Mouse(mouse_event) => handle_mouse(app, holly, mouse_event).await,
        Event::Resize => {}
        Event::FocusGained => attention.set_focused(true),
        Event::FocusLost => attention.set_focused(false),
        Event::Paste(s) => {
            if matches!(app.approval_mode(), ApprovalMode::Normal) {
                app.input().insert_str(&s);
                app.update_popups();
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

/// Issue 2: `/cmd -h`/`--help` and `/help <cmd>` both render the clap-generated
/// help for the command. Returns `true` if the input was a help request (so the
/// caller short-circuits the normal dispatch), `false` to let dispatch proceed.
/// For `/cmd -h`, `cmd` is the parsed command; for `/help <cmd>`, `cmd` is
/// `Command::Help` and the target is parsed from the trailing text.
async fn send_help_if_requested(
    app: &mut App,
    text: &str,
    cmd: &crate::tui::commands::Command,
) -> bool {
    use crate::tui::commands::Command;
    // `/help <cmd>`: look up the named command and render its help_text.
    if *cmd == Command::Help {
        let target = text
            .trim()
            .strip_prefix("/help")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|name| {
                crate::tui::commands::all_commands()
                    .into_iter()
                    .find(|c| c.name() == name)
            });
        if let Some(target) = target {
            let help = target.help_text();
            app.record_notice("help", help);
            return true;
        }
        return false;
    }
    // `/cmd -h`/`--help`: any arg-bearing command accepts the help flag.
    if cmd.has_args() && has_help_flag(text) {
        let help = cmd.help_text();
        app.record_notice(cmd.name(), help);
        return true;
    }
    false
}

/// Whether the raw `/cmd …` text contains a `-h` or `--help` token (Issue 2).
fn has_help_flag(text: &str) -> bool {
    text.split_whitespace()
        .any(|tok| tok == "-h" || tok == "--help")
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

/// Send `/name <text>` as an [`InMsg::SetSessionMeta`] naming the active
/// session (same raw-text re-parse pattern as [`send_compact`]). The
/// confirming `SessionMetaChanged` folds into the session view, so the sidebar
/// title updating *is* the confirmation — no status line. Bare `/name`
/// renders a one-line usage toast instead; no engine traffic.
async fn send_name(app: &mut App, holly: &Holly, text: &str) {
    let Some(name) = crate::tui::commands::parse_name_args(text) else {
        app.set_toast("usage: /name <text>".to_string());
        return;
    };
    let _ = holly
        .send(InMsg::SetSessionMeta {
            session: app.active_session_id().clone(),
            name: Some(name),
            action: None,
        })
        .await;
}

/// Send `/stop [--all]` as one [`InMsg::Stop`] per live session (#6): the bare
/// form cancels the active session's in-flight turn (the same wire message the
/// repurposed Esc sends); `--all` fans out to every live session. A parse error
/// (unknown argument) is rendered as a status line instead.
async fn send_stop(app: &mut App, holly: &Holly, text: &str) {
    let all = match crate::tui::commands::parse_all_flag(text, crate::tui::commands::Command::Stop)
    {
        Ok(all) => all,
        Err(e) => {
            app.record_status("stop", e);
            return;
        }
    };
    if all {
        for (id, _) in app.sessions() {
            let _ = holly
                .send(InMsg::Stop {
                    session: id.clone(),
                })
                .await;
        }
    } else {
        let _ = holly
            .send(InMsg::Stop {
                session: app.active_session_id().clone(),
            })
            .await;
    }
}

/// Send `/pause [--all]` as one [`InMsg::PauseSession`] per live session (#6):
/// holds the session at `AgentState::Paused` without cancelling the turn or
/// evicting memory. `--all` fans out to every live session.
async fn send_pause(app: &mut App, holly: &Holly, text: &str) {
    let all = match crate::tui::commands::parse_all_flag(text, crate::tui::commands::Command::Pause)
    {
        Ok(all) => all,
        Err(e) => {
            app.record_status("pause", e);
            return;
        }
    };
    if all {
        for (id, _) in app.sessions() {
            let _ = holly
                .send(InMsg::PauseSession {
                    session: id.clone(),
                })
                .await;
        }
    } else {
        let _ = holly
            .send(InMsg::PauseSession {
                session: app.active_session_id().clone(),
            })
            .await;
    }
}

/// Send `/continue [--all]` as one [`InMsg::ResumeSession`] per live session
/// (#6): lifts a hold placed by `/pause`. Idempotent on a non-paused session.
/// `--all` fans out to every live session.
async fn send_resume(app: &mut App, holly: &Holly, text: &str) {
    let all =
        match crate::tui::commands::parse_all_flag(text, crate::tui::commands::Command::Continue) {
            Ok(all) => all,
            Err(e) => {
                app.record_status("resume", e);
                return;
            }
        };
    if all {
        for (id, _) in app.sessions() {
            let _ = holly
                .send(InMsg::ResumeSession {
                    session: id.clone(),
                })
                .await;
        }
    } else {
        let _ = holly
            .send(InMsg::ResumeSession {
                session: app.active_session_id().clone(),
            })
            .await;
    }
}

/// Ctrl+Space toggle (#6): pause the active session if it is currently running,
/// resume it if it is paused. Mirrors the idempotent wire semantics — the engine
/// treats `PauseSession` on an idle session and `ResumeSession` on a non-paused
/// one as no-ops, so toggling by observed state is safe.
async fn send_pause_resume_toggle(app: &mut App, holly: &Holly) {
    use entanglement_core::AgentState;
    let msg = if app.state() == AgentState::Paused {
        InMsg::ResumeSession {
            session: app.active_session_id().clone(),
        }
    } else {
        InMsg::PauseSession {
            session: app.active_session_id().clone(),
        }
    };
    let _ = holly.send(msg).await;
}

/// Send an [`InMsg::Approve`] with the chosen [`ApprovalScope`] (#174) and clear
/// the prompt. Scope is inert for `propose_plan` (the runtime records grants
/// only on the generic tool path); the sponsored-build handoff is now runtime
/// policy (ADR-0138), so the head just forwards the approval.
async fn send_approval(app: &mut App, holly: &Holly, request_id: String, scope: ApprovalScope) {
    let pending = app.pending_tool_request().cloned();
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
    use entanglement_core::{EngineConfig, OutEvent, SessionId};

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
        app.init_head_context(
            dir.path().to_path_buf(),
            crate::bash_live::LiveBashState::new(false),
        );
        let holly = engine();
        let mut rx = holly.subscribe_inbound();
        let mut attention = Attention::from_env();

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
