//! Unit tests for the shared HTTP transport (pool identity, pacing gate,
//! retry-after window, transient-error classification).

use super::*;

#[test]
fn test_default_http_client() {
    let _client = HttpClient::default();
}

#[test]
fn test_new_http_client() {
    let _client = HttpClient::new();
}

#[test]
fn endpoints_are_isolated_and_stable_by_key() {
    let http = HttpClient::new();
    let a1 = http.endpoint("https://api.a/v1", None, None);
    let a2 = http.endpoint("https://api.a/v1", None, None);
    let b = http.endpoint("https://api.b/v1", None, None);
    // Same key → same state; different keys → isolated state.
    assert!(Arc::ptr_eq(&a1, &a2));
    assert!(!Arc::ptr_eq(&a1, &b));
}

#[test]
fn endpoint_uses_provided_rpm_budget() {
    let http = HttpClient::new();
    // A per-provider rpm sets the pacing gate's base interval (60s / rpm);
    // `None` falls back to the pool default (RetryConfig::rpm).
    let custom = http.endpoint("https://api.custom/v1", Some(6), None);
    assert_eq!(custom.limiter.base, Duration::from_millis(60_000 / 6));
    let default = http.endpoint("https://api.default/v1", None, None);
    assert_eq!(
        default.limiter.base,
        Duration::from_millis(60_000 / RPM_LIMIT as u64)
    );
}

#[test]
fn endpoint_concurrency_permits_match_config() {
    let http = HttpClient::with_config(RetryConfig {
        concurrency: 2,
        ..RetryConfig::default()
    });
    let ep = http.endpoint("https://api.x/v1", None, None);
    // The in-flight cap is seeded from config; the default is DEFAULT_CONCURRENCY.
    assert_eq!(ep.concurrency.available_permits(), 2);
    let dflt = HttpClient::new().endpoint("https://api.y/v1", None, None);
    assert_eq!(dflt.concurrency.available_permits(), DEFAULT_CONCURRENCY);
}

#[test]
fn endpoint_uses_provided_concurrency_cap_over_pool_default() {
    let http = HttpClient::with_config(RetryConfig {
        concurrency: 2,
        ..RetryConfig::default()
    });
    // A per-provider concurrency override wins over the pool-wide default.
    let custom = http.endpoint("https://api.custom/v1", None, Some(5));
    assert_eq!(custom.concurrency.available_permits(), 5);
    let default = http.endpoint("https://api.default/v1", None, None);
    assert_eq!(default.concurrency.available_permits(), 2);
}

#[tokio::test]
async fn endpoint_concurrency_bounds_in_flight() {
    // With a cap of 1, a second owned permit can't be taken until the first
    // is dropped — the property that serializes a spawn-storm.
    let ep = EndpointState::new(RPM_LIMIT, 1);
    let first = ep.concurrency.clone().acquire_owned().await.unwrap();
    assert!(ep.concurrency.clone().try_acquire_owned().is_err());
    drop(first);
    assert!(ep.concurrency.clone().try_acquire_owned().is_ok());
}

#[test]
fn rate_limiter_penalize_doubles_and_caps() {
    let rl = RateLimiter::new(60); // base = 1s
    assert_eq!(rl.state.lock().unwrap().interval, Duration::from_secs(1));
    // Each penalty doubles the spacing…
    rl.penalize();
    assert_eq!(rl.state.lock().unwrap().interval, Duration::from_secs(2));
    rl.penalize();
    assert_eq!(rl.state.lock().unwrap().interval, Duration::from_secs(4));
    // …but never past base × SLOWDOWN_CAP.
    for _ in 0..10 {
        rl.penalize();
    }
    assert_eq!(rl.state.lock().unwrap().interval, rl.max);
    assert_eq!(rl.max, rl.base * SLOWDOWN_CAP);
}

#[test]
fn rate_limiter_relax_steps_back_to_base_and_floors() {
    let rl = RateLimiter::new(60); // base = 1s
    rl.penalize();
    rl.penalize(); // interval = 4s
    rl.relax(); // -1 base → 3s
    assert_eq!(rl.state.lock().unwrap().interval, Duration::from_secs(3));
    // Relaxing past base clamps at base, never below.
    for _ in 0..10 {
        rl.relax();
    }
    assert_eq!(rl.state.lock().unwrap().interval, rl.base);
}

#[tokio::test]
async fn rate_limiter_spaces_concurrent_acquires() {
    // A burst of acquires against one gate must be paced ≥ interval apart,
    // not all released at once (the anti-burst property).
    let rl = RateLimiter::new(600); // base = 100ms
    let start = Instant::now();
    rl.acquire().await; // first slot: immediate
    rl.acquire().await; // second slot: ~100ms later
    rl.acquire().await; // third slot: ~200ms later
    assert!(
        start.elapsed() >= Duration::from_millis(180),
        "three acquires should span ~2 intervals, got {:?}",
        start.elapsed()
    );
}

#[test]
fn pool_key_partitions_by_endpoint_and_api_key() {
    let base = "https://api.z.ai/v4";
    // Same endpoint, different keys → different buckets (each key its own
    // limit). Same endpoint + same key → same bucket. Keyless is stable.
    assert_ne!(pool_key(base, Some("k1")), pool_key(base, Some("k2")));
    assert_eq!(pool_key(base, Some("k1")), pool_key(base, Some("k1")));
    assert_eq!(pool_key(base, None), base);
    // A key never appears verbatim in the pool identity (hashed, not raw).
    assert!(!pool_key(base, Some("supersecret")).contains("supersecret"));
    // Same key on different endpoints stays isolated.
    assert_ne!(
        pool_key(base, Some("k1")),
        pool_key("https://api.openai.com/v1", Some("k1"))
    );
}

#[test]
fn retry_after_window_extends_never_shrinks() {
    let state = EndpointState::new(RPM_LIMIT, DEFAULT_CONCURRENCY);
    state.set_retry_after(Duration::from_secs(10));
    let long = state.retry_after.lock().unwrap().unwrap();
    // A shorter window must not overwrite a longer one.
    state.set_retry_after(Duration::from_secs(1));
    assert_eq!(state.retry_after.lock().unwrap().unwrap(), long);
    // A longer window does extend it.
    state.set_retry_after(Duration::from_secs(60));
    assert!(state.retry_after.lock().unwrap().unwrap() > long);
}

#[test]
fn retryable_status_classification() {
    use reqwest::StatusCode;
    assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
    assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
    assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
    assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
    assert!(!is_retryable_status(StatusCode::OK));
}

#[tokio::test]
async fn request_send_failure_is_transient() {
    use std::io::Read;
    // A listener that accepts a connection then drops it without replying
    // reproduces a keep-alive connection reset mid-request. reqwest surfaces
    // this as a request-send error (`is_request()`, "error sending request for
    // url ...") — *not* `is_connect()` (the connection was established) and its
    // display carries no "incomplete", so the pre-fix classifier let it bubble
    // up as `openai-compat request failed`. It must now be transient (#z.ai).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 64];
            let _ = sock.read(&mut buf); // consume the request line, then drop → reset
        }
    });
    let client = reqwest::Client::new();
    let err = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect_err("server drops the connection, so send must fail");
    assert!(
        is_transient_error(&err),
        "a request-send failure (dropped connection) must be transient: \
         {err} (is_request={}, is_connect={}, is_timeout={})",
        err.is_request(),
        err.is_connect(),
        err.is_timeout(),
    );
}

#[test]
fn parse_retry_after_reads_delta_seconds() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Retry-After", "12".parse().unwrap());
    assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));
}

#[test]
fn truncate_on_boundary_keeps_short_bodies_whole() {
    let (shown, truncated) = truncate_on_boundary("hello", 8 * 1024);
    assert_eq!(shown, "hello");
    assert!(!truncated);
}

#[test]
fn truncate_on_boundary_never_splits_a_utf8_char() {
    // "é" is two bytes; capping at 3 must drop it rather than slice mid-char.
    let (shown, truncated) = truncate_on_boundary("aéé", 3);
    assert!(truncated);
    assert_eq!(shown, "aé");
}

#[test]
fn log_request_body_is_silent_without_optin() {
    // No panic and nothing emitted when the flag is unset (the default).
    std::env::remove_var(LOG_BODIES_ENV);
    log_request_body("openai", &serde_json::json!({"messages": []}));
}

#[test]
fn next_backoff_caps_at_max() {
    // Even from a large starting point the doubled+jittered value stays
    // within [max, 2*max).
    let max = Duration::from_secs(30);
    let d = next_backoff(Duration::from_secs(100), max);
    assert!(d >= max && d < max * 2, "got {d:?}");
}
