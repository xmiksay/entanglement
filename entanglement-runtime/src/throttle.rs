//! Runtime-side throttle-transition responder (#517, ADR-0141): polls the
//! shared `HttpClient`'s per-endpoint resilience pool
//! (ADR-0050/[ADR-0111](../../docs/adr/0111-adaptive-endpoint-pacing-and-429-retry-until-clear.md))
//! and emits [`OutEvent::Throttle`] only on a transition, so a remote (stdio/
//! WS) head sees the same stall the TUI already renders directly via
//! `HttpClient::throttle_status()` (`tui/input_panel.rs`).
//!
//! The pool's throttle state is engine-global, not per-session (many sessions
//! can share one endpoint, and one throttled endpoint never blocks another),
//! so this owns no per-session bookkeeping — it just diffs each endpoint's
//! classification against what it last saw and emits on change, mirroring
//! `mcp::spawn_mcp_responder`/`bash_live::spawn_lazy_builtin_responder`'s
//! "runtime service holding the one thing core doesn't" shape, except driven
//! by a poll
//! rather than the inbound `InMsg` fan-out (nothing arrives on the wire to
//! react to — the provider pool changes from LLM traffic core never sees).

use std::collections::HashMap;
use std::time::Duration;

use entanglement_core::Holly;
use entanglement_provider::{HttpClient, ThrottleStatus};

/// How often the poller re-reads the pool's live state. Only a *transition*
/// gets emitted (never every tick), so this just bounds detection latency —
/// short enough that a 429 cool-down or pacing slowdown surfaces promptly.
const THROTTLE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// One endpoint's throttle posture, coarsened to the granularity a head
/// actually renders differently (mirrors the TUI's `throttle_label` ordering:
/// an active cool-down wins over pacing, which wins over a bare saturated cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThrottleClass {
    AtRest,
    Busy,
    Pacing,
    Backoff,
}

fn classify(status: &ThrottleStatus) -> ThrottleClass {
    if status.backoff_remaining.is_some() {
        ThrottleClass::Backoff
    } else if status.penalized {
        ThrottleClass::Pacing
    } else if status.in_flight >= status.cap
        || status
            .shared_leases
            .is_some_and(|leases| leases >= status.cap)
    {
        // A sibling process (or a not-yet-reconciled lease of this process's
        // own) can saturate the shared cap even while this process's own
        // semaphore reads under it (#552) — that must still classify as busy.
        ThrottleClass::Busy
    } else {
        ThrottleClass::AtRest
    }
}

/// Diff one poll's snapshot against `last` and emit `OutEvent::Throttle` for
/// every endpoint whose classification changed since the previous poll,
/// updating `last` in place. Split out from [`spawn_throttle_responder`] so
/// the transition logic is unit-testable without a live poll loop.
fn diff_and_emit(
    holly: &Holly,
    last: &mut HashMap<String, ThrottleClass>,
    statuses: Vec<ThrottleStatus>,
) {
    for status in statuses {
        let class = classify(&status);
        // A never-seen-before endpoint is implicitly `AtRest` — so a brand new
        // endpoint that's already at rest on its first poll emits nothing,
        // same as any other steady state.
        let prev = last
            .get(&status.endpoint)
            .copied()
            .unwrap_or(ThrottleClass::AtRest);
        if prev == class {
            continue;
        }
        last.insert(status.endpoint.clone(), class);

        let retry_in_ms = status.backoff_remaining.map(|d| d.as_millis() as u64);
        // Only surfaced while actually pacing — a stale `next_request_in` from
        // a since-cleared penalty would be misleading alongside `throttled: false`.
        let pacing_in_ms = (class == ThrottleClass::Pacing)
            .then_some(status.next_request_in)
            .flatten()
            .map(|d| d.as_millis() as u64);

        holly.emit_throttle(
            status.endpoint,
            class != ThrottleClass::AtRest,
            status.in_flight,
            status.cap,
            retry_in_ms,
            pacing_in_ms,
            status.waiters,
            status.shared_leases,
        );
    }
}

/// Spawns the polling task. Aborted at shutdown alongside the other runtime
/// responders (`main.rs`) — it holds only a `Holly` clone and the shared
/// `HttpClient`, neither of which needs a graceful drain, so an abort is
/// sufficient (unlike the persistence subscriber, which must flush).
pub fn spawn_throttle_responder(
    holly: &Holly,
    http_client: HttpClient,
) -> tokio::task::JoinHandle<()> {
    let holly = holly.clone();
    tokio::spawn(async move {
        let mut last = HashMap::new();
        loop {
            diff_and_emit(&holly, &mut last, http_client.throttle_statuses());
            tokio::time::sleep(THROTTLE_POLL_INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration as StdDuration;

    use entanglement_core::{EngineConfig, OutEvent};
    use tokio::sync::broadcast::error::TryRecvError;

    use super::*;

    fn empty_engine() -> Holly {
        Holly::spawn(EngineConfig::default())
    }

    fn status(endpoint: &str) -> ThrottleStatus {
        ThrottleStatus {
            endpoint: endpoint.to_string(),
            in_flight: 0,
            cap: 3,
            backoff_remaining: None,
            penalized: false,
            model: None,
            next_request_in: None,
            waiters: 0,
            shared_leases: None,
        }
    }

    /// Drains every currently-buffered `OutEvent::Throttle` off `sub`, in order.
    fn drain_throttle_events(
        sub: &mut tokio::sync::broadcast::Receiver<OutEvent>,
    ) -> Vec<OutEvent> {
        let mut out = Vec::new();
        loop {
            match sub.try_recv() {
                Ok(ev @ OutEvent::Throttle { .. }) => out.push(ev),
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
        out
    }

    #[tokio::test]
    async fn at_rest_endpoint_emits_nothing_on_first_poll() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let mut last = HashMap::new();
        diff_and_emit(&holly, &mut last, vec![status("https://api.calm/v1")]);
        assert!(drain_throttle_events(&mut sub).is_empty());
    }

    #[tokio::test]
    async fn entering_a_429_cool_down_emits_a_backoff_transition_with_a_countdown() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let mut last = HashMap::new();
        let mut busy = status("https://api.busy/v1");
        busy.backoff_remaining = Some(StdDuration::from_secs(8));
        diff_and_emit(&holly, &mut last, vec![busy]);

        let events = drain_throttle_events(&mut sub);
        assert_eq!(events.len(), 1);
        match &events[0] {
            OutEvent::Throttle {
                endpoint,
                throttled,
                retry_in_ms,
                pacing_in_ms,
                ..
            } => {
                assert_eq!(endpoint, "https://api.busy/v1");
                assert!(*throttled);
                assert_eq!(*retry_in_ms, Some(8000));
                assert_eq!(*pacing_in_ms, None);
            }
            other => panic!("expected Throttle, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn penalized_pacing_emits_a_pacing_transition_with_a_countdown() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let mut last = HashMap::new();
        let mut paced = status("https://api.paced/v1");
        paced.penalized = true;
        paced.next_request_in = Some(StdDuration::from_millis(1200));
        diff_and_emit(&holly, &mut last, vec![paced]);

        let events = drain_throttle_events(&mut sub);
        assert_eq!(events.len(), 1);
        match &events[0] {
            OutEvent::Throttle {
                throttled,
                retry_in_ms,
                pacing_in_ms,
                ..
            } => {
                assert!(*throttled);
                assert_eq!(*retry_in_ms, None);
                assert_eq!(*pacing_in_ms, Some(1200));
            }
            other => panic!("expected Throttle, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn steady_state_reemits_nothing_only_transitions_do() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let mut last = HashMap::new();
        let mut busy = status("https://api.busy/v1");
        busy.backoff_remaining = Some(StdDuration::from_secs(8));
        diff_and_emit(&holly, &mut last, vec![busy.clone()]);
        drain_throttle_events(&mut sub); // consume the enter transition

        // Same class next poll (still backing off, just a shorter countdown) —
        // no re-emission; a held stall doesn't spam the wire every tick.
        busy.backoff_remaining = Some(StdDuration::from_secs(3));
        diff_and_emit(&holly, &mut last, vec![busy]);
        assert!(drain_throttle_events(&mut sub).is_empty());
    }

    #[tokio::test]
    async fn clearing_a_cool_down_emits_an_exit_transition() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let mut last = HashMap::new();
        let mut busy = status("https://api.busy/v1");
        busy.backoff_remaining = Some(StdDuration::from_secs(8));
        diff_and_emit(&holly, &mut last, vec![busy]);
        drain_throttle_events(&mut sub); // consume the enter transition

        diff_and_emit(&holly, &mut last, vec![status("https://api.busy/v1")]);
        let events = drain_throttle_events(&mut sub);
        assert_eq!(events.len(), 1);
        match &events[0] {
            OutEvent::Throttle {
                throttled,
                retry_in_ms,
                pacing_in_ms,
                ..
            } => {
                assert!(!*throttled);
                assert_eq!(*retry_in_ms, None);
                assert_eq!(*pacing_in_ms, None);
            }
            other => panic!("expected Throttle, got {other:?}"),
        }
    }

    /// Two endpoints — each standing in for a different session's provider,
    /// since the pool is keyed per-endpoint (ADR-0050) — one stalled, one not:
    /// only the stalled one's transition is emitted.
    #[tokio::test]
    async fn one_endpoint_stalled_another_at_rest_only_the_stalled_one_emits() {
        let holly = empty_engine();
        let mut sub = holly.subscribe();
        let mut last = HashMap::new();
        let mut stalled = status("https://api.session-a/v1");
        stalled.backoff_remaining = Some(StdDuration::from_secs(5));
        let calm = status("https://api.session-b/v1");
        diff_and_emit(&holly, &mut last, vec![stalled, calm]);

        let events = drain_throttle_events(&mut sub);
        assert_eq!(events.len(), 1);
        match &events[0] {
            OutEvent::Throttle {
                endpoint,
                throttled,
                ..
            } => {
                assert_eq!(endpoint, "https://api.session-a/v1");
                assert!(*throttled);
            }
            other => panic!("expected Throttle, got {other:?}"),
        }
    }
}
