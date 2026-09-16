//! Key, click and wheel routing for bare `/set`'s settings dialog. Kept out
//! of `modal_events.rs`/`event_loop.rs` (both over the file cap), which only
//! forward here.

use anyhow::Result;
use entanglement_core::Holly;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::App;
use super::hit_test::rect_contains;
use super::modals::settings_tab_at;
use super::settings_dialog::Stage;

/// Returns `Ok(true)` only for the `Ctrl+Q` quit escape hatch.
pub(super) async fn handle_settings_key(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    if key.code == KeyCode::Char('q') && key.modifiers == KeyModifiers::CONTROL {
        return Ok(true);
    }
    let Some(dialog) = app.settings_dialog_mut() else {
        return Ok(false);
    };
    match (dialog.stage(), key.code) {
        (_, KeyCode::Esc) => app.close_settings_dialog(),
        (Stage::ConfirmRepin, KeyCode::Enter | KeyCode::Char('y')) => {
            app.confirm_settings_dialog(holly).await
        }
        (Stage::ConfirmRepin, KeyCode::Char('n') | KeyCode::Backspace) => dialog.back(),
        (Stage::ConfirmRepin, _) => {}
        (Stage::Editing, KeyCode::Enter) => app.confirm_settings_dialog(holly).await,
        (Stage::Editing, KeyCode::Tab) => dialog.next_tab(),
        (Stage::Editing, KeyCode::BackTab) => dialog.prev_tab(),
        (Stage::Editing, KeyCode::Up | KeyCode::Char('k')) => dialog.move_focus(-1),
        (Stage::Editing, KeyCode::Down | KeyCode::Char('j')) => dialog.move_focus(1),
        (Stage::Editing, KeyCode::Left | KeyCode::Char('h')) => dialog.activate(false),
        (Stage::Editing, KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ')) => {
            dialog.activate(true)
        }
        (Stage::Editing, KeyCode::Char('a')) => dialog.toggle_allow(),
        (Stage::Editing, KeyCode::PageUp) => dialog.page(false),
        (Stage::Editing, KeyCode::PageDown) => dialog.page(true),
        _ => {}
    }
    app.mark_dirty();
    Ok(false)
}

/// A left click: a tab title switches tab, a body row takes focus. Clicking
/// elsewhere does nothing — unlike the list pickers, discarding a half-edited
/// dialog on a stray click would lose work.
pub(super) fn click_settings(app: &mut App, column: u16, row: u16) {
    let (tabs, body) = app.settings_rects();
    let Some(dialog) = app.settings_dialog_mut() else {
        return;
    };
    if dialog.stage() != Stage::Editing {
        return;
    }
    if rect_contains(tabs, column, row) {
        if let Some(tab) = settings_tab_at(tabs.x, column) {
            dialog.set_tab(tab);
        }
    } else if rect_contains(body, column, row) {
        let offset = dialog.list_state().offset();
        dialog.focus_row(offset + usize::from(row - body.y));
    }
    app.mark_dirty();
}

/// The wheel moves the focus, like `j`/`k`.
pub(super) fn wheel_settings(app: &mut App, forward: bool) {
    if let Some(dialog) = app.settings_dialog_mut() {
        dialog.move_focus(if forward { 1 } else { -1 });
        app.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::settings_dialog::{RowId, Tab};
    use entanglement_core::{EngineConfig, SessionId};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn tab_and_back_tab_switch_tabs_and_esc_cancels() {
        let holly = Holly::spawn(EngineConfig::default());
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_settings_dialog();
        handle_settings_key(&mut app, &holly, key(KeyCode::Tab))
            .await
            .unwrap();
        assert_eq!(app.settings_dialog().unwrap().tab(), Tab::Generation);
        handle_settings_key(&mut app, &holly, key(KeyCode::BackTab))
            .await
            .unwrap();
        handle_settings_key(&mut app, &holly, key(KeyCode::BackTab))
            .await
            .unwrap();
        assert_eq!(app.settings_dialog().unwrap().tab(), Tab::Aux);
        handle_settings_key(&mut app, &holly, key(KeyCode::Esc))
            .await
            .unwrap();
        assert!(!app.showing_settings_dialog());
    }

    #[test]
    fn clicking_a_tab_title_and_a_row() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_settings_dialog();
        let tabs = ratatui::layout::Rect::new(10, 3, 50, 1);
        let body = ratatui::layout::Rect::new(10, 5, 50, 10);
        app.set_settings_rects(tabs, body);
        // " Session │ Generation │" — column 10 + 12 lands inside "Generation".
        click_settings(&mut app, 22, 3);
        assert_eq!(app.settings_dialog().unwrap().tab(), Tab::Generation);
        click_settings(&mut app, 12, 5); // row 0 = the persist checkbox
        let d = app.settings_dialog().unwrap();
        assert_eq!(d.rows()[d.focused_row()].id, RowId::Persist);
    }
}
