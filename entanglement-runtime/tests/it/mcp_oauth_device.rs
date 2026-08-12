//! End-to-end test for the RFC 8628 device-code flow (#631, amending
//! ADR-0153): `DeviceFlow::begin` + `PendingDeviceAuthorization::poll` driven
//! against a hand-rolled `tokio::net::TcpListener` mock server (no
//! `wiremock`/`mockito` in this workspace, mirroring `mcp_oauth_refresh.rs`
//! and `entanglement-provider/tests/streaming.rs`).
//!
//! `OauthConfig`'s `authorization_url`/`token_url`/`device_authorization_url`
//! overrides skip discovery entirely (the short-circuit branch in
//! `discovery::discover`), and a configured `client_id` skips dynamic client
//! registration — so the mock server only has to answer two endpoints: device
//! authorization and token. That keeps this test focused on what's actually
//! new here (the device-code poll loop), since discovery and DCR are already
//! covered by `AuthFlow`'s own tests.
//!
//! The mock token endpoint answers `authorization_pending` on the first poll,
//! `slow_down` on the second, and a real grant on the third — proving the
//! poll loop drives all three RFC 8628 §3.4/§3.5 branches against real HTTP,
//! not just the pure `classify_poll_response` unit tests in `device.rs`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use entanglement_core::{DeviceFlow, OauthConfig};

// ── hand-rolled mock server ─────────────────────────────────────────────────

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read one HTTP/1.1 request off `stream`, returning its request-line path
/// and body. No framework — just enough parsing to route by path and drain
/// the `Content-Length` body, mirroring `mcp_oauth_refresh.rs`.
async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).await.expect("read request");
        if n == 0 {
            return String::from_utf8_lossy(&buf).into_owned();
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_len = head
        .to_ascii_lowercase()
        .split("\r\n")
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_len {
        let n = stream.read(&mut tmp).await.expect("read body");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn request_path(raw: &str) -> String {
    raw.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string()
}

fn json_response(status: u16, reason: &str, body: &serde_json::Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// How many times the token endpoint has been polled so far — determines
/// which of the three scripted responses to send.
type PollCount = Arc<AtomicU32>;

async fn handle_conn(mut stream: TcpStream, poll_count: PollCount) {
    let raw = read_http_request(&mut stream).await;
    let path = request_path(&raw);

    let response = match path.as_str() {
        "/device" => json_response(
            200,
            "OK",
            &serde_json::json!({
                "device_code": "test-device-code",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://as.example/device",
                "verification_uri_complete": "https://as.example/device?user_code=ABCD-EFGH",
                "expires_in": 60,
                // Kept tiny so the test's wall-clock assertion stays fast
                // (RFC 8628 doesn't mandate any particular interval).
                "interval": 1
            }),
        ),
        "/token" => {
            let n = poll_count.fetch_add(1, Ordering::SeqCst);
            match n {
                0 => json_response(
                    400,
                    "Bad Request",
                    &serde_json::json!({ "error": "authorization_pending" }),
                ),
                1 => json_response(
                    400,
                    "Bad Request",
                    &serde_json::json!({ "error": "slow_down" }),
                ),
                _ => json_response(
                    200,
                    "OK",
                    &serde_json::json!({
                        "access_token": "granted-access-token",
                        "refresh_token": "granted-refresh-token",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    }),
                ),
            }
        }
        other => json_response(
            404,
            "Not Found",
            &serde_json::json!({ "error": "not_found", "path": other }),
        ),
    };
    let _ = stream.write_all(&response).await;
    let _ = stream.flush().await;
}

/// Bind an ephemeral port and keep accepting connections — a poll loop makes
/// several requests over the test's lifetime, so (unlike a one-shot mock)
/// this has to keep serving.
async fn spawn_device_server() -> (String, PollCount) {
    let poll_count: PollCount = Arc::new(AtomicU32::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accept_count = poll_count.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(handle_conn(stream, accept_count.clone()));
        }
    });
    (format!("http://{addr}"), poll_count)
}

// ── the end-to-end test ──────────────────────────────────────────────────

#[tokio::test]
async fn device_flow_polls_through_pending_and_slow_down_to_a_grant() {
    let (base, poll_count) = spawn_device_server().await;

    let cfg = OauthConfig {
        // Short-circuits discovery entirely (`discovery::discover`'s
        // fully-overridden branch) — `authorization_url` is never actually
        // requested by the device-code flow, but `discover` still requires
        // both it and `token_url` set together to skip the network.
        authorization_url: Some("https://unused.example/authorize".to_string()),
        token_url: Some(format!("{base}/token")),
        device_authorization_url: Some(format!("{base}/device")),
        // A pre-issued client id skips dynamic client registration, so the
        // mock server needs no `/register` route.
        client_id: Some("test-client".to_string()),
        ..Default::default()
    };

    let pending = DeviceFlow::begin("srv", "https://192.0.2.1/mcp", &cfg, None)
        .await
        .expect("begin the device flow");
    assert_eq!(pending.user_code(), "ABCD-EFGH");
    assert_eq!(
        pending.verification_uri_complete(),
        Some("https://as.example/device?user_code=ABCD-EFGH")
    );

    let started = Instant::now();
    let auth = pending.poll().await.expect("poll through to a grant");
    let elapsed = started.elapsed();

    assert_eq!(auth.tokens.access_token, "granted-access-token");
    assert_eq!(
        auth.tokens.refresh_token.as_deref(),
        Some("granted-refresh-token")
    );
    assert_eq!(auth.client_id, "test-client");

    // Three polls happened: authorization_pending, slow_down, grant.
    assert_eq!(
        poll_count.load(Ordering::SeqCst),
        3,
        "expected exactly three token-endpoint polls (pending, slow_down, granted)"
    );

    // The poll loop must actually have taken the `slow_down` backoff path,
    // not just returned early: with a 1s declared interval, the minimum
    // wall-clock spend is the first poll's interval-length wait plus the
    // post-`slow_down` wait of `interval + 5`s.
    let minimum_expected = Duration::from_secs(1 + (1 + 5));
    assert!(
        elapsed >= minimum_expected,
        "expected at least {minimum_expected:?} of poll backoff (slow_down must add 5s to \
         the interval), got {elapsed:?}"
    );
}

#[tokio::test]
async fn device_flow_reports_a_clear_error_when_the_server_offers_no_device_endpoint() {
    // No `device_authorization_url` override and a fully-overridden discovery
    // short-circuit that names none either — `Endpoints::device_authorization_endpoint`
    // comes back `None`, and `DeviceFlow::begin` must fail before ever
    // touching the network for a device code.
    let cfg = OauthConfig {
        authorization_url: Some("https://unused.example/authorize".to_string()),
        token_url: Some("https://unused.example/token".to_string()),
        client_id: Some("test-client".to_string()),
        ..Default::default()
    };
    let err = DeviceFlow::begin("srv", "https://192.0.2.1/mcp", &cfg, None)
        .await
        .expect_err("no device-authorization endpoint must be a clear error");
    assert!(err.to_string().contains("no device-authorization endpoint"));
    assert!(err.to_string().contains("device_authorization_url"));
}
