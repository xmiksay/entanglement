//! Running a key-mapped [`Action`] (#735): the leader chords and the direct
//! chords of `tui.keybindings` land here, so a remapped key behaves exactly
//! like the default one did.

use anyhow::Result;
use entanglement_core::{Holly, InMsg};

use super::normal_keys::submit;
use super::slash_send::send_pause_resume_toggle;
use crate::tui::app::App;
use crate::tui::keybindings::Action;

/// Run `action`. `Ok(true)` asks the event loop to quit.
pub(super) async fn run_action(app: &mut App, holly: &Holly, action: Action) -> Result<bool> {
    match action {
        Action::Submit => return submit(app, holly).await,
        Action::Newline => {
            app.input().insert_newline();
            app.update_popups();
        }
        Action::CycleMode => cycle_mode(app, holly, true).await,
        Action::CycleModeBack => cycle_mode(app, holly, false).await,
        Action::PauseToggle => send_pause_resume_toggle(app, holly).await,
        _ => return Ok(app.dispatch_action(action)),
    }
    Ok(false)
}

/// The Tab / Shift+Tab behavior: accept an open mention or slash popup, open
/// the palette on a lone `/`, else step the permission mode. Mode — not
/// agent — because ADR-0207 fixed the agent at session start, so mode is the
/// only axis left that a keystroke can meaningfully switch.
async fn cycle_mode(app: &mut App, holly: &Holly, forward: bool) {
    if app.mention_visible() {
        app.accept_mention();
    } else if app.slash_visible() {
        app.accept_slash();
    } else if app.input().lines().join("\n") == "/" {
        app.toggle_command_palette();
    } else if let Some(mode) = app.cycle_mode(forward) {
        let _ = holly
            .send(InMsg::SetMode {
                session: app.active_session_id().clone(),
                mode,
            })
            .await;
    }
}
