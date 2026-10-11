//! The session working directory (#764, ADR-0224): applying a validated
//! `SetWorkdir`, and telling the model about it.
//!
//! The model learns its workdir from the per-round **trailing notice**, the
//! same out-of-band slot the mode notice rides (`mode.rs` explains why): the
//! system prompt sits in the provider's cached prefix, so a mid-session `/cd`
//! must not rewrite it, and a note pushed into `Context` would land on the
//! other side of a session's first prompt on replay than it did live. A
//! notice rebuilt from [`Session::workdir`] every round has neither problem —
//! replay only has to restore the field.

use tokio::sync::broadcast;

use super::Session;
use crate::protocol::{OutEvent, SessionId};

/// Apply an already-validated working directory and announce it.
pub(super) fn set_workdir(
    s: &mut Session,
    session: &SessionId,
    events: &broadcast::Sender<OutEvent>,
    dir: String,
) {
    s.workdir = Some(dir);
    announce(s, session, events);
}

/// Emit `WorkdirChanged` for the session's current workdir, if it has one —
/// at session start too, so a head (and the persisted log) learns it.
pub(super) fn announce(s: &Session, session: &SessionId, events: &broadcast::Sender<OutEvent>) {
    if let Some(dir) = &s.workdir {
        let _ = events.send(OutEvent::WorkdirChanged {
            session: session.clone(),
            dir: dir.clone(),
        });
    }
}

/// The trailing notice for one round: the mode notice, plus the workdir when
/// the session has one. A session that never set one gets exactly the mode
/// notice, so nothing changes for it.
pub(super) fn with_workdir(mode_notice: String, workdir: Option<&str>) -> String {
    match workdir {
        Some(dir) => format!(
            "{mode_notice} [workdir: {dir} — relative paths and bash/call commands resolve here]"
        ),
        None => mode_notice,
    }
}

#[cfg(test)]
mod tests {
    use super::with_workdir;

    #[test]
    fn no_workdir_leaves_the_mode_notice_untouched() {
        assert_eq!(with_workdir("[mode: build]".into(), None), "[mode: build]");
    }

    #[test]
    fn a_workdir_is_named_after_the_mode() {
        let notice = with_workdir("[mode: build]".into(), Some("/p/sub"));
        assert!(
            notice.starts_with("[mode: build] [workdir: /p/sub"),
            "{notice}"
        );
    }
}
