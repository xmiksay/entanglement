//! Unit tests for the one limiter: window, concurrency, per-model keying and
//! the `Retry-After` cool-down.

use std::collections::VecDeque;

use futures::FutureExt;

use super::*;
use crate::client::gate::{window_wait, WINDOW};
use crate::client::{test_http_client, SharedStateLocation};

fn limits(rpm: Option<u32>, tpm: Option<u32>, concurrency: Option<usize>) -> RateLimits {
    RateLimits {
        rpm,
        tpm,
        concurrency,
    }
}

fn limiter(endpoint: RateLimits) -> Limiter {
    Limiter::new(
        endpoint,
        RateLimits::DEFAULT,
        "test",
        &SharedStateLocation::Disabled,
    )
}

fn far_deadline() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

#[test]
fn window_admits_rpm_requests_then_waits_for_the_oldest_to_expire() {
    let rpm2 = limits(Some(2), None, None);
    let t0 = Instant::now();
    let mut sent = VecDeque::new();
    assert_eq!(window_wait(&mut sent, &rpm2, t0, 0), None);
    sent.push_back((t0, 0));
    let t1 = t0 + Duration::from_secs(10);
    assert_eq!(window_wait(&mut sent, &rpm2, t1, 0), None);
    sent.push_back((t1, 0));
    // Full: the third request waits until the first leaves the window.
    let t2 = t0 + Duration::from_secs(20);
    assert_eq!(
        window_wait(&mut sent, &rpm2, t2, 0),
        Some(Duration::from_secs(40))
    );
    // Once it has left, there is room again and the expired entry is pruned.
    let t3 = t0 + WINDOW;
    assert_eq!(window_wait(&mut sent, &rpm2, t3, 0), None);
    assert_eq!(sent.len(), 1);
}

#[test]
fn window_counts_tokens_against_tpm() {
    let tpm100 = limits(None, Some(100), None);
    let t0 = Instant::now();
    let mut sent = VecDeque::from([(t0, 60)]);
    let later = t0 + Duration::from_secs(1);
    assert_eq!(window_wait(&mut sent, &tpm100, later, 40), None);
    assert_eq!(
        window_wait(&mut sent, &tpm100, later, 50),
        Some(WINDOW - Duration::from_secs(1))
    );
}

#[test]
fn a_request_larger_than_tpm_still_goes_through_an_empty_window() {
    let tpm100 = limits(None, Some(100), None);
    let mut sent = VecDeque::new();
    assert_eq!(window_wait(&mut sent, &tpm100, Instant::now(), 500), None);
}

#[test]
fn endpoint_fallbacks_fill_only_unset_axes() {
    assert_eq!(
        RateLimits::default().or(RateLimits::DEFAULT),
        RateLimits::DEFAULT
    );
    let set = limits(Some(7), Some(1000), Some(2));
    assert_eq!(set.or(RateLimits::DEFAULT), set);
    let partial = limits(None, Some(1000), None);
    assert_eq!(
        partial.or(RateLimits::DEFAULT),
        limits(Some(50), Some(1000), Some(3))
    );
}

#[test]
fn a_client_with_its_own_defaults_applies_them_to_new_endpoints() {
    let http = test_http_client().with_default_limits(limits(None, None, Some(1)));
    let ep = http.endpoint("https://mcp.example/mcp", RateLimits::default());
    assert_eq!(
        ep.current_endpoint_gate().limits(),
        limits(None, None, Some(1))
    );
}

#[tokio::test]
async fn endpoint_concurrency_bounds_in_flight() {
    let l = limiter(limits(None, None, Some(1)));
    let req = RequestLimits {
        endpoint: limits(None, None, Some(1)),
        model: RateLimits::default(),
    };
    let first = l.admit("m", req, 0, far_deadline()).await.unwrap();
    assert!(l
        .admit("m", req, 0, far_deadline())
        .now_or_never()
        .is_none());
    drop(first);
    assert!(l
        .admit("m", req, 0, far_deadline())
        .now_or_never()
        .is_some());
}

#[tokio::test]
async fn a_model_limit_is_keyed_per_model_on_the_same_endpoint() {
    let l = limiter(RateLimits::default());
    let flash = RequestLimits {
        endpoint: RateLimits::default(),
        model: limits(None, None, Some(1)),
    };
    let _held = l
        .admit("glm-4.7-flash", flash, 0, far_deadline())
        .await
        .unwrap();
    // A second flash call waits on its own model's slot…
    assert!(l
        .admit("glm-4.7-flash", flash, 0, far_deadline())
        .now_or_never()
        .is_none());
    // …while another model on the same endpoint (same limits) is admitted.
    assert!(l
        .admit("glm-5.2", flash, 0, far_deadline())
        .now_or_never()
        .is_some());
}

#[tokio::test]
async fn a_caller_blocked_on_its_model_holds_no_endpoint_slot() {
    let l = limiter(limits(None, None, Some(2)));
    let flash = RequestLimits {
        endpoint: limits(None, None, Some(2)),
        model: limits(None, None, Some(1)),
    };
    let _held = l.admit("flash", flash, 0, far_deadline()).await.unwrap();
    let mut blocked = Box::pin(l.admit("flash", flash, 0, far_deadline()));
    assert!((&mut blocked).now_or_never().is_none());
    assert_eq!(l.current_endpoint_gate().in_flight(), 1);
}

#[test]
fn a_model_without_limits_gets_no_gate() {
    let l = limiter(RateLimits::default());
    assert!(l.model_gate("glm-4.7", RateLimits::default()).is_none());
}

#[test]
fn changed_limits_update_the_one_gate_in_place() {
    let l = limiter(RateLimits::default());
    let first = l.model_gate("flash", limits(None, None, Some(5))).unwrap();
    let corrected = l.model_gate("flash", limits(None, None, Some(1))).unwrap();
    assert!(Arc::ptr_eq(&first, &corrected));
    assert_eq!(corrected.cap(), Some(1));

    let ep = l.endpoint_gate(limits(Some(10), None, Some(4)));
    assert_eq!(ep.limits(), limits(Some(10), None, Some(4)));
    assert!(Arc::ptr_eq(&ep, &l.current_endpoint_gate()));
}

#[tokio::test]
async fn alternating_callers_share_one_window() {
    let l = limiter(RateLimits::default());
    let a = RequestLimits {
        endpoint: limits(Some(3), None, None),
        model: RateLimits::default(),
    };
    let b = RequestLimits {
        endpoint: limits(Some(4), None, None),
        ..a
    };
    for req in [a, b, a] {
        l.admit("m", req, 0, far_deadline()).await.unwrap();
    }
    // Three requests are in the window whoever sent them: `a`'s rpm 3 is
    // full, `b`'s 4 has one slot left.
    assert!(l.admit("m", a, 0, far_deadline()).now_or_never().is_none());
    assert!(l.admit("m", b, 0, far_deadline()).now_or_never().is_some());
    assert!(l.admit("m", b, 0, far_deadline()).now_or_never().is_none());
}

#[tokio::test]
async fn shrinking_the_cap_under_load_never_over_admits() {
    let l = limiter(RateLimits::default());
    let wide = RequestLimits {
        endpoint: limits(None, None, Some(2)),
        model: RateLimits::default(),
    };
    let narrow = RequestLimits {
        endpoint: limits(None, None, Some(1)),
        ..wide
    };
    let first = l.admit("m", wide, 0, far_deadline()).await.unwrap();
    let second = l.admit("m", wide, 0, far_deadline()).await.unwrap();
    let mut queued = Box::pin(l.admit("m", narrow, 0, far_deadline()));
    assert!((&mut queued).now_or_never().is_none());
    // One finishing leaves one in flight — the narrow cap is still full.
    drop(first);
    assert!((&mut queued).now_or_never().is_none());
    assert_eq!(l.current_endpoint_gate().in_flight(), 1);
    drop(second);
    let held = (&mut queued).now_or_never().expect("admitted").unwrap();
    assert_eq!(l.current_endpoint_gate().in_flight(), 1);
    // Widening again hands out exactly the difference.
    let _again = l.admit("m", wide, 0, far_deadline()).await.unwrap();
    assert!(l
        .admit("m", wide, 0, far_deadline())
        .now_or_never()
        .is_none());
    drop(held);
    assert_eq!(l.current_endpoint_gate().in_flight(), 1);
}

#[tokio::test]
async fn pacers_are_admitted_in_arrival_order() {
    let gate = Gate::new(limits(None, Some(100), None));
    gate.pace(60).await;
    let mut large = Box::pin(gate.pace(50));
    assert!((&mut large).now_or_never().is_none());
    // A small request that would fit queues behind the blocked large one,
    // so the large one cannot be overtaken forever.
    assert!(gate.pace(10).now_or_never().is_none());
    // The status reports the tpm wait even though rpm-wise there is room.
    assert!(gate.window_full_for().is_some());
}

#[tokio::test]
async fn an_unrecorded_request_frees_its_window_slot() {
    let gate = Gate::new(limits(Some(1), None, None));
    let at = gate.pace(0).await.expect("recorded");
    assert!(gate.window_full_for().is_some());
    gate.unrecord(at, 0);
    assert!(gate.window_full_for().is_none());
}

#[test]
fn retry_after_extends_never_shrinks() {
    let l = limiter(RateLimits::default());
    l.set_retry_after(Duration::from_secs(30));
    l.set_retry_after(Duration::from_secs(1));
    let remaining = l.retry_after_remaining().expect("parked");
    assert!(remaining > Duration::from_secs(25), "got {remaining:?}");
}

#[tokio::test]
async fn wait_for_retry_after_gives_up_when_the_cool_down_exceeds_the_deadline() {
    let l = limiter(RateLimits::default());
    l.set_retry_after(Duration::from_secs(60));
    let started = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(1);
    assert!(l.wait_for_retry_after(deadline).await.is_err());
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "must not sleep"
    );
}

#[tokio::test]
async fn wait_for_retry_after_waits_out_a_short_cool_down() {
    let l = limiter(RateLimits::default());
    l.set_retry_after(Duration::from_millis(50));
    let started = Instant::now();
    assert!(l.wait_for_retry_after(far_deadline()).await.is_ok());
    assert!(started.elapsed() >= Duration::from_millis(40));
}

#[test]
fn http_client_keys_one_limiter_per_endpoint() {
    let http = test_http_client();
    let a1 = http.endpoint("https://api.a/v1", RateLimits::default());
    let a2 = http.endpoint("https://api.a/v1", RateLimits::default());
    let b = http.endpoint("https://api.b/v1", RateLimits::default());
    assert!(Arc::ptr_eq(&a1, &a2));
    assert!(!Arc::ptr_eq(&a1, &b));
}

#[test]
fn tokens_are_estimated_only_when_a_tpm_window_counts_them() {
    let body = serde_json::json!({ "text": "x".repeat(400) });
    assert_eq!(RequestLimits::default().estimate_tokens(&body), 0);
    let counted = RequestLimits {
        endpoint: RateLimits::default(),
        model: limits(None, Some(10_000), None),
    };
    let tokens = counted.estimate_tokens(&body);
    assert!((100..=110).contains(&tokens), "got {tokens}");
}
