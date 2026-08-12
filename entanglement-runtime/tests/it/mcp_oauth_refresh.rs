//! Regression test for the cross-process MCP OAuth refresh race (#631,
//! ADR-0153's "Consequences" section): two `skutter` processes racing a
//! refresh of the same rotating `refresh_token` must not have the loser
//! surface an `invalid_grant` failure to its caller.
//!
//! Simulates "two processes sharing one token file" with two separate
//! [`McpTokenStore`] handles pointed at the same path — exactly what two real
//! `skutter` instances look like, since neither caches anything in memory
//! (`config::mcp_tokens`'s module doc). A hand-rolled `tokio::net::TcpListener`
//! mock token endpoint (no `wiremock`/`mockito` in this workspace, mirroring
//! `entanglement-provider/tests/streaming.rs`) tracks how many times each
//! specific `refresh_token` value is redeemed and 400s a second redemption of
//! an already-consumed one, the way a real rotating-refresh-token
//! authorization server does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use entanglement_core::{AccessTokenSource, StoredAuth, StoredTokenSource, TokenSet, TokenStore};
use entanglement_runtime::config::McpTokenStore;

const TOKENS_FILE_ENV: &str = "ENTANGLEMENT_MCP_TOKENS_FILE";

// ── hand-rolled mock token endpoint ─────────────────────────────────────────

/// Read one HTTP request's headers + `Content-Length` body off `stream`, the
/// way `entanglement-provider/tests/streaming.rs` does for its mock LLM
/// endpoint — no framework, just enough HTTP/1.1 to see the POSTed form body.
async fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).await.expect("read request");
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let content_len = headers
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
    buf
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A tiny `application/x-www-form-urlencoded` decoder — the values this test
/// posts are plain opaque tokens, but percent-decoding `+`/`%XX` keeps this
/// correct rather than merely lucky.
fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let mut parts = kv.splitn(2, '=');
            let k = percent_decode(parts.next().unwrap_or(""));
            let v = percent_decode(parts.next().unwrap_or(""));
            (k, v)
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_response(status: u16, reason: &str, body: &serde_json::Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Redemption counts per `refresh_token` value: first redemption of a given
/// value succeeds and rotates it; any further redemption of that same
/// (already-consumed) value gets an OAuth `invalid_grant` 400, matching a
/// real rotating-refresh-token authorization server.
type Counts = Arc<Mutex<HashMap<String, u32>>>;

async fn handle_conn(mut stream: TcpStream, counts: Counts) {
    let raw = read_http_request(&mut stream).await;
    let text = String::from_utf8_lossy(&raw);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let form = parse_form(body);
    let refresh = form.get("refresh_token").cloned().unwrap_or_default();

    let already_consumed = {
        let mut counts = counts.lock().unwrap_or_else(|p| p.into_inner());
        let count = counts.entry(refresh.clone()).or_insert(0);
        let consumed = *count > 0;
        *count += 1;
        consumed
    };

    let response = if already_consumed {
        json_response(
            400,
            "Bad Request",
            &serde_json::json!({
                "error": "invalid_grant",
                "error_description": "refresh token already redeemed"
            }),
        )
    } else {
        json_response(
            200,
            "OK",
            &serde_json::json!({
                "access_token": format!("access-for-{refresh}"),
                "refresh_token": format!("rotated-{refresh}"),
                "token_type": "Bearer",
                "expires_in": 3600,
            }),
        )
    };
    let _ = stream.write_all(&response).await;
    let _ = stream.flush().await;
}

/// Bind an ephemeral port and keep accepting connections (a real rotating
/// grant server keeps running; a one-shot mock like `streaming.rs`'s wouldn't
/// let both racers connect).
async fn spawn_token_server() -> (String, Counts) {
    let counts: Counts = Arc::new(Mutex::new(HashMap::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accept_counts = counts.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(handle_conn(stream, accept_counts.clone()));
        }
    });
    (format!("http://{addr}/token"), counts)
}

// ── the regression test ─────────────────────────────────────────────────────

fn expired_seed(token_endpoint: &str) -> StoredAuth {
    StoredAuth {
        client_id: "cid".into(),
        client_secret: None,
        token_endpoint: token_endpoint.to_string(),
        revocation_endpoint: None,
        resource: None,
        tokens: TokenSet {
            access_token: "stale-access".into(),
            refresh_token: Some("seed-rt".into()),
            token_type: "Bearer".into(),
            expires_at: Some(0), // already expired
            scope: None,
        },
    }
}

/// Seed the token file directly (no HTTP involved) with an already-expired
/// token, then build two independent `McpTokenStore` handles pointed at the
/// same path — the same shape two real `skutter` processes see, since
/// neither the store nor `StoredTokenSource` caches anything in memory.
///
/// A plain sync function so the `env_lock()` guard is released on return,
/// before the caller does anything `.await` — clippy (`await_holding_lock`)
/// flags a lock held across an await point, and the harness convention
/// (ADR-0180) is to scope these guards to their sync windows.
fn seeded_stores(
    dir: &std::path::Path,
    token_endpoint: &str,
) -> (Box<dyn TokenStore>, Box<dyn TokenStore>) {
    let _g = crate::env_lock();
    let path = dir.join("mcp-tokens.yml");
    std::env::set_var(TOKENS_FILE_ENV, &path);

    let seed_store = McpTokenStore::load();
    seed_store
        .save("srv", &expired_seed(token_endpoint))
        .expect("seed the token file");
    let store_a: Box<dyn TokenStore> = Box::new(McpTokenStore::load());
    let store_b: Box<dyn TokenStore> = Box::new(McpTokenStore::load());
    std::env::remove_var(TOKENS_FILE_ENV);
    (store_a, store_b)
}

#[tokio::test]
async fn losing_racer_gets_the_winners_token_instead_of_an_invalid_grant_error() {
    let (token_endpoint, counts) = spawn_token_server().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let (store_a, store_b) = seeded_stores(dir.path(), &token_endpoint);
    let store_a: Arc<dyn TokenStore> = Arc::from(store_a);
    let store_b: Arc<dyn TokenStore> = Arc::from(store_b);

    let source_a = StoredTokenSource::new("srv", store_a);
    let source_b = StoredTokenSource::new("srv", store_b);

    // Both "processes" hit the expired token at once and race to refresh it.
    let (a, b) = tokio::join!(source_a.access_token(false), source_b.access_token(false));
    let a = a.expect("racer a must not fail");
    let b = b.expect("racer b must not fail");

    assert_eq!(
        a, b,
        "both racers must end up with the same (the winner's) access token — \
         a loser that got its own now-invalid exchange result, or surfaced the \
         400, would break this"
    );
    assert_eq!(a, "access-for-seed-rt");

    // The mock server's `invalid_grant` path never won: `seed-rt` was
    // redeemed exactly once — the loser found the winner's refreshed token
    // already on disk (via `TokenStore::with_exclusive`) and made no exchange
    // call of its own.
    let counts = counts.lock().unwrap();
    assert_eq!(
        counts.get("seed-rt").copied(),
        Some(1),
        "the rotating refresh token must be redeemed exactly once, not twice"
    );
}
