//! Mouse routing: wheel over an open modal or the transcript, and left-button
//! selection / click-to-act.

use entanglement_core::Holly;
use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use crate::tui::app::{App, UiEffect};
use crate::tui::hit_test::list_row_index;

use super::click::click_modal;

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
pub(in crate::tui) async fn handle_mouse(app: &mut App, holly: &Holly, ev: MouseEvent) {
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

fn any_modal_open(app: &App) -> bool {
    app.showing_settings_dialog()
        || app.showing_sessions_modal()
        || app.showing_profile_picker()
        || app.showing_model_picker()
        || app.showing_mode_picker()
        || app.showing_key_dialog()
        || app.showing_command_palette()
        || app.showing_resume_modal()
        || app.showing_help()
        || app.showing_inspect()
        || app.showing_mcp_panel()
}

/// Moves the open modal's selection forward for a wheel-down; returns whether a
/// modal consumed the event (so the chat isn't scrolled underneath it).
fn wheel_modal_next(app: &mut App) -> bool {
    if app.showing_settings_dialog() {
        crate::tui::settings_events::wheel_settings(app, true);
    } else if app.showing_sessions_modal() {
        app.sessions_modal_next();
    } else if app.showing_profile_picker() {
        app.profile_picker_next();
    } else if app.showing_model_picker() {
        app.model_picker_next();
    } else if app.showing_mode_picker() {
        app.mode_picker_next();
    } else if app.showing_key_dialog() {
        app.key_dialog_next();
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
    if app.showing_settings_dialog() {
        crate::tui::settings_events::wheel_settings(app, false);
    } else if app.showing_sessions_modal() {
        app.sessions_modal_prev();
    } else if app.showing_profile_picker() {
        app.profile_picker_prev();
    } else if app.showing_model_picker() {
        app.model_picker_prev();
    } else if app.showing_mode_picker() {
        app.mode_picker_prev();
    } else if app.showing_key_dialog() {
        app.key_dialog_prev();
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
