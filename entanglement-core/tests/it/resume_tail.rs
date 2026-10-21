//! A resume behind a slow teardown (#906, ADR-0246): the old task hands its
//! teardown tail to the new one, which folds it before it starts, so a late
//! `FilesReverted` reaches the resumed context as a later replay would.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, FileRevertReport, FileReverter, Holly, InMsg, Llm,
    LlmRequest, LlmResponse, LlmStream, OutEvent, SessionId, WorkdirResolver,
};
use tokio::sync::broadcast::Receiver;

use super::seq_robustness::{next_for, record_turn};

/// Each request's messages and trailing notice, rendered for searching.
pub(super) type Seen = Arc<Mutex<Vec<String>>>;

pub(super) struct RecordingLlm(pub(super) Seen);

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let seen = format!("{:?} {:?}", req.messages, req.trailing_notice);
        self.0.lock().unwrap().push(seen);
        Ok(stream_from_response(LlmResponse {
            text: "reply".into(),
            tool_calls: vec![],
            ..Default::default()
        }))
    }
}

/// Restores one file, slowly enough that a hibernate and a resume land
/// while it runs.
fn slow_restoring_reverter() -> FileReverter {
    Arc::new(|_session, to_turn| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            Ok(FileRevertReport {
                to_turn: to_turn.unwrap_or(0),
                restored: vec!["src/lib.rs".into()],
                skipped: Vec::new(),
            })
        })
    })
}

fn engine(seen: &Seen, workdir_resolver: Option<WorkdirResolver>) -> Holly {
    let seen = seen.clone();
    Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || Box::new(RecordingLlm(seen.clone())) as Box<dyn Llm>),
        file_reverter: Some(slow_restoring_reverter()),
        workdir_resolver,
        ..EngineConfig::default()
    })
}

async fn revert(holly: &Holly, sid: &SessionId) {
    let msg = InMsg::RevertFiles {
        session: sid.clone(),
        to_turn: None,
    };
    holly.send(msg).await.unwrap();
}

/// Prompt the resumed `sid` once, after its teardown's `FilesReverted`, and
/// assert the revert note reached the context of that turn.
async fn assert_turn_sees_the_revert(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    seen: &Seen,
    sid: &SessionId,
) {
    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    let mut reverted = false;
    loop {
        match next_for(sub, sid).await {
            OutEvent::FilesReverted { .. } => reverted = true,
            OutEvent::Done { .. } if reverted => break,
            _ => {}
        }
    }
    let requests = seen.lock().unwrap();
    let last = requests.last().expect("the resumed turn ran");
    let text = format!("{last:?}");
    assert!(
        text.contains("[files reverted]"),
        "the resumed context lacks the teardown's revert note: {text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resume_behind_a_teardown_folds_the_late_files_reverted() {
    let seen: Seen = Arc::default();
    let holly = engine(&seen, None);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("tail");
    // The records the resume carries end before the revert.
    let log = record_turn(&holly, &mut sub, &sid).await;
    revert(&holly, &sid).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();
    assert_turn_sees_the_revert(&holly, &mut sub, &seen, &sid).await;
}

/// The tail starts at the `Hibernate` send, not after it: with the old
/// task's inbox full, that send only completes once the revert emitted its
/// `FilesReverted` and the task took the next command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tail_starts_at_the_hibernate_send() {
    let seen: Seen = Arc::default();
    let holly = engine(&seen, None);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("full");
    let log = record_turn(&holly, &mut sub, &sid).await;
    revert(&holly, &sid).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    for i in 0..64 {
        let filler = InMsg::SetSessionMeta {
            session: sid.clone(),
            name: Some(format!("n{i}")),
            action: None,
            if_unset: false,
        };
        holly.send(filler).await.unwrap();
    }
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();
    assert_turn_sees_the_revert(&holly, &mut sub, &seen, &sid).await;
}

/// While the old task tears down, the supervisor decides the workdir from
/// its cache (the teardown applied a `SetWorkdir` the records lack), and its
/// re-validation (the directory is gone by now) wins over the tail's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_supervisors_workdir_decision_survives_the_fold() {
    // `sub` validates once (the `SetWorkdir`), and is gone by the resume's
    // re-validation.
    let checked = Arc::new(AtomicBool::new(false));
    let resolver: WorkdirResolver = Arc::new(move |current, dir, _| {
        if dir.contains("sub") && checked.swap(true, Ordering::SeqCst) {
            return Err("gone".into());
        }
        Ok(match dir.starts_with('/') {
            true => dir.to_string(),
            false => format!("{}/{dir}", current.unwrap_or("/p")),
        })
    });
    let seen: Seen = Arc::default();
    let holly = engine(&seen, Some(resolver));
    let mut sub = holly.subscribe();
    let sid = SessionId::new("wd");
    let log = record_turn(&holly, &mut sub, &sid).await;
    revert(&holly, &sid).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cd = InMsg::SetWorkdir {
        session: sid.clone(),
        dir: "sub".into(),
        approve: None,
    };
    holly.send(cd).await.unwrap();
    holly.hibernate(sid.clone()).await.unwrap();
    holly.resume(sid.clone(), log).await.unwrap();

    let mut started = false;
    let dir = loop {
        match next_for(&mut sub, &sid).await {
            OutEvent::SessionStarted { .. } => started = true,
            OutEvent::WorkdirChanged { dir, .. } if started => break dir,
            OutEvent::Status { .. } | OutEvent::ModeChanged { .. } => {}
            OutEvent::Done { .. } if started => panic!("no workdir announced"),
            _ => {}
        }
    };
    assert_eq!(dir, "/p/.", "the re-validated fallback, not the tail's dir");
}

/// The head read its log after the teardown's first `FilesReverted`, so its
/// records already hold that seq'd tail event: the fold cuts the tail after
/// it. A second slow revert keeps the old task tearing down past the read,
/// so its note comes through the tail: two notes in all, neither twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_held_seqd_tail_event_folds_once() {
    let seen: Seen = Arc::default();
    let holly = engine(&seen, None);
    let mut sub = holly.subscribe();
    let sid = SessionId::new("once");
    let mut log = record_turn(&holly, &mut sub, &sid).await;
    revert(&holly, &sid).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let rename = InMsg::SetSessionMeta {
        session: sid.clone(),
        name: Some("x".into()),
        action: None,
        if_unset: false,
    };
    holly.send(rename).await.unwrap();
    revert(&holly, &sid).await;
    holly.hibernate(sid.clone()).await.unwrap();
    loop {
        let ev = next_for(&mut sub, &sid).await;
        let stop = matches!(ev, OutEvent::FilesReverted { .. });
        log.push((None, ev));
        if stop {
            break;
        }
    }
    holly.resume(sid.clone(), log).await.unwrap();
    holly.send(InMsg::prompt(sid.clone(), "two")).await.unwrap();
    let mut started = false;
    loop {
        match next_for(&mut sub, &sid).await {
            OutEvent::SessionStarted { .. } => started = true,
            OutEvent::Done { .. } if started => break,
            _ => {}
        }
    }
    let last = seen.lock().unwrap().last().cloned().expect("the turn ran");
    assert_eq!(last.matches("[files reverted]").count(), 2, "{last}");
}
