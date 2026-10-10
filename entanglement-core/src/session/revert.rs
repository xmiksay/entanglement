//! Applying an [`InMsg::RevertFiles`][crate::protocol::InMsg::RevertFiles]
//! (#731, ADR-0212). The file I/O is the runtime's
//! (`EngineConfig::file_reverter`); the session owns the two things that must
//! stay replay-exact — the persisted [`OutEvent::FilesReverted`] and the
//! user-role note it appends to `Context`, rebuilt from that event by
//! [`revert_note`] both live and on replay.

use tokio::sync::broadcast;

use super::emit::next_seq;
use super::Session;
use crate::protocol::{FileRevertReport, OutEvent, RevertSkip, SessionId};
use crate::EngineConfig;

/// Most paths [`revert_note`] names before summarizing the rest as a count —
/// the note lands in every later request, so a mass revert must not bloat it.
const NOTE_MAX_PATHS: usize = 20;

/// Run the revert through the runtime seam, announce the outcome, and — when
/// anything was restored — tell the model, so it never builds on edits that no
/// longer exist. A missing seam or a seam error is a recoverable `Error`.
pub(super) async fn revert_files(
    s: &mut Session,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    cfg: &EngineConfig,
    to_turn: Option<u32>,
) {
    let outcome = match &cfg.file_reverter {
        Some(reverter) => reverter(session.clone(), to_turn).await,
        None => Err("file revert is not available in this runtime".to_string()),
    };
    let report = match outcome {
        Ok(report) => report,
        Err(message) => {
            let _ = events.send(OutEvent::Error {
                session: session.clone(),
                seq: next_seq(&s.seq),
                message: format!("revert files: {message}"),
            });
            return;
        }
    };
    if let Some(note) = revert_note(&report.restored, &report.skipped, report.to_turn) {
        s.ctx.push_user(note);
    }
    let FileRevertReport {
        to_turn,
        restored,
        skipped,
    } = report;
    let _ = events.send(OutEvent::FilesReverted {
        session: session.clone(),
        seq: next_seq(&s.seq),
        to_turn,
        restored,
        skipped,
    });
}

/// The user-role note a revert leaves in `Context`, or `None` when nothing was
/// restored (the model's view of the files is then still accurate). Shared by
/// the live path above and `Session::replay`'s `FilesReverted` fold, so a
/// resumed session's history matches what the live model saw.
pub(crate) fn revert_note(
    restored: &[String],
    skipped: &[RevertSkip],
    to_turn: u32,
) -> Option<String> {
    if restored.is_empty() {
        return None;
    }
    let mut note = format!(
        "[files reverted] The user rolled back the file edits made after turn {to_turn}. \
         Restored to their earlier content (or removed, if created later): {}.",
        name_list(restored.iter().map(String::as_str))
    );
    if !skipped.is_empty() {
        note.push_str(&format!(
            " Not restored (still as they are now): {}.",
            name_list(skipped.iter().map(|s| s.path.as_str()))
        ));
    }
    note.push_str(
        " Changes made through bash, call or rhai commands were not reverted. \
         Re-read any of these files before relying on or editing them.",
    );
    Some(note)
}

fn name_list<'a>(paths: impl ExactSizeIterator<Item = &'a str>) -> String {
    let total = paths.len();
    let mut list = paths.take(NOTE_MAX_PATHS).collect::<Vec<_>>().join(", ");
    if total > NOTE_MAX_PATHS {
        list.push_str(&format!(" and {} more", total - NOTE_MAX_PATHS));
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_restored_leaves_no_note() {
        let skipped = [RevertSkip {
            path: "a".into(),
            reason: "modified".into(),
        }];
        assert_eq!(revert_note(&[], &skipped, 2), None);
    }

    #[test]
    fn note_names_restored_and_skipped_paths_and_the_exec_caveat() {
        let skipped = [RevertSkip {
            path: "b.rs".into(),
            reason: "modified".into(),
        }];
        let note = revert_note(&["a.rs".into()], &skipped, 3).expect("note");
        assert!(note.contains("after turn 3"), "{note}");
        assert!(note.contains("a.rs"), "{note}");
        assert!(
            note.contains("Not restored (still as they are now): b.rs"),
            "{note}"
        );
        assert!(note.contains("bash, call or rhai"), "{note}");
    }

    #[test]
    fn long_path_lists_are_capped() {
        let restored: Vec<String> = (0..25).map(|i| format!("f{i}")).collect();
        let note = revert_note(&restored, &[], 0).expect("note");
        assert!(note.contains("f19"), "{note}");
        assert!(!note.contains("f20,"), "{note}");
        assert!(note.contains("and 5 more"), "{note}");
    }
}
