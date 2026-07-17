//! `ReplayFrom` history responder — answers a late subscriber's history request
//! from the persisted event log (#160, ADR-0072).
//!
//! Core carries no event log (the log is the runtime's persistence seam), so the
//! [`InMsg::ReplayFrom`] query is answered here, off the inbound fan-out —
//! symmetric to how the supervisor answers [`InMsg::ListSessions`], just
//! runtime-side because that is where the records live. The responder reads the
//! session's `{session}.jsonl` file, keeps every persisted content
//! [`OutEvent`] whose `seq` exceeds the requested `after_seq`, and broadcasts a
//! single [`OutEvent::History`] snapshot with the requester's `correlation_id`
//! echoed so a multiplexed head can pair the reply.
//!
//! Scope (local single-user, [ADR-0048](../../docs/adr/0048-serve-head-local-trust-model.md)):
//! the request `session` is read as a **root** log id. A child session's history
//! lives in its root's file, so a `ReplayFrom` naming a child yields an empty
//! reply until the per-connection replay of the WS `serve` head (#153) maps it.

use std::path::PathBuf;

use entanglement_core::{Holly, InMsg, OutEvent, SessionId};
use tokio::sync::broadcast::error::RecvError;

use crate::session_store::{read, LogPayload};

/// Spawns a subscriber that answers [`InMsg::ReplayFrom`] with an
/// [`OutEvent::History`] snapshot read from the persisted log under `cwd`.
pub fn spawn_history_responder(holly: &Holly, cwd: PathBuf) -> tokio::task::JoinHandle<()> {
    let emitter = holly.clone();
    let mut inbound = holly.subscribe_inbound();

    tokio::spawn(async move {
        loop {
            match inbound.recv().await {
                Ok(InMsg::ReplayFrom {
                    session,
                    correlation_id,
                    after_seq,
                }) => {
                    let events = read_history(&cwd, &session, after_seq);
                    emitter.emit_history(correlation_id, session, events);
                }
                Ok(_) => {}
                // A dropped inbound frame under lag can only lose a query — the
                // head times out and re-asks; keep serving rather than exiting.
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("history responder lagged, skipped {n} inbound messages");
                }
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// Read `session`'s persisted content history after `after_seq`. A missing or
/// unreadable log yields an empty reply (the head still gets its
/// `correlation_id` back), logged rather than propagated.
fn read_history(cwd: &std::path::Path, session: &SessionId, after_seq: u64) -> Vec<OutEvent> {
    match read(cwd, session) {
        Ok(records) => history_after(&records, after_seq),
        Err(e) => {
            tracing::warn!(%session, "history responder: no readable log ({e}); empty reply");
            Vec::new()
        }
    }
}

/// The persisted content events with `seq() > after_seq`, in log order. Pure so
/// it is unit-testable without the filesystem. Point-in-time lifecycle/query
/// events (no `seq`) are excluded — the cursor semantics are content-only.
fn history_after(records: &[crate::session_store::LogRecord], after_seq: u64) -> Vec<OutEvent> {
    records
        .iter()
        .filter_map(|r| match &r.payload {
            LogPayload::Out(ev) => ev.seq().filter(|s| *s > after_seq).map(|_| ev.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::LogRecord;

    fn out(ev: OutEvent) -> LogRecord {
        LogRecord::new(SessionId::new("s1"), LogPayload::Out(ev))
    }

    fn text(seq: u64) -> OutEvent {
        OutEvent::TextDelta {
            session: SessionId::new("s1"),
            seq,
            text: format!("d{seq}"),
        }
    }

    #[test]
    fn history_after_keeps_only_content_past_the_cursor() {
        let records = vec![
            out(OutEvent::SessionStarted {
                session: SessionId::new("s1"),
                parent: None,
                predecessor: None,
                profile: "build".into(),
                model: None,
                root: true,
                ts: 0,
            }),
            out(text(1)),
            out(text(2)),
            out(OutEvent::Done {
                session: SessionId::new("s1"),
                seq: 3,
            }),
        ];

        // after_seq = 1 drops the SessionStarted (no seq) and the seq-1 delta,
        // keeping the seq-2 delta and the seq-3 Done.
        let got = history_after(&records, 1);
        assert_eq!(got, vec![text(2), records_done(3)]);

        // after_seq = 0 keeps every seq-bearing event (still no SessionStarted).
        let all = history_after(&records, 0);
        assert_eq!(all, vec![text(1), text(2), records_done(3)]);
    }

    fn records_done(seq: u64) -> OutEvent {
        OutEvent::Done {
            session: SessionId::new("s1"),
            seq,
        }
    }
}
