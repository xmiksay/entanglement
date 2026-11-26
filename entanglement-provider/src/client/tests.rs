//! Unit tests for the shared HTTP transport (pool identity, the retry loop,
//! transient-error classification, the byte pump). The limiter's own tests
//! live in `limiter_tests.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tokio::sync::Semaphore;

use super::backoff::*;
use super::endpoint::*;
use super::stream::*;
use super::*;

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
    let client = super::client_builder().build().unwrap();
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
        // open and silent — never write headers. Held far longer than the
        // test can take, so a slow host can't turn the silence into an EOF
        // (a different, non-header-timeout failure). Every later connection
        // (the retries) replies normally, so a retry that itself ran slow
        // under load still lands on a live server.
        let mut first = true;
        while let Ok((mut sock, _)) = listener.accept() {
            if std::mem::take(&mut first) {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    std::thread::sleep(Duration::from_secs(60));
                });
                continue;
            }
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
            );
            let _ = sock.flush();
        }
    });

    let http = test_http_client_with(RetryConfig {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(10),
        // Wide enough that the retry's own (prompt) reply always beats it on a
        // loaded host; the silent first connection still trips it.
        response_header_timeout: Duration::from_millis(500),
        ..RetryConfig::default()
    });

    let url = format!("http://{addr}/");
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        http.execute_with_retry(
            &url,
            None,
            "test-model",
            RequestLimits::default(),
            0,
            None,
            || http.client().get(&url).send(),
        ),
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
                // Outlives the test so silence never degrades into an EOF.
                std::thread::sleep(Duration::from_secs(60));
            });
        }
    });

    let http = test_http_client_with(RetryConfig {
        max_attempts: 2,
        initial_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(5),
        response_header_timeout: Duration::from_millis(200),
        ..RetryConfig::default()
    });

    let url = format!("http://{addr}/");
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        http.execute_with_retry(
            &url,
            None,
            "test-model",
            RequestLimits::default(),
            0,
            None,
            || http.client().get(&url).send(),
        ),
    )
    .await
    .expect("must not hang past 2 attempts × the header timeout");

    match result {
        Err(RetryError::HeaderTimeout(timeout, attempts)) => {
            assert_eq!(timeout, Duration::from_millis(200));
            assert_eq!(attempts, 2);
        }
        Err(other) => panic!("expected RetryError::HeaderTimeout, got {other}"),
        Ok(_) => panic!("expected RetryError::HeaderTimeout, got Ok"),
    }
}

#[tokio::test]
async fn minimal_retry_config_gives_up_fast_against_a_dead_endpoint() {
    // #560 aux fail-fast follow-up: a caller passing `RetryConfig::minimal()`
    // as `execute_with_retry`'s per-call `retry` override must fail a
    // connect-level failure (connection refused — the "aux-models.yml pins a
    // dead ollama endpoint" incident) in at most 2 attempts (one retry), not
    // the 5-attempt default ladder. `attempts == 2` is what tells the configs
    // apart; the wall-clock bound below is only a loose hang guard, so a
    // loaded host can't flake it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    drop(listener); // nothing listens now — every connect attempt is refused

    // The pool's own config stays the ordinary default; `minimal()` rides in
    // as the per-call override, exactly like the aux call sites do.
    let http = test_http_client();

    let url = format!("http://{addr}/");
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        http.execute_with_retry(
            &url,
            None,
            "test-model",
            RequestLimits::default(),
            0,
            Some(RetryConfig::minimal()),
            || http.client().get(&url).send(),
        ),
    )
    .await
    .expect("must not hang against a refused connection");

    match result {
        Err(RetryError::Exhausted(attempts, _)) => assert_eq!(attempts, 2),
        Err(other) => panic!("expected Exhausted(2, _), got {other}"),
        Ok(_) => panic!("expected an error against a dead endpoint"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "minimal retry must fail fast, took {:?}",
        started.elapsed()
    );
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
            std::thread::sleep(Duration::from_millis(1000));
            let _ = sock.write_all(b"hello");
            let _ = sock.flush();
        }
    });

    let http = test_http_client_with(RetryConfig {
        response_header_timeout: Duration::from_millis(300),
        ..RetryConfig::default()
    });

    let url = format!("http://{addr}/");
    let (response, _guard) = tokio::time::timeout(
        Duration::from_secs(30),
        http.execute_with_retry(
            &url,
            None,
            "test-model",
            RequestLimits::default(),
            0,
            None,
            || http.client().get(&url).send(),
        ),
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
    // No panic and nothing emitted when the flag is unset (the default; the
    // test-isolated runner strips every `ENTANGLEMENT_*` var).
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

// ── 429 + `Retry-After` through the retry loop (#918) ───────────────────────

/// Serve `first` on the first connection and a 200 on every later one,
/// returning the address and the instant each request arrived.
fn serve_then_ok(
    first: &'static [u8],
) -> (std::net::SocketAddr, Arc<Mutex<Vec<std::time::Instant>>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let seen = arrivals.clone();
    std::thread::spawn(move || {
        while let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(std::time::Instant::now());
                seen.len()
            };
            let reply: &[u8] = if n == 1 {
                first
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
            };
            let _ = sock.write_all(reply);
            let _ = sock.flush();
        }
    });
    (addr, arrivals)
}

#[tokio::test]
async fn a_429_parks_the_endpoint_for_retry_after_then_succeeds() {
    let (addr, arrivals) = serve_then_ok(
        b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let http = test_http_client();
    let url = format!("http://{addr}/");
    let (response, _guard) = tokio::time::timeout(
        Duration::from_secs(20),
        http.execute_with_retry(&url, None, "m", RequestLimits::default(), 0, None, || {
            http.client().get(&url).send()
        }),
    )
    .await
    .expect("must not hang")
    .expect("the retry after the cool-down succeeds");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let arrivals = arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 2);
    assert!(
        arrivals[1] - arrivals[0] >= Duration::from_millis(900),
        "the retry must wait out Retry-After, waited {:?}",
        arrivals[1] - arrivals[0]
    );
}

#[tokio::test]
async fn a_retry_after_past_the_budget_surfaces_the_429() {
    let (addr, arrivals) = serve_then_ok(
        b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    let http = test_http_client_with(RetryConfig {
        rate_limit_max_elapsed: Duration::from_secs(5),
        ..RetryConfig::default()
    });
    let url = format!("http://{addr}/");
    let (response, _guard) = tokio::time::timeout(
        Duration::from_secs(10),
        http.execute_with_retry(&url, None, "m", RequestLimits::default(), 0, None, || {
            http.client().get(&url).send()
        }),
    )
    .await
    .expect("gives up instead of sleeping an hour")
    .expect("the 429 itself comes back for the caller to report");
    assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(arrivals.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_caller_queued_on_its_model_holds_no_shared_lease() {
    // #546: the cross-process lease is taken last, so a caller waiting on
    // its own model's slot never starves sibling processes of the endpoint.
    let dir = tempfile::tempdir().expect("tempdir");
    let key = "https://api.example/v1#shared-lease-test";
    let http = HttpClient::with_shared_state(
        SharedStateLocation::Dir(dir.path().to_path_buf()),
        RetryConfig::default(),
    )
    .unwrap();
    let limits = RequestLimits {
        endpoint: RateLimits {
            concurrency: Some(3),
            ..RateLimits::default()
        },
        model: RateLimits {
            concurrency: Some(1),
            ..RateLimits::default()
        },
    };
    let ep = http.endpoint(key, limits.endpoint);
    let deadline = Instant::now() + Duration::from_secs(10);
    let _held = ep
        .admit("glm-4.7-flash", limits, 0, deadline)
        .await
        .unwrap();

    let http2 = http.clone();
    let blocked = tokio::spawn(async move {
        let _ = http2
            .execute_with_retry(key, None, "glm-4.7-flash", limits, 0, None, || async {
                std::future::pending::<Result<reqwest::Response, reqwest::Error>>().await
            })
            .await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!blocked.is_finished(), "queued on the model slot");

    // The held call has one lease; siblings must get the other two at once.
    let siblings = tokio::time::timeout(
        Duration::from_secs(10),
        futures::future::join_all((0..2).map(|_| ep.shared.acquire(1000, 3, deadline))),
    )
    .await
    .expect("siblings are not starved by the queued caller");
    assert!(siblings.iter().all(|s| matches!(s, Ok(Some(_)))));
    blocked.abort();
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

    let client = super::client_builder().build().unwrap();
    let response = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("connect");

    let sem = Arc::new(Semaphore::new(1));
    let permit = sem.clone().try_acquire_owned().expect("only permit");
    let guard = StreamGuard(Some(permit), None, None);

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

    let client = super::client_builder().build().unwrap();
    let response = client
        .get(format!("http://{addr}/"))
        .send()
        .await
        .expect("connect");

    let sem = Arc::new(Semaphore::new(1));
    let permit = sem.clone().try_acquire_owned().expect("only permit");
    let guard = StreamGuard(Some(permit), None, None);

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
