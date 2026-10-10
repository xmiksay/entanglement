//! Left-click inside an open modal: hit-test the row and fire its `Enter`
//! action, or close the modal on a click outside it.

use entanglement_core::{Holly, InMsg};

use crate::tui::app::App;
use crate::tui::hit_test::{grouped_row_index, list_row_index, rect_contains};

/// Dispatch a left-click inside an open modal: map `(column, row)` to the
/// modal's list row, select it, and fire the row's `Enter` action. A click
/// landing outside the open modal's rect closes it (click-outside-to-close).
///
/// The modals are mutually exclusive in routing (the highest-priority open one
/// wins, mirroring `handle_event`'s key dispatch order), so the first modal
/// whose rect contains the click claims it. Modals without a clickable list
/// (Help, Inspect, which-key) are intentionally absent — they have no row
/// action to fire, and a click inside them is a no-op rather than a close.
pub(super) async fn click_modal(app: &mut App, holly: &Holly, column: u16, row: u16) {
    if app.showing_settings_dialog() {
        return crate::tui::settings_events::click_settings(app, column, row);
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
        // Read-only (ADR-0207 §9): a click just moves the highlight or, off
        // the list, closes the picker — there is nothing left to send.
        let area = app.profile_picker_rect();
        let len = app.available_profiles().len();
        if let Some(idx) = list_row_index(area, row, len) {
            app.profile_picker_state().select(Some(idx));
        } else if !rect_contains(area, column, row) {
            app.close_profile_picker();
        }
        return;
    }
    if app.showing_mode_picker() {
        let area = app.mode_picker_rect();
        let len = app.available_modes().len();
        if let Some(idx) = list_row_index(area, row, len) {
            app.mode_picker_state().select(Some(idx));
            if let Some(mode) = app.select_mode_picker() {
                let _ = holly
                    .send(InMsg::SetMode {
                        session: app.active_session_id().clone(),
                        mode,
                    })
                    .await;
            }
        } else if !rect_contains(area, column, row) {
            app.close_mode_picker();
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
        match crate::session_store::load_for_resume(&cwd, &id) {
            Ok((records, note)) => {
                app.restore_session(id.clone(), &records);
                // A retired agent resumes as `general` (#720); say so.
                if let Some(note) = note {
                    app.record_status("resume", note);
                }
                let paired = crate::session_store::pair_records(&records);
                if let Err(e) = holly.resume(id.clone(), paired).await {
                    tracing::error!("Failed to resume session {}: {}", id, e);
                }
            }
            Err(e) => {
                tracing::error!("Cannot resume session {}: {:#}", id, e);
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
            app.open_settings_dialog();
        } else if cmd == crate::tui::commands::Command::Show {
            let _ = holly
                .send(InMsg::SetGeneration {
                    session: app.active_session_id().clone(),
                    overrides: entanglement_core::GenerationParams::default(),
                })
                .await;
        } else if cmd == crate::tui::commands::Command::Mcp {
            crate::tui::mcp_command::send_mcp_list(app, holly).await;
        } else if cmd == crate::tui::commands::Command::Allow {
            app.set_input_text("/allow ".to_string());
        } else if cmd == crate::tui::commands::Command::Enable {
            app.open_session_tools_dialog();
        } else if cmd == crate::tui::commands::Command::Disable {
            app.set_input_text("/disable ".to_string());
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
