//! `FileChange` audit wiring (#202). The `edit`/`write` host tools know a
//! change's `path`, `kind`, and after-content, but not the session it ran
//! under — that lives in the [`crate::tool_runner`] executor, which owns the
//! `ToolExec` round-trip. This module bridges the two without threading a
//! session id through the [`crate::tools::Tool`] signature: a tool calls
//! [`record`] after a successful write, and the executor wraps the execution in
//! [`capture`] to pick the record up and stamp it with `session`/`seq` before
//! broadcasting [`OutEvent::FileChange`].
//!
//! The record carries a content **hash**, not the whole-file bytes: the audit
//! event fans out to every subscriber, so a large edit must not clone its
//! contents once per head. Only the executor's registry path is scoped, so a
//! tool run outside it — the `rhai` bindings, a unit test calling `run`
//! directly — sees no sink and [`record`] is a silent no-op.

use std::cell::RefCell;
use std::future::Future;

use entanglement_core::protocol::FileChangeKind;
use entanglement_core::{Holly, OutEvent, SessionId};
use sha2::{Digest, Sha256};

tokio::task_local! {
    /// Per-execution slot the running tool records its change into. `RefCell`
    /// is sound here: the sink is task-local and the tool writes it
    /// synchronously (never across an `.await`), so there is no concurrent
    /// borrow within the task.
    static SINK: RefCell<Option<Record>>;
}

/// A file change a host tool performed, awaiting a session/seq stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub path: String,
    pub kind: FileChangeKind,
    /// Lowercase hex SHA-256 of the file's after-content.
    pub hash: String,
}

impl Record {
    fn into_event(self, session: SessionId, seq: u64) -> OutEvent {
        OutEvent::FileChange {
            session,
            seq,
            path: self.path,
            change_kind: self.kind,
            hash: self.hash,
        }
    }
}

/// Record a change from the tool that just wrote `after` to `path`. Hashes the
/// after-content and stashes it in the active [`capture`] scope; a no-op when
/// no scope is active (rhai bindings, direct-`run` unit tests).
pub fn record(path: String, kind: FileChangeKind, after: &[u8]) {
    let hash = format!("{:x}", Sha256::digest(after));
    let _ = SINK.try_with(|slot| {
        *slot.borrow_mut() = Some(Record { path, kind, hash });
    });
}

/// Run `fut` (a tool execution) inside a fresh sink scope and return its output
/// alongside any [`Record`] the tool stashed via [`record`].
pub async fn capture<F>(fut: F) -> (F::Output, Option<Record>)
where
    F: Future,
{
    SINK.scope(RefCell::new(None), async move {
        let out = fut.await;
        let rec = SINK.with(|slot| slot.borrow_mut().take());
        (out, rec)
    })
    .await
}

/// Convenience for the executor: run the tool via [`capture`] and broadcast the
/// resulting [`OutEvent::FileChange`] if the tool recorded one, minting a **fresh**
/// per-session seq (#157) rather than reusing the parked `ToolExec` seq. Returns
/// the tool's output for the `ToolResult` reply.
pub async fn capture_and_emit<F>(holly: &Holly, session: &SessionId, fut: F) -> F::Output
where
    F: Future,
{
    let (out, rec) = capture(fut).await;
    if let Some(rec) = rec {
        holly.emit_for_session(session, |seq| rec.into_event(session.clone(), seq));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capture_picks_up_a_recorded_change() {
        let (out, rec) = capture(async {
            record("a.txt".into(), FileChangeKind::Edit, b"beta\n");
            "ok"
        })
        .await;
        assert_eq!(out, "ok");
        let rec = rec.expect("change recorded");
        assert_eq!(rec.path, "a.txt");
        assert_eq!(rec.kind, FileChangeKind::Edit);
        // SHA-256 of "beta\n".
        assert_eq!(rec.hash, format!("{:x}", Sha256::digest(b"beta\n")),);
    }

    #[tokio::test]
    async fn capture_is_none_when_nothing_recorded() {
        let (out, rec) = capture(async { 7 }).await;
        assert_eq!(out, 7);
        assert!(rec.is_none());
    }

    #[tokio::test]
    async fn record_outside_a_scope_is_a_silent_noop() {
        // No panic, no effect — the rhai/direct-run path.
        record("x".into(), FileChangeKind::Create, b"");
    }

    #[test]
    fn record_becomes_a_stamped_file_change_event() {
        let rec = Record {
            path: "p".into(),
            kind: FileChangeKind::Create,
            hash: "deadbeef".into(),
        };
        let ev = rec.into_event(SessionId::new("s"), 9);
        match ev {
            OutEvent::FileChange {
                session,
                seq,
                path,
                change_kind,
                hash,
            } => {
                assert_eq!(session, SessionId::new("s"));
                assert_eq!(seq, 9);
                assert_eq!(path, "p");
                assert_eq!(change_kind, FileChangeKind::Create);
                assert_eq!(hash, "deadbeef");
            }
            other => panic!("expected FileChange, got {other:?}"),
        }
    }
}
