//! `session_store::resumable_prefix` across two resumes (#723): an embedder
//! that resumes a truncated log under the same id must first delete the rows
//! from the cut onward, or the old tombstone stays the first gap and every
//! later resume silently discards the work done since.

use std::sync::{Arc, Mutex};

use entanglement_core::{EngineConfig, Holly, InMsg, OutEvent, SessionId};
use entanglement_runtime::persistence::spawn_persistence_subscriber_with_sink;
use entanglement_runtime::session_store::{pair_records, resumable_prefix, LogPayload, LogRecord};

use crate::record_sink::{turn_through_flaky_sink, wait_for_done, FlakySink};

fn done_count(records: &[LogRecord]) -> usize {
    records
        .iter()
        .filter(|r| matches!(r.payload, LogPayload::Out(OutEvent::Done { .. })))
        .count()
}

#[tokio::test]
async fn second_resume_keeps_work_done_after_a_truncated_resume() {
    let sid = SessionId::new("twice-root");
    // First run: the sink drops a record, so the log carries a tombstone.
    let first_run = turn_through_flaky_sink(&sid, &[2]).await;
    let prefix = resumable_prefix(first_run.iter().cloned().map(Ok::<_, ()>));
    let cut = prefix
        .truncation
        .expect("the dropped record is a hole")
        .index();

    // The documented step: delete the rows from the cut onward before the
    // resumed session writes again.
    let store = Arc::new(Mutex::new(first_run[..cut].to_vec()));
    let holly = Holly::spawn(EngineConfig::default());
    let _tap = spawn_persistence_subscriber_with_sink(&holly, FlakySink::new(&[], store.clone()));
    holly
        .resume(sid.clone(), pair_records(&prefix.records))
        .await
        .expect("resume");
    holly
        .send(InMsg::prompt(sid.clone(), "after resume"))
        .await
        .expect("send prompt");
    let after = wait_for_done(&store, &sid, cut).await;
    let new_rows = &after[cut..];

    // Second load: intact, and the post-resume turn survives.
    let reloaded = resumable_prefix(after.iter().cloned().map(Ok::<_, ()>));
    assert!(!reloaded.truncated(), "{:?}", reloaded.truncation);
    assert_eq!(reloaded.records.len(), after.len());
    assert_eq!(done_count(&reloaded.records), 1);

    // The trap the deletion avoids: had the old rows (and tombstone) stayed,
    // the second load would cut at the same gap and lose the new turn.
    let kept_tombstone: Vec<LogRecord> = first_run.iter().chain(new_rows).cloned().collect();
    let lossy = resumable_prefix(kept_tombstone.into_iter().map(Ok::<_, ()>));
    assert_eq!(lossy.truncation.map(|t| t.index()), Some(cut));
    assert_eq!(done_count(&lossy.records), 0);
}
