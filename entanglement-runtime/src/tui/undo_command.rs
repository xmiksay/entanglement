//! `/undo` and `/rewind` (#731, ADR-0212): roll the active session's tracked
//! file edits back through `InMsg::RevertFiles`. The engine applies the
//! request once the session is idle and answers with `FilesReverted`, which
//! the session view renders as a notice (`session_view::reducer`). Bare
//! `/rewind` lists the turns that still have edits to revert — the picker —
//! read straight from the backup store, no engine traffic.

use std::collections::HashMap;

use entanglement_core::{Holly, InMsg, SessionId};

use crate::file_backup::FileBackups;
use crate::tui::app::App;

/// `/undo`: revert the latest turn that still has tracked edits.
pub(crate) async fn send_undo(app: &App, holly: &Holly) {
    send_revert(app, holly, None).await;
}

/// `/rewind <turn>` reverts every tracked edit made after `<turn>`; bare
/// `/rewind` lists the revertable turns instead.
pub(crate) async fn send_rewind(app: &mut App, holly: &Holly, text: &str) {
    match parse_rewind_args(text) {
        Ok(Some(turn)) => send_revert(app, holly, Some(turn)).await,
        Ok(None) => list_turns(app),
        Err(e) => app.record_status("rewind", e),
    }
}

async fn send_revert(app: &App, holly: &Holly, to_turn: Option<u32>) {
    let _ = holly
        .send(InMsg::RevertFiles {
            session: app.active_session_id().clone(),
            to_turn,
        })
        .await;
}

/// Render the active session's revertable turns as a transcript notice.
pub(crate) fn list_turns(app: &mut App) {
    let session = app.active_session_id().clone();
    let backups = FileBackups::new(app.session_store().clone());
    backups.note_root(session.clone(), root_of(app, &session));
    let text = match backups.turns(&session) {
        Ok(turns) => turns_listing(&turns),
        Err(e) => format!("can't list revertable turns: {e:#}"),
    };
    app.record_notice("rewind", text);
}

fn turns_listing(turns: &[crate::file_backup::TurnEdits]) -> String {
    if turns.is_empty() {
        return "no tracked file edits to rewind".to_string();
    }
    let mut out = String::from(
        "turns with revertable file edits — `/rewind <N>` reverts everything after turn N \
         (bash/call/rhai exec changes are not tracked):",
    );
    for t in turns {
        let files = if t.files == 1 { "file" } else { "files" };
        out.push_str(&format!(
            "\n  {:>3}  {} {files}  {}",
            t.turn, t.files, t.prompt
        ));
    }
    out
}

/// The root of `session`'s spawn tree, from the views' parent links — a
/// child's backups live under its root's directory.
fn root_of(app: &App, session: &SessionId) -> SessionId {
    let parents: HashMap<&SessionId, &SessionId> = app
        .sessions()
        .into_iter()
        .filter_map(|(id, view)| view.parent().map(|p| (id, p)))
        .collect();
    let mut root = session;
    // Bounded by the number of sessions, so a malformed (cyclic) link set
    // can't spin forever.
    for _ in 0..=parents.len() {
        match parents.get(root) {
            Some(parent) => root = parent,
            None => break,
        }
    }
    root.clone()
}

/// `/rewind [N]` → `Ok(None)` (list), `Ok(Some(N))`, or a usage error.
fn parse_rewind_args(text: &str) -> Result<Option<u32>, String> {
    let rest = text.trim().strip_prefix("/rewind").unwrap_or("").trim();
    if rest.is_empty() {
        return Ok(None);
    }
    rest.parse::<u32>()
        .map(Some)
        .map_err(|_| format!("usage: /rewind [<turn>] — `{rest}` is not a turn number"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_backup::TurnEdits;

    #[test]
    fn rewind_args_parse_a_turn_or_nothing() {
        assert_eq!(parse_rewind_args("/rewind"), Ok(None));
        assert_eq!(parse_rewind_args("/rewind 3"), Ok(Some(3)));
        assert!(parse_rewind_args("/rewind x").is_err());
    }

    #[test]
    fn listing_shows_turn_file_counts_and_prompts() {
        let turns = [
            TurnEdits {
                turn: 1,
                files: 1,
                prompt: "add a parser".into(),
            },
            TurnEdits {
                turn: 3,
                files: 2,
                prompt: "fix it".into(),
            },
        ];
        let text = turns_listing(&turns);
        assert!(text.contains("  1  1 file  add a parser"), "{text}");
        assert!(text.contains("  3  2 files  fix it"), "{text}");
        assert_eq!(turns_listing(&[]), "no tracked file edits to rewind");
    }

    #[tokio::test]
    async fn undo_and_rewind_send_revert_files() {
        let holly = Holly::spawn(entanglement_core::EngineConfig::default());
        let mut inbound = holly.subscribe_inbound();
        let mut app = App::new_for_test(SessionId::new("s1"));
        send_undo(&app, &holly).await;
        send_rewind(&mut app, &holly, "/rewind 2").await;
        let first = inbound.recv().await.unwrap();
        let second = inbound.recv().await.unwrap();
        assert!(matches!(first, InMsg::RevertFiles { to_turn: None, .. }));
        assert!(matches!(
            second,
            InMsg::RevertFiles {
                to_turn: Some(2),
                ..
            }
        ));
    }
}
