//! End-to-end test for the web-redirect authorization flow (#684):
//! `WebFlow::begin` + `PendingWebAuthorization::complete` driven against a
//! hand-rolled `tokio::net::TcpListener` mock authorization server (no
//! `wiremock`/`mockito` in this workspace, mirroring
//! `entanglement-runtime/tests/it/mcp_oauth_device.rs` and `tests/streaming.rs`).
//!
//! `OauthConfig`'s `authorization_url`/`token_url`/`registration_url` overrides
//! skip discovery entirely, but `client_id` is deliberately left unset so
//! dynamic client registration *is* exercised with the embedder's own redirect
//! URI — the part that differs from the loopback flow. Between `begin` and
//! `complete` the pending authorization is serialized, dropped, and
//! deserialized, simulating a multi-replica embedder whose callback request
//! lands on a different replica than the one that started the flow.

use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use entanglement_provider::{OauthConfig, PendingWebAuthorization, WebFlow};

// ── hand-rolled mock server ─────────────────────────────────────────────────

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read one HTTP/1.1 request off `stream` (request line + headers + a
/// `Content-Length` body), mirroring `mcp_oauth_device.rs`.
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

fn request_body(raw: &str) -> String {
    raw.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default()
}

fn json_response(status: u16, reason: &str, body: &serde_json::Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Every request the mock served, as `(path, raw request)` — the assertions
/// read captured wire shapes instead of trusting the client's return values.
type Captured = Arc<Mutex<Vec<(String, String)>>>;

async fn handle_conn(mut stream: TcpStream, captured: Captured) {
    let raw = read_http_request(&mut stream).await;
    let path = request_path(&raw);
    captured
        .lock()
        .expect("captured requests lock poisoned")
        .push((path.clone(), raw));

    let response = match path.as_str() {
        "/register" => json_response(
            201,
            "Created",
            &serde_json::json!({ "client_id": "dcr-minted-client" }),
        ),
        "/token" => json_response(
            200,
            "OK",
            &serde_json::json!({
                "access_token": "granted-access-token",
                "refresh_token": "granted-refresh-token",
                "token_type": "Bearer",
                "expires_in": 3600
            }),
        ),
        other => json_response(
            404,
            "Not Found",
            &serde_json::json!({ "error": "not_found", "path": other }),
        ),
    };
    let _ = stream.write_all(&response).await;
    let _ = stream.flush().await;
}

async fn spawn_mock_as() -> (String, Captured) {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accept_captured = captured.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(handle_conn(stream, accept_captured.clone()));
        }
    });
    (format!("http://{addr}"), captured)
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

fn form_param(body: &str, name: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

const REDIRECT_URI: &str = "https://app.example/oauth/mcp/callback";

// ── the end-to-end tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn web_flow_registers_with_the_embedders_redirect_and_completes_after_a_replica_handoff() {
    let (base, captured) = spawn_mock_as().await;

    let cfg = OauthConfig {
        // Short-circuits discovery entirely; `client_id` stays unset so DCR
        // runs — with the *web* redirect URI, the part under test.
        authorization_url: Some("https://as.example/authorize".to_string()),
        token_url: Some(format!("{base}/token")),
        registration_url: Some(format!("{base}/register")),
        scopes: vec!["read".to_string()],
        ..Default::default()
    };

    let pending = WebFlow::begin(
        "kb",
        "https://192.0.2.1/mcp",
        &cfg,
        None,
        REDIRECT_URI,
        "kb-app",
    )
    .await
    .expect("begin the web flow");

    // DCR carried the embedder's redirect URI and product name, as a public
    // authorization-code client.
    let registration = {
        let captured = captured.lock().expect("captured requests lock poisoned");
        let (_, raw) = captured
            .iter()
            .find(|(path, _)| path == "/register")
            .expect("dynamic client registration must have run")
            .clone();
        serde_json::from_str::<serde_json::Value>(&request_body(&raw)).expect("DCR body is JSON")
    };
    assert_eq!(
        registration["redirect_uris"],
        serde_json::json!([REDIRECT_URI])
    );
    assert_eq!(registration["client_name"], "kb-app");
    assert_eq!(registration["grant_types"][0], "authorization_code");
    assert_eq!(registration["token_endpoint_auth_method"], "none");

    // The authorize URL sends the browser back to the embedder's callback.
    let url = pending.authorize_url().to_string();
    assert!(url.starts_with("https://as.example/authorize?"));
    assert_eq!(
        query_param(&url, "client_id").as_deref(),
        Some("dcr-minted-client")
    );
    assert_eq!(
        query_param(&url, "redirect_uri").as_deref(),
        Some("https%3A%2F%2Fapp.example%2Foauth%2Fmcp%2Fcallback")
    );
    let challenge = query_param(&url, "code_challenge").expect("PKCE challenge in the URL");
    let state = query_param(&url, "state").expect("state in the URL");
    assert_eq!(state, pending.state());

    // The multi-replica handoff: the callback lands on a replica that never
    // saw `begin`, so the pending entry round-trips through a shared store.
    let stored = serde_json::to_string(&pending).expect("serialize the pending entry");
    drop(pending);
    let restored: PendingWebAuthorization =
        serde_json::from_str(&stored).expect("deserialize on the other replica");

    let auth = restored
        .complete("the-code", &state)
        .await
        .expect("exchange the code");
    assert_eq!(auth.client_id, "dcr-minted-client");
    assert_eq!(auth.token_endpoint, format!("{base}/token"));
    assert_eq!(auth.tokens.access_token, "granted-access-token");
    assert_eq!(
        auth.tokens.refresh_token.as_deref(),
        Some("granted-refresh-token")
    );

    // The token exchange carried the code, the embedder's redirect URI, and a
    // verifier that actually hashes to the challenge from the authorize URL —
    // proving the PKCE secret survived the serialization handoff.
    let token_body = {
        let captured = captured.lock().expect("captured requests lock poisoned");
        let (_, raw) = captured
            .iter()
            .find(|(path, _)| path == "/token")
            .expect("the token endpoint must have been hit")
            .clone();
        request_body(&raw)
    };
    assert_eq!(
        form_param(&token_body, "grant_type").as_deref(),
        Some("authorization_code")
    );
    assert_eq!(form_param(&token_body, "code").as_deref(), Some("the-code"));
    assert_eq!(
        form_param(&token_body, "redirect_uri").as_deref(),
        Some("https%3A%2F%2Fapp.example%2Foauth%2Fmcp%2Fcallback")
    );
    let verifier = form_param(&token_body, "code_verifier").expect("verifier in the token form");
    let hashed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    assert_eq!(
        hashed, challenge,
        "the exchanged code_verifier must hash to the authorize URL's code_challenge"
    );
}

#[tokio::test]
async fn a_mismatched_state_never_reaches_the_token_endpoint() {
    let (base, captured) = spawn_mock_as().await;

    let cfg = OauthConfig {
        authorization_url: Some("https://as.example/authorize".to_string()),
        token_url: Some(format!("{base}/token")),
        // A pre-issued client id: no `/register` route needed here.
        client_id: Some("pre-issued".to_string()),
        ..Default::default()
    };

    let pending = WebFlow::begin(
        "kb",
        "https://192.0.2.1/mcp",
        &cfg,
        None,
        REDIRECT_URI,
        "kb-app",
    )
    .await
    .expect("begin the web flow");

    let err = pending
        .complete("the-code", "attacker-state")
        .await
        .expect_err("a wrong state must be rejected");
    assert!(err.to_string().contains("state mismatch"), "{err:#}");

    let token_hits = captured
        .lock()
        .expect("captured requests lock poisoned")
        .iter()
        .filter(|(path, _)| path == "/token")
        .count();
    assert_eq!(
        token_hits, 0,
        "the rejection must happen before any network I/O"
    );
}
