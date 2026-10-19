//! Awaiting a parked decision, optionally bounded by a mode's
//! `question_timeout` (ADR-0207 §11).

use std::time::Duration;

use tokio::sync::oneshot;

use crate::seam::Decision;

/// Await a registered waiter's decision. A dropped sender — the router resolved a
/// `Stop`, a re-register superseded this waiter, or the whole executor stopped —
/// resolves to [`Decision::Stop`], preserving the "inbox closed ⇒ unwind
/// silently" semantics the broadcast park had (ADR-0017).
pub async fn await_decision(rx: oneshot::Receiver<Decision>) -> Decision {
    rx.await.unwrap_or(Decision::Stop)
}

/// Like [`await_decision`], bounded by an optional `timeout` (ADR-0207 §11,
/// stage 5c: a mode's `question_timeout` governs both `ask_user` questions
/// and parked tool approvals). `None` waits forever, identical to
/// [`await_decision`] — this module knows nothing about modes, only
/// durations, so the `0` = infinite translation happens in the caller
/// (`crate::run_limits::timeout`). `None` in the *return* means the timeout
/// elapsed with no decision delivered; the receiver is then dropped, so a
/// decision landing after the deadline (a slow head, a stale approval)
/// resolves to nothing rather than being silently applied late.
pub async fn await_decision_timed(
    rx: oneshot::Receiver<Decision>,
    timeout: Option<Duration>,
) -> Option<Decision> {
    match timeout {
        None => Some(await_decision(rx).await),
        Some(d) => tokio::time::timeout(d, rx)
            .await
            .ok()
            .map(|res| res.unwrap_or(Decision::Stop)),
    }
}
