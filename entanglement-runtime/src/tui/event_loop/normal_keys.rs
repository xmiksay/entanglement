//! Key handling in [`ApprovalMode::Normal`]: the prompt input, popups, and
//! submitting a prompt / slash command / `!bash` line. Remappable keys
//! (`tui.keybindings`, #735) resolve to an action first ([`super::actions`]);
//! the arms below are the fixed editing keys.

use anyhow::Result;
use entanglement_core::{Holly, InMsg};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tracing::debug;

use super::actions::run_action;
use super::slash_send::{
    run_bash_passthrough, send_compact, send_help_if_requested, send_name, send_pause, send_resume,
    send_show, send_stop, send_user_command,
};
use crate::tui::app::App;

/// Handle one key in Normal mode. `Ok(true)` asks the event loop to quit.
pub(super) async fn handle_normal_key(app: &mut App, holly: &Holly, key: KeyEvent) -> Result<bool> {
    if let Some(action) = app.keymap().direct_action(&key) {
        return run_action(app, holly, action).await;
    }
    match key.code {
        // Reserved (never remappable): the unconditional escape hatch.
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
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
                let target = app.active_session_id().clone();
                crate::tui::stop_command::request_stop(app, holly, target).await;
            }
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
    }
    Ok(false)
}

/// The `submit` action (`Enter` by default): an open popup accepts its
/// selection, otherwise the input is submitted. Newlines are their own action
/// (`Shift`/`Alt+Enter`, `Ctrl+J`): Alt prefixes an ESC on virtually all
/// vt100+ terminals, Shift works on kitty-protocol terminals (D1).
pub(super) async fn submit(app: &mut App, holly: &Holly) -> Result<bool> {
    if app.mention_visible() {
        app.accept_mention();
    } else if app.slash_visible() {
        app.accept_slash();
    } else {
        let text = app.take_input_text();
        if !text.is_empty() {
            if text.starts_with('/') {
                if let Some(cmd) = crate::tui::commands::parse_command(&text) {
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
                        crate::tui::set_command::send_set(app, holly, &text).await;
                        return Ok(false);
                    }
                    if cmd == crate::tui::commands::Command::Show {
                        send_show(app, holly).await;
                        return Ok(false);
                    }
                    if cmd == crate::tui::commands::Command::Mcp {
                        crate::tui::mcp_command::send_mcp(app, holly, &text).await;
                        return Ok(false);
                    }
                    if cmd == crate::tui::commands::Command::Allow {
                        crate::tui::allow_command::send_allow(app, &text);
                        return Ok(false);
                    }
                    if cmd == crate::tui::commands::Command::Enable
                        || cmd == crate::tui::commands::Command::Disable
                    {
                        let enabling = cmd == crate::tui::commands::Command::Enable;
                        crate::tui::enable_command::send_enable(app, holly, &text, enabling).await;
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
                    if cmd == crate::tui::commands::Command::Undo {
                        crate::tui::undo_command::send_undo(app, holly).await;
                        return Ok(false);
                    }
                    if cmd == crate::tui::commands::Command::Rewind {
                        crate::tui::undo_command::send_rewind(app, holly, &text).await;
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
                if send_user_command(app, holly, &text).await {
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
    Ok(false)
}
