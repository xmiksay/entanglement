//! Keyboard handling for the picker-style dialogs: agent profile, model,
//! mode, provider key, session tools, and the `/tools` browser.

use anyhow::Result;
use entanglement_core::{Holly, InMsg};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::App;

use super::DIALOG_PAGE_SIZE;

/// Read-only (ADR-0207 §9): navigates and closes, never sends anything — the
/// picker lists the roster and marks the session's own agent, with no way to
/// switch.
pub(crate) async fn handle_profile_picker_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            app.close_profile_picker();
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
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) async fn handle_model_picker_event(
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

/// Drive the `/mode` picker (#560 P12, ADR-0207 §12): `Enter` sends a live
/// `InMsg::SetMode` for the active session — core cascades the switch over
/// the session's whole spawn sub-tree (ADR-0207 §6).
pub(crate) async fn handle_mode_picker_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.close_mode_picker();
        }
        KeyCode::Enter => {
            if let Some(mode) = app.select_mode_picker() {
                let _ = holly
                    .send(InMsg::SetMode {
                        session: app.active_session_id().clone(),
                        mode,
                    })
                    .await;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.mode_picker_next();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.mode_picker_prev();
        }
        KeyCode::PageDown => {
            app.mode_picker_page_down(DIALOG_PAGE_SIZE);
        }
        KeyCode::PageUp => {
            app.mode_picker_page_up(DIALOG_PAGE_SIZE);
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
pub(crate) async fn handle_key_dialog_event(app: &mut App, key: KeyEvent) -> Result<bool> {
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

/// Bare `/enable`'s session-tools checklist (#539): `Space` toggles a tool's
/// availability for the active session, `a` toggles auto-allow on an enabled
/// override row, `Enter` lazily connects any newly-checked available server
/// (#555) then sends the computed overlay diff, `Esc` discards.
pub(crate) async fn handle_session_tools_dialog_event(
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

/// `/tools`' read-heavy browser (#560 P9, ADR-0199 part 3): a free-text
/// typeahead filter (like `mention`/`slash_popup`'s own typed-filter
/// popups), so plain letter keys — *including* `j`/`k` — extend the filter
/// rather than doubling as vim-style navigation (`session_tools_dialog`'s
/// own `Space`-toggle checklist has no filter box to conflict with, which is
/// why it can reserve them; this view does, so it can't). `Up`/`Down` move
/// the highlight, `Tab` cycles the category filter, `Backspace` shortens the
/// filter, `Enter` enables the highlighted row exactly like a typed
/// `/enable` (ADR-0199 part 4) and closes the view, `Esc` closes without
/// acting.
pub(crate) async fn handle_tools_view_event(
    app: &mut App,
    holly: &Holly,
    key: KeyEvent,
) -> Result<bool> {
    match key.code {
        KeyCode::Esc => app.close_tools_view(),
        KeyCode::Enter => app.enable_selected_tools_view_row(holly).await,
        KeyCode::Tab => app.tools_view_cycle_category(),
        KeyCode::Down => app.tools_view_select_next(),
        KeyCode::Up => app.tools_view_select_prev(),
        KeyCode::Backspace => app.tools_view_backspace_filter(),
        KeyCode::Char('q') if key.modifiers == KeyModifiers::CONTROL => {
            return Ok(true);
        }
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            app.tools_view_push_filter_char(c);
        }
        _ => {}
    }
    Ok(false)
}
