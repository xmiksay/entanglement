use anyhow::Result;
use entanglement_core::{Holly, InMsg};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use super::app::{App, UiEffect};
use super::hit_test::{grouped_row_index, list_row_index, rect_contains};

/// Fixed page size for dialog navigation (`PageUp`/`PageDown`). Dialog
/// viewports are `centered_rect(_, ~40%)` ≈ 8 visible rows on a standard
/// terminal, so one page ≈ one viewport — the intuitive pager contract. A
/// dynamic size would require threading the rendered area into state
/// (over-coupling); the codebase already uses fixed page sizes (main view =
/// 5, inspect = 10).
pub(super) const DIALOG_PAGE_SIZE: usize = 8;

/// Routes a mouse event. The wheel prefers an open modal's selection (mirroring
/// `j`/`k`), else scrolls the chat transcript. Left-button press/drag/release
/// drives text selection over the transcript: a drag selects and copies (OSC 52)
/// on release; a bare click (no drag) toggles the reasoning block it lands on.
///
/// Issue 1 — when a modal or autocomplete popup is open, a left-click instead
/// acts on the row under the cursor (the same action the `Enter` key would fire
/// for that row): the click sets the `ListState` selection to the hit row and
/// dispatches the row's action. A click landing outside an open modal's rect
/// closes it (click-outside-to-close), matching what `Esc` does. The existing
/// transcript-selection path runs only when no modal/popup is open.
pub(super) async fn handle_mouse(app: &mut App, holly: &Holly, ev: MouseEvent) {
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
        MouseEventKind::Down(MouseButton::Left) => {
            // The slash/mention popups are Normal-mode overlays (not full
            // modals), so they're checked before the `any_modal_open` guard.
            // A click inside the popup accepts the highlighted-as-clicked row;
            // a click anywhere else dismisses it (the input keeps the click —
            // placement isn't supported, so just hide).
            if app.slash_visible() {
                if let Some(idx) =
                    list_row_index(app.slash_popup_rect(), ev.row, app.slash().matches().len())
                {
                    app.slash_mut().state().select(Some(idx));
                    app.accept_slash();
                } else {
                    app.hide_slash();
                }
                return;
            }
            if app.mention_visible() {
                if let Some(idx) = list_row_index(
                    app.mention_popup_rect(),
                    ev.row,
                    app.mention().matches().len(),
                ) {
                    app.mention_mut().state().select(Some(idx));
                    app.accept_mention();
                } else {
                    app.hide_mention();
                }
                return;
            }
            if any_modal_open(app) {
                click_modal(app, holly, ev.column, ev.row).await;
                return;
            }
            // No modal/popup open → begin a (possibly zero-width) selection;
            // mouse-up decides whether it was a drag (copy) or a bare click
            // (block toggle).
            app.start_selection(ev.column, ev.row);
        }
        MouseEventKind::Drag(MouseButton::Left) if !any_modal_open(app) => {
            app.update_selection(ev.column, ev.row);
        }
        MouseEventKind::Up(MouseButton::Left) if !any_modal_open(app) => {
            if app.selection_moved() {
                if let Some(text) = app.take_selection_text() {
                    app.request_effect(UiEffect::CopyToClipboard(text));
                }
            } else {
                // No drag → treat as a click: drop the empty selection, then
                // resolve the target by surface — a sidebar session row
                // switches to that session, the attention panel jumps to the
                // oldest waiting one, and the transcript toggles the block
                // under the cursor (the pre-selection UX).
                app.clear_selection();
                if let Some(id) = app.session_at(ev.column, ev.row) {
                    app.switch_to_session(id);
                } else if app.attention_at(ev.column, ev.row) {
                    app.jump_to_next_attention();
                } else if let Some(id) = app.block_at(ev.column, ev.row) {
                    app.toggle_block(id);
                }
            }
        }
        _ => {}
    }
}

/// Dispatch a left-click inside an open modal: map `(column, row)` to the
/// modal's list row, select it, and fire the row's `Enter` action. A click
/// landing outside the open modal's rect closes it (click-outside-to-close).
///
/// The modals are mutually exclusive in routing (the highest-priority open one
/// wins, mirroring `handle_event`'s key dispatch order), so the first modal
/// whose rect contains the click claims it. Modals without a clickable list
/// (Help, Inspect, which-key) are intentionally absent — they have no row
/// action to fire, and a click inside them is a no-op rather than a close.
async fn click_modal(app: &mut App, holly: &Holly, column: u16, row: u16) {
    // Highest priority first: the tools dialog overlays the profile picker
    // (`e` opens it over the picker without closing it, #330), so it wins.
    if app.showing_tools_dialog() {
        let area = app.tools_dialog_rect();
        if let Some(idx) = list_row_index(area, row, app.tools_dialog().tools().len()) {
            app.tools_dialog_state().select(Some(idx));
        }
        return;
    }
    if app.showing_session_tools_dialog() {
        let area = app.session_tools_dialog_rect();
        if let Some(idx) = list_row_index(area, row, app.session_tools_dialog().rows().len()) {
            app.session_tools_dialog_state().select(Some(idx));
        }
        return;
    }
    if app.showing_sessions_modal() {
        let area = app.sessions_modal_rect();
        let len = app.sessions_with_depth().len();
        if let Some(idx) = list_row_index(area, row, len) {
            app.sessions_modal_state().select(Some(idx));
            app.select_session_from_modal();
        } else if !rect_contains(area, column, row) {
            app.close_sessions_modal();
        }
        return;
    }
    if app.showing_resume_modal() {
        let area = app.resume_modal_rect();
        let len = app.available_sessions().len();
        if let Some(idx) = list_row_index(area, row, len) {
            // Set the clicked index, then run the same resume code the Enter
            // key uses (`resume_selected` reads `selected_resume_session()`
            // off the ListState).
            app.resume_state().select(Some(idx));
            resume_selected(app, holly).await;
        } else if !rect_contains(area, column, row) {
            app.close_resume_modal();
        }
        return;
    }
    if app.showing_profile_picker() {
        let area = app.profile_picker_rect();
        let len = app.available_profiles().len();
        if let Some(idx) = list_row_index(area, row, len) {
            app.profile_picker_state().select(Some(idx));
            if let Some(agent_name) = app.select_profile_picker() {
                let _ = holly
                    .send(InMsg::SetAgent {
                        session: app.active_session_id().clone(),
                        agent: agent_name,
                    })
                    .await;
            }
        } else if !rect_contains(area, column, row) {
            app.close_profile_picker();
        }
        return;
    }
    if app.showing_model_picker() {
        let area = app.model_picker_rect();
        let total: usize = app.available_models().iter().map(|(_, m)| m.len()).sum();
        if let Some(idx) = list_row_index(area, row, total) {
            app.model_picker_state().select(Some(idx));
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
        } else if !rect_contains(area, column, row) {
            app.close_model_picker();
        }
        return;
    }
    if app.showing_key_dialog() {
        // Only the `PickProvider` stage has a clickable list; `EnterKey` is a
        // text form whose click has no natural target, so ignore it.
        if matches!(
            app.key_dialog_stage(),
            crate::tui::key_dialog::KeyStage::PickProvider
        ) {
            let area = app.key_dialog_rect();
            let len = app.key_dialog().providers().len();
            if let Some(idx) = list_row_index(area, row, len) {
                app.key_dialog_state().select(Some(idx));
                app.key_dialog_confirm_provider();
            } else if !rect_contains(area, column, row) {
                app.close_key_dialog();
            }
        }
        return;
    }
    if app.showing_command_palette() {
        let area = app.command_palette_rect();
        let len = app.command_palette().filtered_commands().len();
        if let Some(idx) = list_row_index(area, row, len) {
            app.command_palette().state().select(Some(idx));
            dispatch_palette_click(app, holly).await;
        } else if !rect_contains(area, column, row) {
            app.close_command_palette();
        }
        return;
    }
    if app.showing_mcp_panel() {
        // The panel renders two lines per server (header + tools/error).
        let area = app.mcp_panel_rect();
        let len = app.mcp_servers().len();
        if let Some(idx) = grouped_row_index(area, row, len, 2) {
            // `mcp_select_next`/`prev` are the only public movers; nudge
            // forward/back to the clicked index. With no direct setter, walk
            // from the current selection — bounded by the server count.
            let cur = app.mcp_selected();
            if idx > cur {
                for _ in cur..idx {
                    app.mcp_select_next();
                }
            } else {
                for _ in idx..cur {
                    app.mcp_select_prev();
                }
            }
        } else if !rect_contains(area, column, row) {
            app.close_mcp_panel();
        }
    }
}

/// Runs the resume-modal's `Enter` action for the currently-selected past
/// session — the same code path `handle_resume_modal_event`'s `Enter` arm uses,
/// factored out so the mouse click dispatch reuses it verbatim.
async fn resume_selected(app: &mut App, holly: &Holly) {
    if let Some(meta) = app.selected_resume_session() {
        let id = meta.id.clone();
        let cwd = app.root().to_path_buf();
        match crate::session_store::read(&cwd, &id) {
            Ok(records) => {
                if let Some(dropped) = crate::session_store::integrity_gap(&records) {
                    tracing::error!(
                        "Refusing to resume session {}: log is missing {} dropped record(s)",
                        id,
                        dropped
                    );
                } else {
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

/// Runs the command palette's `Enter` action for the currently-selected
/// filtered command — the same dispatch `handle_command_palette_event`'s
/// `Enter` arm uses (each `Command` variant's bespoke engine send), factored
/// out so the mouse click dispatch reuses it verbatim. Returns early (true)
/// only when the command is a quit.
async fn dispatch_palette_click(app: &mut App, holly: &Holly) {
    if let Some(cmd) = app.command_palette().execute_selected() {
        if cmd == crate::tui::commands::Command::Compact {
            let _ = holly
                .send(InMsg::Oneshot {
                    session: app.active_session_id().clone(),
                    op: "compact".to_string(),
                    args: serde_json::Value::Object(serde_json::Map::new()),
                })
                .await;
        } else if cmd == crate::tui::commands::Command::Set {
            app.set_input_text("/set ".to_string());
        } else if cmd == crate::tui::commands::Command::Show {
            let _ = holly
                .send(InMsg::SetGeneration {
                    session: app.active_session_id().clone(),
                    overrides: entanglement_core::GenerationParams::default(),
                })
                .await;
        } else if cmd == crate::tui::commands::Command::Mcp {
            super::mcp_command::send_mcp_list(app, holly).await;
        } else if cmd == crate::tui::commands::Command::Allow {
            app.set_input_text("/allow ".to_string());
        } else if cmd == crate::tui::commands::Command::Enable {
            app.open_session_tools_dialog();
        } else if cmd == crate::tui::commands::Command::Disable {
            app.set_input_text("/disable ".to_string());
        } else if cmd == crate::tui::commands::Command::Bash {
            let _ = holly
                .send(InMsg::BashEnable {
                    grade: super::bash_command::default_grade(),
                })
                .await;
        } else if cmd == crate::tui::commands::Command::Stop {
            let _ = holly
                .send(InMsg::Stop {
                    session: app.active_session_id().clone(),
                })
                .await;
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
        } else {
            app.execute_command(cmd);
        }
    }
}

fn any_modal_open(app: &App) -> bool {
    app.showing_sessions_modal()
        || app.showing_profile_picker()
        || app.showing_model_picker()
        || app.showing_key_dialog()
        || app.showing_tools_dialog()
        || app.showing_command_palette()
        || app.showing_resume_modal()
        || app.showing_help()
        || app.showing_inspect()
        || app.showing_mcp_panel()
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
    } else if app.showing_tools_dialog() {
        app.tools_dialog_next();
    } else if app.showing_command_palette() {
        app.command_palette().select_next();
    } else if app.showing_resume_modal() {
        app.resume_next();
    } else if app.showing_inspect() {
        // Level-aware (#331): the list level moves the highlight, the detail /
        // Prompt level scrolls the document.
        if app.inspect_showing_list() {
            app.inspect_list_down(1);
        } else {
            app.inspect_scroll_down(3);
        }
    } else if app.showing_help() || app.showing_mcp_panel() {
        // Consume without acting — neither has a selection to move.
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
    } else if app.showing_tools_dialog() {
        app.tools_dialog_prev();
    } else if app.showing_command_palette() {
        app.command_palette().select_prev();
    } else if app.showing_resume_modal() {
        app.resume_prev();
    } else if app.showing_inspect() {
        if app.inspect_showing_list() {
            app.inspect_list_up(1);
        } else {
            app.inspect_scroll_up(3);
        }
    } else if app.showing_help() || app.showing_mcp_panel() {
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
        KeyCode::PageDown => {
            app.profile_picker_page_down(DIALOG_PAGE_SIZE);
        }
        KeyCode::PageUp => {
            app.profile_picker_page_up(DIALOG_PAGE_SIZE);
        }
        // `e`: edit the highlighted profile's tool allowlist (#330) — opens the
        // checklist dialog over the picker, leaving it open underneath.
        KeyCode::Char('e') => {
            app.open_tools_dialog();
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
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
        KeyCode::PageDown => {
            app.model_picker_page_down(DIALOG_PAGE_SIZE);
        }
        KeyCode::PageUp => {
            app.model_picker_page_up(DIALOG_PAGE_SIZE);
        }
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
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
            KeyCode::PageDown => app.key_dialog_page_down(DIALOG_PAGE_SIZE),
            KeyCode::PageUp => app.key_dialog_page_up(DIALOG_PAGE_SIZE),
            KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
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
            // Ctrl+Q still quits (immediate escape hatch); Ctrl+C is
            // intercepted upstream (ADR-0087) before reaching here, and other
            // control combos are ignored so they don't land in the key buffer.
            KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
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

/// Drive the `/agent` picker's `e` tools-checklist dialog (#330): `Space`
/// toggles the highlighted row, `Enter` materializes the checked set as a
/// user-layer override and closes, `Esc` discards. No engine traffic — the
/// write is head-side and takes effect on the next restart.
pub(super) async fn handle_tools_dialog_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc => app.close_tools_dialog(),
        KeyCode::Enter => {
            let _ = app.submit_tools_dialog();
        }
        KeyCode::Char(' ') => app.tools_dialog_toggle(),
        KeyCode::Down | KeyCode::Char('j') => app.tools_dialog_next(),
        KeyCode::Up | KeyCode::Char('k') => app.tools_dialog_prev(),
        KeyCode::PageDown => app.tools_dialog_page_down(DIALOG_PAGE_SIZE),
        KeyCode::PageUp => app.tools_dialog_page_up(DIALOG_PAGE_SIZE),
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

/// Bare `/enable`'s session-tools checklist (#539): `Space` toggles a tool's
/// availability for the active session, `a` toggles auto-allow on an enabled
/// override row, `Enter` lazily connects any newly-checked available server
/// (#555) then sends the computed overlay diff, `Esc` discards.
pub(super) async fn handle_session_tools_dialog_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => app.close_session_tools_dialog(),
        KeyCode::Enter => {
            let entries = app.session_tools_dialog().to_entries();
            app.close_session_tools_dialog();
            // A checked row may name an available bundled server (#542, #555) —
            // lazily connect every such row before the full-replacement write,
            // mirroring `upsert_enable`'s single-pattern connect.
            match crate::tui::enable_command::lazy_enable_entries(app, &entries).await {
                Ok(()) => {
                    let session = app.active_session_id().clone();
                    crate::tui::enable_command::send_overlay(holly, session, entries).await;
                }
                Err(message) => app.record_enable_error(message),
            }
        }
        KeyCode::Char(' ') => app.session_tools_dialog_mut().toggle_selected(),
        KeyCode::Char('a') => app.session_tools_dialog_mut().toggle_allow_selected(),
        KeyCode::Down | KeyCode::Char('j') => app.session_tools_dialog_mut().select_next(),
        KeyCode::Up | KeyCode::Char('k') => app.session_tools_dialog_mut().select_prev(),
        KeyCode::PageDown => app.session_tools_dialog_mut().page_down(DIALOG_PAGE_SIZE),
        KeyCode::PageUp => app.session_tools_dialog_mut().page_up(DIALOG_PAGE_SIZE),
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

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
        // an incompatible state (idempotent server-side).
        KeyCode::Char('s') => {
            if let Some(id) = app.modal_selected_session_id() {
                let _ = holly.send(InMsg::Stop { session: id }).await;
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
                    // The palette carries no trailing `key value` text (#376), and
                    // unlike `/compact` or `/mcp` a bare `/set` has no sensible
                    // default — it needs an argument. A usage-hint status line was
                    // a dead-end (the user could not proceed from there); instead
                    // prefill the input with `/set ` and drop back to normal
                    // editing. The user then types the key/value and presses Enter,
                    // which routes through the typed path (`event_loop::send_set`).
                    app.set_input_text("/set ".to_string());
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
                    // Same "no sensible default" reasoning as `/set` (#486): a
                    // bare `/allow` has no path to grant, so prefill the input
                    // instead of running it — the user types the path and
                    // presses Enter, which routes through the typed path
                    // (`event_loop`'s Enter handler → `allow_command::send_allow`).
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
                } else if cmd == crate::tui::commands::Command::Bash {
                    // The palette carries no trailing `on`/`off` args (#498), so
                    // a picked `/bash` always live-enables with the shared safe
                    // default — the same one `bash_command::parse_bash_on`'s
                    // bare-arg arm falls back to, kept in one place.
                    let _ = holly
                        .send(InMsg::BashEnable {
                            grade: super::bash_command::default_grade(),
                        })
                        .await;
                } else if cmd == crate::tui::commands::Command::Stop {
                    // Lifecycle commands (#6): the palette carries no `--all`
                    // flag, so a picked `/stop` acts on the active session only
                    // — the same default a bare typed `/stop` falls back to.
                    let _ = holly
                        .send(InMsg::Stop {
                            session: app.active_session_id().clone(),
                        })
                        .await;
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
        KeyCode::Enter => {
            if let Some(meta) = app.selected_resume_session() {
                let id = meta.id.clone();
                let cwd = app.root().to_path_buf();
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

/// Records `answer` as a draft for the pending call's current question and
/// steps forward — to its next question, or to the review/submit step once
/// every question in the call has a draft (#518). Nothing is sent here.
fn commit_current_answer(app: &mut App, answer: Vec<String>) {
    app.commit_question_answer(answer);
}

/// Sends the front call's drafted answers as one [`InMsg::AnswerQuestion`] and
/// promotes the next queued call, if any (#273, #488, #518) — the explicit
/// Submit action, only reachable once every question has a draft.
async fn submit_pending_question(
    app: &mut App,
    holly: &Holly,
    session: &entanglement_core::SessionId,
    request_id: &str,
) {
    if let Some(answers) = app.question_answers_for_submit() {
        let _ = holly
            .send(InMsg::answer_question(
                session.clone(),
                request_id.to_string(),
                answers,
            ))
            .await;
        app.advance_question();
    }
}

/// Drive a pending `ask_user` call (#488, supersedes parts of ADR-0027; #518
/// draft-until-submit): arrow/number selection over the labelled options
/// (checkboxes + `Space` toggle for a multi-select question), plus an
/// always-available "Other" entry that opens the shared input box for a
/// free-text answer. Answers are drafts — `Left`/`Backspace` steps back to any
/// earlier question in the batch to revise it, and once every question has a
/// draft the call parks on a review step whose own `Enter` is the one explicit
/// Submit that sends the batch as a single [`InMsg::AnswerQuestion`]. `Esc`
/// while editing a question interrupts the turn like an approval; `Esc` on
/// the review step just steps back to revise instead (sends nothing, keeps
/// the call parked).
pub(super) async fn handle_question_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    let Some(q) = app.pending_question() else {
        return Ok(false);
    };
    let request_id = q.request_id.clone();
    let session = app.active_session_id().clone();

    if q.is_reviewing() {
        match key.code {
            KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
                return Ok(true);
            }
            KeyCode::Enter => {
                submit_pending_question(app, holly, &session, &request_id).await;
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Esc => {
                app.question_retreat();
            }
            _ => {}
        }
        return Ok(false);
    }

    let entering = q.entering_free_form;
    let free_form_selected = q.free_form_selected();
    let multi_select = q.is_multi_select();
    let can_retreat = q.current > 0;
    let selected_label = q
        .current_question()
        .options
        .get(q.selected)
        .map(|o| o.label.clone());
    let picked_labels: Vec<String> = {
        let mut idxs: Vec<usize> = q.picked.iter().copied().collect();
        idxs.sort_unstable();
        let options = &q.current_question().options;
        idxs.into_iter()
            .filter_map(|i| options.get(i).map(|o| o.label.clone()))
            .collect()
    };

    if entering {
        // Shared input-edit keys (Ctrl+arrows, Home/End, doc jumps, Alt+Enter
        // newline) — Enter stays = submit below.
        if super::event_loop::apply_input_edit_key(app, &key) {
            return Ok(false);
        }
        match key.code {
            KeyCode::Esc => {
                let _ = app.take_input_text();
                app.question_cancel_free_form();
            }
            KeyCode::Enter => {
                let text = app.take_input_text();
                if !text.is_empty() {
                    commit_current_answer(app, vec![text]);
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
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        KeyCode::Up | KeyCode::Char('k') => app.question_move(-1),
        KeyCode::Down | KeyCode::Char('j') => app.question_move(1),
        KeyCode::PageUp => app.question_page(-(DIALOG_PAGE_SIZE as isize)),
        KeyCode::PageDown => app.question_page(DIALOG_PAGE_SIZE as isize),
        KeyCode::Char(' ') => app.question_toggle(),
        // Back to the previous question to revise it (#518) — a no-op on the
        // batch's first question.
        KeyCode::Left | KeyCode::Backspace if can_retreat => {
            app.question_retreat();
        }
        // Quick-pick by number: options are 1-based; the "Other" entry follows.
        // Multi-select toggles the option; single-select commits immediately.
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as u8 - b'1') as usize;
            let opt_count = app
                .pending_question()
                .map(|q| q.current_question().options.len())
                .unwrap_or(0);
            if idx < opt_count {
                if multi_select {
                    app.question_toggle_at(idx);
                } else if let Some(label) = app.pending_question().and_then(|q| {
                    q.current_question()
                        .options
                        .get(idx)
                        .map(|o| o.label.clone())
                }) {
                    commit_current_answer(app, vec![label]);
                }
            } else if idx == opt_count {
                app.question_begin_free_form();
            }
        }
        KeyCode::Enter => {
            if free_form_selected {
                app.question_begin_free_form();
            } else if multi_select {
                commit_current_answer(app, picked_labels);
            } else if let Some(label) = selected_label {
                commit_current_answer(app, vec![label]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::{
        EngineConfig, OutEvent, Question, QuestionOption, Questions, SessionId,
    };
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
