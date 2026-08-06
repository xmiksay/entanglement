//! Unit tests for the shared HTTP transport (pool identity, pacing gate,
//! retry-after window, transient-error classification).

use super::*;

#[test]
fn test_new_http_client() {
    let _client = HttpClient::new().unwrap();
}

#[test]
fn endpoints_are_isolated_and_stable_by_key() {
    let http = HttpClient::new().unwrap();
    let a1 = http.endpoint("https://api.a/v1", None, None);
    let a2 = http.endpoint("https://api.a/v1", None, None);
    let b = http.endpoint("https://api.b/v1", None, None);
    // Same key → same state; different keys → isolated state.
    assert!(Arc::ptr_eq(&a1, &a2));
    assert!(!Arc::ptr_eq(&a1, &b));
}

#[test]
fn endpoint_uses_provided_rpm_budget() {
    let http = HttpClient::new().unwrap();
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
    })
    .unwrap();
    let ep = http.endpoint("https://api.x/v1", None, None);
    // The in-flight cap is seeded from config; the default is DEFAULT_CONCURRENCY.
    assert_eq!(ep.concurrency.available_permits(), 2);
    let dflt = HttpClient::new()
        .unwrap()
        .endpoint("https://api.y/v1", None, None);
    assert_eq!(dflt.concurrency.available_permits(), DEFAULT_CONCURRENCY);
}

#[test]
fn endpoint_uses_provided_concurrency_cap_over_pool_default() {
    let http = HttpClient::with_config(RetryConfig {
        concurrency: 2,
        ..RetryConfig::default()
    })
    .unwrap();
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
    let ep = EndpointState::new(RPM_LIMIT, 1, "test");
    let first = ep.concurrency.clone().acquire_owned().await.unwrap();
    assert!(ep.concurrency.clone().try_acquire_owned().is_err());
    drop(first);
    assert!(ep.concurrency.clone().try_acquire_owned().is_ok());
}

#[tokio::test]
async fn model_slot_bounds_in_flight_per_model() {
    // With a per-model cap of 1 (e.g. GLM-4.7-Flash), a second owned permit for
    // that same model can't be taken until the first is dropped — the property
    // that serializes calls to a model z.ai only allows one in-flight for
    // (#521).
    let ep = EndpointState::new(RPM_LIMIT, DEFAULT_CONCURRENCY, "test");
    let slot = ep.model_slot("glm-4.7-flash", 1);
    let first = slot.semaphore.clone().acquire_owned().await.unwrap();
    assert!(slot.semaphore.clone().try_acquire_owned().is_err());
    drop(first);
    assert!(slot.semaphore.clone().try_acquire_owned().is_ok());
}

#[tokio::test]
async fn model_slot_is_independent_across_models_on_the_same_endpoint() {
    // GLM-5.2 (cap 5) and GLM-4.7-Flash (cap 1) share one endpoint but must
    // meter independently — one saturated model must never block another
    // (#521): the mixed-model scenario ADR-0140 exists to fix.
    let ep = EndpointState::new(RPM_LIMIT, DEFAULT_CONCURRENCY, "test");
    let flash = ep.model_slot("glm-4.7-flash", 1);
    let glm52 = ep.model_slot("glm-5.2", 5);
    let _flash_permit = flash.semaphore.clone().acquire_owned().await.unwrap();
    // Flash is now saturated (cap 1), but GLM-5.2's independent semaphore is
    // untouched — up to 5 concurrent GLM-5.2 permits are still available.
    assert!(flash.semaphore.clone().try_acquire_owned().is_err());
    let permits: Vec<_> = (0..5)
        .map(|_| glm52.semaphore.clone().try_acquire_owned().unwrap())
        .collect();
    assert!(glm52.semaphore.clone().try_acquire_owned().is_err());
    drop(permits);
}

#[tokio::test]
async fn model_permit_blocked_never_holds_the_endpoint_permit_hostage() {
    // Regression for the #521 refinement: `execute_with_retry` acquires the
    // model permit *before* the endpoint permit specifically so a caller
    // blocked on its own saturated model never sits holding a scarce
    // endpoint-wide permit — which would starve every *other* model sharing
    // the endpoint of admission, even with room to spare.
    use futures::FutureExt;

    let ep = EndpointState::new(RPM_LIMIT, 2, "test");
    let flash = ep.model_slot("glm-4.7-flash", 1);

    // One in-flight Flash call: holds both its model permit and one of the
    // endpoint's two permits, exactly as `execute_with_retry` would.
    let _flash_model_permit = flash.semaphore.clone().acquire_owned().await.unwrap();
    let _flash_endpoint_permit = ep.concurrency.clone().acquire_owned().await.unwrap();

    // A second Flash call queues on the now-exhausted model semaphore — it
    // must not complete, and critically must never have touched the endpoint
    // semaphore to get there.
    let second_flash_model_attempt = flash.semaphore.clone().acquire_owned();
    assert!(
        second_flash_model_attempt.now_or_never().is_none(),
        "a second call to a saturated model must block on the model semaphore"
    );
    assert_eq!(
        ep.concurrency.available_permits(),
        1,
        "the endpoint's spare permit must stay free while a caller is \
         blocked purely on its own model's semaphore"
    );

    // A *different* model, sharing the same endpoint, must still get that
    // spare endpoint permit immediately — it is not starved by Flash's
    // internally-queued caller.
    assert!(
        ep.concurrency.clone().try_acquire_owned().is_ok(),
        "a sibling model must not be starved of endpoint room by another \
         model's queued caller"
    );
}

#[tokio::test]
async fn model_permit_wait_does_not_hold_the_shared_cross_process_lease() {
    // Regression for #546: `execute_with_retry` must acquire the shared
    // cross-process lease *after* both in-process permits (the innermost
    // gate). The shipped z.ai config that triggered this: endpoint cap 3,
    // `glm-4.7-flash` model cap 1 — a caller queued on the model semaphore
    // must not sit holding one of the 3 shared leases, or it starves sibling
    // `skutter` processes sharing the same endpoint+key even though this
    // process has sent nothing.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let dir = tempfile::tempdir().expect("tempdir");
    let key = "https://api.example/v1#shared-lease-test";
    let (http, ep) = {
        // Held only across the env var + endpoint construction, never across
        // an `.await` (`clippy::await_holding_lock`) — the shared-state dir
        // is only read here, at `SharedGate::new` time.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("ENTANGLEMENT_SHARED_STATE_DIR", dir.path());
        let http = HttpClient::with_config(RetryConfig {
            concurrency: 3,
            ..RetryConfig::default()
        })
        .unwrap();
        let ep = http.endpoint(key, None, Some(3));
        std::env::remove_var("ENTANGLEMENT_SHARED_STATE_DIR");
        (http, ep)
    };

    // Simulate one in-flight Flash call already holding the model's only permit.
    let model_slot = ep.model_slot("glm-4.7-flash", 1);
    let _held_model_permit = model_slot.semaphore.clone().acquire_owned().await.unwrap();

    // A second call to the same model, driven through the real entry point,
    // must queue on the model semaphore before ever touching the shared gate.
    let http2 = http.clone();
    let key2 = key.to_string();
    let blocked = tokio::spawn(async move {
        let _ = http2
            .execute_with_retry(
                &key2,
                None,
                None,
                Some(3),
                "glm-4.7-flash",
                Some(1),
                None,
                || async {
                    std::future::pending::<Result<reqwest::Response, reqwest::Error>>().await
                },
            )
            .await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !blocked.is_finished(),
        "must still be queued on the saturated model permit"
    );

    // A sibling "process" touching the same shared-state file must be able to
    // take the *whole* endpoint-wide shared budget (3) right away — proving
    // the blocked caller holds none of it. Bounded by a timeout: under the
    // pre-fix ordering this would hang forever (the blocked caller holds one
    // of the 3 leases and never releases it).
    let sibling_deadline = Instant::now() + Duration::from_secs(2);
    let siblings = tokio::time::timeout(
        Duration::from_secs(2),
        futures::future::join_all(
            (0..3).map(|_| ep.shared.acquire(RPM_LIMIT, 3, sibling_deadline)),
        ),
    )
    .await
    .expect(
        "a sibling process must not be starved while the caller is blocked \
         purely on its own model permit",
    );
    assert!(
        siblings.iter().all(|s| matches!(s, Ok(Some(_)))),
        "the full shared cap must be available to siblings"
    );

    blocked.abort();
}

#[test]
fn model_cap_wider_than_endpoint_cap_warns_but_is_still_honored() {
    // A model cap wider than its endpoint's own is a likely misconfiguration
    // (the narrower endpoint cap binds first, so the model cap can never
    // actually be reached) but must warn, not refuse to build the slot.
    let ep = EndpointState::new(RPM_LIMIT, 2, "test");
    let slot = ep.model_slot("glm-5.2", 10);
    assert_eq!(slot.cap, 10);
    assert_eq!(slot.semaphore.available_permits(), 10);
}

#[test]
fn model_slot_is_stable_by_model_id_while_the_cap_agrees() {
    let ep = EndpointState::new(RPM_LIMIT, DEFAULT_CONCURRENCY, "test");
    let a1 = ep.model_slot("glm-5.2", 5);
    let a2 = ep.model_slot("glm-5.2", 5);
    // Same model id, same cap → the same slot (and thus the same live
    // semaphore state) — no needless churn on the common case.
    assert!(Arc::ptr_eq(&a1, &a2));
    assert_eq!(a1.cap, 5);
    // A different model id gets its own independent slot.
    let flash = ep.model_slot("glm-4.7-flash", 1);
    assert!(!Arc::ptr_eq(&a1, &flash));
    assert_eq!(flash.cap, 1);
}

#[test]
fn model_slot_corrects_a_stale_cap_instead_of_latching_it_forever() {
    // #550: a wrong cap reaching `model_slot` first (e.g. resolved against the
    // wrong model before the per-request fix) must not stick for the rest of
    // the process — a later, correct caller has to be able to fix it.
    let ep = EndpointState::new(RPM_LIMIT, DEFAULT_CONCURRENCY, "test");
    let wrong = ep.model_slot("glm-4.7-flash", 5);
    assert_eq!(wrong.cap, 5);
    let corrected = ep.model_slot("glm-4.7-flash", 1);
    assert!(!Arc::ptr_eq(&wrong, &corrected));
    assert_eq!(corrected.cap, 1);
    // Every subsequent caller now sees the corrected slot.
    let again = ep.model_slot("glm-4.7-flash", 1);
    assert!(Arc::ptr_eq(&corrected, &again));
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
fn pool_key_normalizes_trailing_slash_and_host_case() {
    // #551: a trailing slash or a differently-cased host must not split one
    // real endpoint's budget into two — `ZAI_API_BASE=.../v4/` and the
    // catalog's `.../v4` are the same endpoint.
    assert_eq!(
        pool_key("https://api.z.ai/v4/", Some("k1")),
        pool_key("https://api.z.ai/v4", Some("k1"))
    );
    assert_eq!(
        pool_key("https://Api.Z.AI/v4", Some("k1")),
        pool_key("https://api.z.ai/v4", Some("k1"))
    );
    // The path itself stays case-sensitive — only the host is folded.
    assert_ne!(
        pool_key("https://api.z.ai/V4", Some("k1")),
        pool_key("https://api.z.ai/v4", Some("k1"))
    );
    // Keyless normalizes the same way.
    assert_eq!(
        pool_key("https://api.z.ai/v4/", None),
        pool_key("https://api.z.ai/v4", None)
    );
}

#[test]
fn pool_key_is_stable_across_calls_within_and_across_processes() {
    // #551: the pool key becomes the cross-process shared-state file name
    // (`shared_store::hash_key`), so it must not depend on per-process/
    // per-toolchain hash randomization the way `DefaultHasher` did.
    let a = pool_key("https://api.z.ai/v4", Some("supersecret"));
    let b = pool_key("https://api.z.ai/v4", Some("supersecret"));
    assert_eq!(a, b);
    // Matches an independently-computed sha256("supersecret"), so a future
    // accidental regression back to an unspecified hasher is caught
    // immediately rather than only by a cross-process integration test.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"supersecret");
    assert!(a.ends_with(&format!("#{:x}", hasher.finalize())));
}

#[test]
fn retry_after_window_extends_never_shrinks() {
    let state = EndpointState::new(RPM_LIMIT, DEFAULT_CONCURRENCY, "test");
    state.set_retry_after(Duration::from_secs(10));
    let long = state.retry_after.lock().unwrap().unwrap();
    // A shorter window must not overwrite a longer one.
    state.set_retry_after(Duration::from_secs(1));
    assert_eq!(state.retry_after.lock().unwrap().unwrap(), long);
    // A longer window does extend it.
    state.set_retry_after(Duration::from_secs(60));
    assert!(state.retry_after.lock().unwrap().unwrap() > long);
}

#[tokio::test]
async fn wait_for_retry_after_gives_up_when_the_cool_down_exceeds_the_deadline() {
    // #547: a caller must never sleep past its own `rate_limit_max_elapsed`
    // budget, regardless of how far out the endpoint's cool-down is.
    let state = EndpointState::new(RPM_LIMIT, DEFAULT_CONCURRENCY, "test");
    state.set_retry_after(Duration::from_secs(3600));
    let deadline = Instant::now() + Duration::from_millis(50);
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        state.wait_for_retry_after(deadline),
    )
    .await
    .expect("must give up promptly instead of sleeping toward the cool-down");
    assert_eq!(result, Err(()), "a cool-down past the deadline must bail");
}

#[tokio::test]
async fn wait_for_retry_after_waits_normally_within_the_deadline() {
    let state = EndpointState::new(RPM_LIMIT, DEFAULT_CONCURRENCY, "test");
    state.set_retry_after(Duration::from_millis(20));
    let deadline = Instant::now() + Duration::from_secs(5);
    let result = state.wait_for_retry_after(deadline).await;
    assert_eq!(result, Ok(()));
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

// ── header-phase timeout (#658) ─────────────────────────────────────────────

#[tokio::test]
async fn header_timeout_retries_and_recovers_when_the_server_replies_late() {
    // The incident: a server accepts the connection, takes the request, then
    // goes silent — no headers, no error. Before #658 this hung
    // `execute_with_retry` forever. The header-phase timeout must fire, drop
    // the silent connection, and retry against a fresh one — exactly the
    // recovery the incident report observed once the dead TCP connection was
    // killed manually.
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        // First connection: accept, drain the request, then hold the socket
        // open and silent — never write headers. Handled on its own thread so
        // it can't block accepting the retry's connection below.
        if let Ok((mut sock, _)) = listener.accept() {
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                std::thread::sleep(Duration::from_secs(3));
            });
        }
        // Second connection (the retry): reply normally.
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
            );
            let _ = sock.flush();
        }
    });

    let http = HttpClient::with_config(RetryConfig {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(10),
        rpm: 100_000, // effectively no pacing gap between attempts
        response_header_timeout: Duration::from_millis(80),
        ..RetryConfig::default()
    })
    .unwrap();

    let url = format!("http://{addr}/");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        http.execute_with_retry(&url, None, None, None, "test-model", None, None, || {
            http.client().get(&url).send()
        }),
    )
    .await
    .expect("must not hang past the header timeout + one retry");

    let (response, _guard) = result.expect("the second attempt must succeed");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn header_timeout_exhausted_after_max_attempts_returns_a_typed_error() {
    // A server that stays silent on *every* attempt must surface a clear,
    // typed error once the retry budget is spent — not hang forever.
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        while let Ok((mut sock, _)) = listener.accept() {
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                std::thread::sleep(Duration::from_secs(3));
            });
        }
    });

    let http = HttpClient::with_config(RetryConfig {
        max_attempts: 2,
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(5),
        rpm: 100_000,
        response_header_timeout: Duration::from_millis(50),
        ..RetryConfig::default()
    })
    .unwrap();

    let url = format!("http://{addr}/");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        http.execute_with_retry(&url, None, None, None, "test-model", None, None, || {
            http.client().get(&url).send()
        }),
    )
    .await
    .expect("must not hang past 2 attempts × the header timeout");

    match result {
        Err(RetryError::HeaderTimeout(timeout, attempts)) => {
            assert_eq!(timeout, Duration::from_millis(50));
            assert_eq!(attempts, 2);
        }
        Err(other) => panic!("expected RetryError::HeaderTimeout, got {other}"),
        Ok(_) => panic!("expected RetryError::HeaderTimeout, got Ok"),
    }
}

#[tokio::test]
async fn header_timeout_does_not_apply_once_headers_have_arrived() {
    // The header-phase timeout must bound only the wait *for* headers. Once a
    // `Response` exists, liveness is `spawn_byte_stream`'s own per-chunk
    // `STREAM_IDLE_TIMEOUT` watchdog — a slow-but-alive body that outlives
    // `response_header_timeout` must still complete normally (must not
    // regress the #241 guarantee that a long healthy stream runs to
    // completion).
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\n",
            );
            let _ = sock.flush();
            // Headers are already out; the body now drips slower than the
            // header-phase timeout but well inside the idle-gap one.
            std::thread::sleep(Duration::from_millis(150));
            let _ = sock.write_all(b"hello");
            let _ = sock.flush();
        }
    });

    let http = HttpClient::with_config(RetryConfig {
        rpm: 100_000,
        response_header_timeout: Duration::from_millis(60),
        ..RetryConfig::default()
    })
    .unwrap();

    let url = format!("http://{addr}/");
    let (response, _guard) = tokio::time::timeout(
        Duration::from_secs(5),
        http.execute_with_retry(&url, None, None, None, "test-model", None, None, || {
            http.client().get(&url).send()
        }),
    )
    .await
    .expect("must not hang")
    .expect("headers arrive within the header timeout even though the body is still slow");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("read body");
    assert_eq!(body, "hello");
}

#[test]
fn parse_retry_after_reads_delta_seconds() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Retry-After", "12".parse().unwrap());
    assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));
}

#[test]
fn parse_retry_after_clamps_huge_value() {
    // A hostile/misbehaving endpoint sending `Retry-After: u64::MAX` must not
    // overflow `Instant + Duration` downstream (#548): the parsed value is
    // clamped to a sane ceiling instead of passed through as-is.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Retry-After", u64::MAX.to_string().parse().unwrap());
    let delay = parse_retry_after(&headers).expect("huge value still parses");
    assert_eq!(delay, MAX_RETRY_AFTER);
    // The whole point: this must not panic.
    let _ = Instant::now() + delay;
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

#[test]
fn waiter_guard_increments_on_construction_and_decrements_on_drop() {
    let counter = AtomicUsize::new(0);
    {
        let _guard = WaiterGuard::new(&counter);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }
    assert_eq!(counter.load(Ordering::Relaxed), 0);
}

// ── spawn_byte_stream guard release (#552) ──────────────────────────────────

/// Write a bare HTTP/1.1 response (no framing that lets the client know the
/// body is complete) to `sock`, then hold the connection open and silent for
/// a while — standing in for a keep-alive proxy that never closes after
/// `[DONE]`, or a provider connection sitting through a silent reasoning
/// pause. Blocking (runs on its own `std::thread`) since it must hold the
/// socket open independent of the async client under test.
fn serve_one_response_then_hold_open(listener: std::net::TcpListener, body: &'static [u8]) {
    use std::io::{Read, Write};
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf); // drain the request
            let mut head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n".to_vec();
            head.extend_from_slice(body);
            let _ = sock.write_all(&head);
            let _ = sock.flush();
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

#[tokio::test]
async fn spawn_byte_stream_releases_the_guard_promptly_when_the_consumer_drops() {
    // Before #552 the pump task only noticed a gone consumer on its *next*
    // chunk-send attempt or the 120s idle timeout — neither of which fires
    // while the body is silently paused. A `Stop` mid reasoning pause, or
    // `[DONE]` under a keep-alive proxy, dropped the receiver without either
    // ever happening, so the guard (endpoint/model permits, cross-process
    // lease) stayed held for up to the full idle timeout. Racing the read
    // against `tx.closed()` must free it right away instead.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    serve_one_response_then_hold_open(listener, b"data: some event\n\n");

    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("connect");

    let sem = Arc::new(Semaphore::new(1));
    let permit = sem.clone().try_acquire_owned().expect("only permit");
    let guard = StreamGuard(permit, None, None, None);

    let rx = spawn_byte_stream(response, "test", guard);
    // No one ever reads from `rx` — simulates `Stop` dropping the whole
    // `LlmStream` (and the mpsc receiver inside it) mid silent pause.
    drop(rx);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        sem.available_permits(),
        1,
        "the permit must be released promptly, not held for the idle timeout"
    );
}

#[tokio::test]
async fn spawn_byte_stream_still_forwards_chunks_and_ends_cleanly_on_close() {
    // Guards against the fix above breaking the ordinary path: a normally
    // draining consumer must still see every chunk and a clean end when the
    // server actually closes the connection.
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nhello",
            );
            let _ = sock.flush();
            // Dropping `sock` here (end of scope) closes the connection.
        }
    });

    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("connect");

    let sem = Arc::new(Semaphore::new(1));
    let permit = sem.clone().try_acquire_owned().expect("only permit");
    let guard = StreamGuard(permit, None, None, None);

    let mut rx = spawn_byte_stream(response, "test", guard);
    let mut collected = Vec::new();
    while let Some(chunk) = rx.recv().await {
        collected.extend_from_slice(&chunk.expect("no read error"));
    }
    assert_eq!(collected, b"hello");
    // The pump task drops its guard as it exits; give it a beat to run.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(sem.available_permits(), 1);
}
