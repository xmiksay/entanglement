//! A session task started while an earlier task under its id is still
//! tearing down waits for that task's exit before it does anything (#892,
//! ADR-0244), and a resumed one folds the old task's teardown tail first
//! (#906, ADR-0246).

use std::sync::Arc;

use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

use super::{Session, SessionCmd, SessionStart};
use crate::protocol::{InMsg, OutEvent, SessionId, ToolOverlayEntry};
use crate::EngineConfig;

/// Most events a teardown tail keeps. A teardown finishes one command and
/// hibernates, so a real tail is a handful; past this the tail is dropped
/// whole rather than handed over cut.
const TAIL_CAP: usize = 1024;

/// Everything broadcast for an id from its `Hibernate` send until its old
/// task exited, in broadcast (so log) order.
pub(crate) type Tail = Arc<[OutEvent]>;

/// The log a resume replayed, kept for a task that must re-fold it with its
/// predecessor's tail.
pub(crate) type Records = Arc<[(Option<InMsg>, OutEvent)]>;

/// A hibernating task's last mode and tool overlay: the authoritative
/// state a successor starts from, where folding events could reconstruct it
/// wrongly (a head-appended mode, a same-value repetition the tail match
/// skips).
#[derive(Clone, Debug)]
pub(crate) struct Final {
    pub(crate) mode: String,
    pub(crate) overlay: Vec<ToolOverlayEntry>,
}

impl Final {
    pub(super) fn of(s: &Session) -> Self {
        Self {
            mode: s.mode.clone(),
            overlay: s.tool_overlay.clone(),
        }
    }
}

/// What a hibernated task's exit signal carries once it has exited: its
/// teardown tail, or `None` when it has none to hand over (it panicked, or
/// the tail lagged or overflowed), and its final state (`None` after a
/// panic, or when it never started).
#[derive(Clone, Debug)]
pub(crate) struct Exited {
    pub(crate) tail: Option<Tail>,
    pub(crate) last: Option<Final>,
}

/// How a session task ended, for the watcher that awaits a hibernated one.
pub(crate) enum Ending {
    /// It ended (inbox closed, or compacted away): no final state.
    Ended,
    /// It hibernated with this final state.
    Hibernated(Final),
    /// It never ran: hibernated or closed while it waited for its own
    /// predecessor, whose hand-over it passes on unchanged, so a task
    /// started behind it still gets that teardown tail and final state.
    NeverRan(Option<Exited>),
}

/// The exit signal a hibernated task leaves behind: `Some` once it has
/// exited. Its sender is dropped only after `Some` was sent, so a closed
/// signal means exited too (with no tail).
pub(crate) type ExitSignal = watch::Receiver<Option<Exited>>;

/// Wait for the hibernated `task` of `session` to exit, collecting what
/// `tap` (subscribed before the `Hibernate` was sent) sees for the id
/// meanwhile, plus what is still buffered once it exited, and what the task
/// returned.
pub(crate) async fn collect_tail(
    session: &SessionId,
    mut task: JoinHandle<Ending>,
    mut tap: broadcast::Receiver<OutEvent>,
) -> Exited {
    let mut tail = Vec::new();
    let mut whole = true;
    let joined = loop {
        tokio::select! {
            joined = &mut task => break joined,
            ev = tap.recv() => match ev {
                Ok(ev) => keep(&mut tail, &mut whole, session, ev),
                Err(RecvError::Lagged(_)) => whole = false,
                Err(RecvError::Closed) => break task.await,
            },
        }
    };
    loop {
        match tap.try_recv() {
            Ok(ev) => keep(&mut tail, &mut whole, session, ev),
            Err(TryRecvError::Lagged(_)) => whole = false,
            Err(_) => break,
        }
    }
    let last = match joined {
        Ok(Ending::Hibernated(last)) => Some(last),
        Ok(Ending::Ended) => None,
        // Its own tap saw at most the tail of that same predecessor, already
        // in the hand-over it passes on.
        Ok(Ending::NeverRan(prior)) => {
            return prior.unwrap_or(Exited {
                tail: None,
                last: None,
            })
        }
        Err(e) => {
            if e.is_panic() {
                tracing::error!(%session, "session task panicked while hibernating");
            }
            return Exited {
                tail: None,
                last: None,
            };
        }
    };
    if !whole {
        tracing::warn!(%session, "teardown tail lagged or overflowed; not handed over");
    }
    Exited {
        tail: whole.then(|| tail.into()),
        last,
    }
}

/// Keep `ev` when it is the id's and persisted: a live-only event (a nested
/// call row) never reaches the log or a head's records, so in the tail it
/// would only break the match against the records' last events.
fn keep(tail: &mut Vec<OutEvent>, whole: &mut bool, session: &SessionId, ev: OutEvent) {
    if *whole && ev.session() == Some(session) && !ev.is_live_only() {
        if tail.len() < TAIL_CAP {
            tail.push(ev);
        } else {
            *whole = false;
        }
    }
}

/// Wait for the previous task under this id to exit. Until then this task
/// neither registers nor emits (a resume's replay already ran in the
/// supervisor): commands buffer in its inbox in arrival order, as they do
/// for a live session busy in an await that doesn't watch its inbox.
/// `Ok(exited)`: start, after folding what the old task handed over, if
/// anything. `Err(exited)`: the inbox closed meanwhile (the id was closed or
/// hibernated before this task ever ran), so it never starts, ends without
/// emitting, and passes the hand-over on ([`Ending::NeverRan`]).
pub(super) async fn ready(
    prior: Option<ExitSignal>,
    rx: &mpsc::Receiver<SessionCmd>,
) -> Result<Option<Exited>, Option<Exited>> {
    let Some(mut prior) = prior else {
        return Ok(None);
    };
    let exited = match prior.wait_for(Option::is_some).await {
        Ok(exited) => exited.clone(),
        Err(_) => None,
    };
    match rx.is_closed() {
        true => Err(exited),
        false => Ok(exited),
    }
}

/// The part of `tail` a replay up to seq `replayed` has not folded yet:
/// everything after the last tail event whose seq the records hold. A
/// seq-less event right after that cut may be in the records too (a head
/// that read part of the tail) and is folded again; that is idempotent. A
/// run of seq-less events folded twice in a row leaves the state it left
/// once: each sets fields to values carried by the event itself (`Status`
/// and `SessionHibernated` close an already closed turn), except the
/// overlay drop a `ModeChanged` makes, and mode and overlay come from the
/// old task's [`Final`] state instead of the fold.
fn unseen(tail: &[OutEvent], replayed: u64) -> &[OutEvent] {
    let cut = tail
        .iter()
        .rposition(|ev| ev.seq().is_some_and(|seq| seq != 0 && seq <= replayed));
    cut.map_or(tail, |i| &tail[i + 1..])
}

/// Fold what the old task handed over into a resumed start (#906,
/// ADR-0246). The tail: re-replay the resume's records with its unseen
/// suffix through [`Session::replay`]. Its final state: the mode and overlay
/// it ended with, taken as they are, so the start-up switch to
/// `start.initial_mode` clears the overlay exactly when a later replay of
/// the log (the old task's events, then that `ModeChanged`) does. The
/// workdir the supervisor re-validated on top of the replay carries over;
/// the lineage needs no carrying, as the re-replay rebuilds the same parent
/// and children from the same records (the tail holds only this session's
/// own events).
pub(super) fn fold_tail(
    session: &SessionId,
    start: &mut SessionStart,
    exited: Option<Exited>,
    cfg: &EngineConfig,
) {
    let records = start.records.take();
    let (Some(exited), Some(records), Some(initial)) =
        (exited, records, start.initial_session.as_mut())
    else {
        return;
    };
    if let Some(tail) = exited.tail {
        fold(session, initial, &tail, &records, cfg);
    }
    if let Some(last) = exited.last {
        initial.mode = last.mode;
        initial.tool_overlay = last.overlay;
    }
}

fn fold(
    session: &SessionId,
    initial: &mut Session,
    tail: &[OutEvent],
    records: &Records,
    cfg: &EngineConfig,
) {
    let suffix = unseen(tail, initial.seq.current());
    if suffix.is_empty() {
        return;
    }
    let log: Vec<_> = records
        .iter()
        .cloned()
        .chain(suffix.iter().cloned().map(|ev| (None, ev)))
        .collect();
    match Session::replay(&log, cfg, session) {
        Ok(mut folded) => {
            folded.workdir = initial.workdir.take();
            *initial = folded;
        }
        Err(e) => tracing::warn!(%session, error = %e, "could not fold the teardown tail"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn done(seq: u64) -> OutEvent {
        OutEvent::Done {
            session: SessionId::new("s"),
            seq,
        }
    }

    fn mode(m: &str) -> OutEvent {
        OutEvent::ModeChanged {
            session: SessionId::new("s"),
            mode: m.into(),
        }
    }

    #[tokio::test]
    async fn waits_for_the_signal_and_refuses_a_closed_inbox() {
        let (_tx, rx) = mpsc::channel::<SessionCmd>(1);
        assert!(ready(None, &rx).await.is_ok(), "no predecessor: start");

        let (exited, signal) = watch::channel(None);
        let wait = tokio::spawn(async move { ready(Some(signal), &rx).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!wait.is_finished(), "must wait for the old task");
        let tail: Tail = vec![done(3)].into();
        let last = Final {
            mode: "plan".into(),
            overlay: Vec::new(),
        };
        let handed = Exited {
            tail: Some(tail),
            last: Some(last),
        };
        exited.send_replace(Some(handed));
        let got = wait.await.unwrap().expect("inbox still open: start");
        let got = got.expect("handed over");
        assert_eq!(
            got.tail.map(|t| t.len()),
            Some(1),
            "the tail is handed over"
        );
        assert_eq!(got.last.map(|l| l.mode).as_deref(), Some("plan"));

        let (tx, rx) = mpsc::channel::<SessionCmd>(1);
        let (exited, signal) = watch::channel(None);
        drop(tx);
        drop(exited); // a dropped signal counts as exited
        assert!(
            matches!(ready(Some(signal), &rx).await, Err(None)),
            "closed before it ran: pass on what there is (nothing)"
        );
    }

    #[test]
    fn only_the_suffix_after_the_last_replayed_seq_is_unseen() {
        let tail = [mode("plan"), done(4), mode("research"), done(6)];
        assert_eq!(unseen(&tail, 3).len(), 4, "nothing replayed yet");
        assert_eq!(unseen(&tail, 4).len(), 2, "up to seq 4 already held");
        assert!(unseen(&tail, 9).is_empty(), "all held");
    }

    #[tokio::test]
    async fn collects_the_ids_events_until_the_task_exits() {
        let (events, tap) = broadcast::channel(16);
        let sid = SessionId::new("s");
        let ev = events.clone();
        let task = tokio::spawn(async move {
            let _ = ev.send(done(1));
            let _ = ev.send(OutEvent::Done {
                session: SessionId::new("other"),
                seq: 1,
            });
            let _ = ev.send(done(2));
            Ending::Ended
        });
        let tail = collect_tail(&sid, task, tap)
            .await
            .tail
            .expect("whole tail");
        let seqs: Vec<_> = tail.iter().filter_map(OutEvent::seq).collect();
        assert_eq!(seqs, [1, 2]);
    }

    /// A seq-`0` supervisor error (the bypass seq) is held by no records'
    /// seq and must not move the cut.
    #[test]
    fn a_seq_zero_error_does_not_move_the_cut() {
        let err = OutEvent::Error {
            session: SessionId::new("s"),
            seq: 0,
            message: "refused".into(),
        };
        let tail = [done(4), mode("plan"), err, mode("research")];
        assert_eq!(unseen(&tail, 4), &tail[1..]);
    }

    #[tokio::test]
    async fn an_overflowing_tail_is_dropped_whole() {
        let (events, tap) = broadcast::channel(2 * TAIL_CAP);
        let ev = events.clone();
        let task = tokio::spawn(async move {
            for seq in 1..=(TAIL_CAP as u64 + 1) {
                let _ = ev.send(done(seq));
            }
            Ending::Ended
        });
        let exited = collect_tail(&SessionId::new("s"), task, tap).await;
        assert!(exited.tail.is_none());
    }

    /// A task hibernated before it ever ran passes its predecessor's
    /// hand-over on, not the little its own tap saw.
    #[tokio::test]
    async fn a_task_that_never_ran_passes_its_predecessors_hand_over_on() {
        let (_events, tap) = broadcast::channel(16);
        let prior = Exited {
            tail: Some(vec![done(7)].into()),
            last: Some(Final {
                mode: "plan".into(),
                overlay: vec![ToolOverlayEntry::deny("bash")],
            }),
        };
        let task = tokio::spawn(async move { Ending::NeverRan(Some(prior)) });
        let exited = collect_tail(&SessionId::new("s"), task, tap).await;
        assert_eq!(exited.tail.map(|t| t.len()), Some(1));
        let last = exited.last.expect("final state passed on");
        assert_eq!(last.overlay, [ToolOverlayEntry::deny("bash")]);
    }

    /// A lagged tap drops the whole tail, but the final state still goes.
    #[tokio::test]
    async fn a_lagged_tail_is_dropped_but_the_final_state_kept() {
        let (events, tap) = broadcast::channel(4);
        let task = tokio::spawn(async move {
            for seq in 1..=32 {
                let _ = events.send(done(seq));
            }
            Ending::Hibernated(Final {
                mode: "plan".into(),
                overlay: Vec::new(),
            })
        });
        let exited = collect_tail(&SessionId::new("s"), task, tap).await;
        assert!(exited.tail.is_none(), "lagged: no tail");
        assert_eq!(exited.last.map(|l| l.mode).as_deref(), Some("plan"));
    }

    /// A tap that lags while the task still runs (the `select!` arm, not
    /// the drain after the exit) drops the tail too.
    #[tokio::test]
    async fn a_tap_lagging_mid_teardown_drops_the_tail() {
        let (events, tap) = broadcast::channel(4);
        let task = tokio::spawn(async move {
            for seq in 1..=32 {
                let _ = events.send(done(seq));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ending::Ended
        });
        let exited = collect_tail(&SessionId::new("s"), task, tap).await;
        assert!(exited.tail.is_none(), "lagged mid-teardown: no tail");
    }

    /// The tail mirrors the persisted log, which leaves live-only events
    /// (nested-call rows) out.
    #[tokio::test]
    async fn live_only_events_stay_out_of_the_tail() {
        let (events, tap) = broadcast::channel(16);
        let ev = events.clone();
        let task = tokio::spawn(async move {
            let _ = ev.send(OutEvent::NestedCall {
                session: SessionId::new("s"),
                parent_request_id: "r".into(),
                index: 1,
                tool: "read".into(),
                args: String::new(),
                duration_ms: 1,
                is_error: false,
            });
            let _ = ev.send(done(1));
            Ending::Ended
        });
        let tail = collect_tail(&SessionId::new("s"), task, tap)
            .await
            .tail
            .expect("tail");
        assert_eq!(tail.len(), 1, "{tail:?}");
    }

    /// The old task's final mode and overlay replace what the fold rebuilt:
    /// here its tail was dropped (the deny it took during the teardown is
    /// not in it), but its final state still carries the deny.
    #[test]
    fn the_final_state_replaces_the_folded_overlay() {
        let cfg = EngineConfig::default();
        let sid = SessionId::new("s");
        let deny = || OutEvent::ToolOverlayChanged {
            session: sid.clone(),
            entries: vec![ToolOverlayEntry::deny("bash")],
        };
        let started = OutEvent::SessionStarted {
            session: sid.clone(),
            parent: None,
            predecessor: None,
            agent: "general".into(),
            model: None,
            root: true,
            ts: 0,
            user: None,
        };
        let records: Records = [started, mode("build"), deny(), mode("plan")]
            .map(|ev| (None, ev))
            .into();
        let replayed = Session::replay(&records, &cfg, &sid).expect("replay");
        assert!(replayed.tool_overlay.is_empty(), "the switch cleared it");
        let hibernated = OutEvent::SessionHibernated {
            session: sid.clone(),
            ts: 1,
        };
        let tail: Tail = vec![hibernated].into();
        let start = |last: Option<Final>| {
            let mut start = SessionStart {
                agent: replayed.agent.clone(),
                initial_session: Some(Session::replay(&records, &cfg, &sid).expect("replay")),
                parent: None,
                predecessor: None,
                user: None,
                initial_mode: "plan".into(),
                spawn_model: None,
                inherited: None,
                initial_workdir: None,
                prior: None,
                records: Some(records.clone()),
            };
            let exited = Exited {
                tail: Some(tail.clone()),
                last,
            };
            fold_tail(&sid, &mut start, Some(exited), &cfg);
            start.initial_session.expect("session").tool_overlay
        };
        assert!(start(None).is_empty(), "the fold alone has no deny");
        let last = Final {
            mode: "plan".into(),
            overlay: vec![ToolOverlayEntry::deny("bash")],
        };
        assert_eq!(start(Some(last)), [ToolOverlayEntry::deny("bash")]);
    }

    #[tokio::test]
    async fn a_panicked_task_hands_over_no_tail() {
        let (events, tap) = broadcast::channel(16);
        let ev = events.clone();
        let task = tokio::spawn(async move {
            let _ = ev.send(done(1));
            panic!("boom");
        });
        let exited = collect_tail(&SessionId::new("s"), task, tap).await;
        assert!(exited.tail.is_none() && exited.last.is_none());
    }
}
