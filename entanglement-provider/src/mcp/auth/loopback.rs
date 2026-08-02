//! The loopback redirect catcher for the authorization-code flow (ADR-0153).
//!
//! OAuth for a native application ends with the authorization server redirecting
//! the user's browser to a URI the app controls. For a CLI that means binding a
//! listener on `127.0.0.1` and reading the `?code=…&state=…` off the one request
//! the browser makes (RFC 8252 §7.3, "loopback interface redirection").
//!
//! Hand-rolled over `tokio::net::TcpListener` rather than pulling `axum`/`hyper`
//! into this crate: the entire server is one request, one response, no routing.
//! It binds `127.0.0.1` explicitly — never `0.0.0.0` — so nothing off-host can
//! reach it, and it is dropped the moment the code arrives.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Cap on the request bytes we read before giving up on a client — a browser's
/// `GET` with headers is well under this; anything larger is not our redirect.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// Bind a loopback listener. `port` of `None` (or `Some(0)`) takes an ephemeral
/// port, which is then readable via [`TcpListener::local_addr`].
pub async fn bind(port: Option<u16>) -> Result<TcpListener> {
    let port = port.unwrap_or(0);
    TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .with_context(|| format!("binding the OAuth redirect listener on 127.0.0.1:{port}"))
}

/// Accept connections until one carries an authorization response whose `state`
/// matches `expected_state`, and return its `code`.
///
/// Requests that are not the redirect (a browser's `/favicon.ico`, a probe) get
/// a `404` and are ignored. A response carrying `error=` fails the flow with the
/// server's own message. A mismatched `state` is refused outright — that is the
/// CSRF check, and treating it as "keep waiting" would defeat it.
pub async fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _peer) = listener
            .accept()
            .await
            .context("accepting the OAuth redirect connection")?;
        // A stray connection reset/probe reading the request head must not
        // abort the whole flow (#556) — only a `state` mismatch or an
        // explicit `error=` from the authorization server does that, below.
        let request = match read_request_head(&mut stream).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("OAuth loopback: ignoring a failed request read: {e:#}");
                continue;
            }
        };
        let Some(target) = request_target(&request) else {
            respond(&mut stream, 400, "Bad Request").await;
            continue;
        };
        let params = parse_query(&target);
        if let Some(error) = params.get("error") {
            let description = params
                .get("error_description")
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            respond(
                &mut stream,
                400,
                "Authorization failed. You can close this tab.",
            )
            .await;
            bail!("authorization server returned `{error}`{description}");
        }
        let Some(code) = params.get("code") else {
            // Not the redirect (favicon, preflight, a stray probe).
            respond(&mut stream, 404, "Not Found").await;
            continue;
        };
        if params.get("state").map(String::as_str) != Some(expected_state) {
            respond(&mut stream, 400, "State mismatch. You can close this tab.").await;
            bail!("authorization redirect carried an unexpected `state` — request refused");
        }
        respond(
            &mut stream,
            200,
            "Authorized. You can close this tab and return to the terminal.",
        )
        .await;
        return Ok(code.clone());
    }
}

/// Read until the end of the request head (`\r\n\r\n`) or the byte cap.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .context("reading the OAuth redirect request")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Write a minimal HTML response and close. Failures are ignored: the browser
/// having gone away does not invalidate a code we already parsed.
async fn respond(stream: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let reason = if status == 200 { "OK" } else { "Error" };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>skutter</title>\
         <body style=\"font-family:system-ui;margin:3rem\"><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// The request target (`/callback?code=…`) from the request line.
pub fn request_target(request: &str) -> Option<String> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    let _method = parts.next()?;
    Some(parts.next()?.to_string())
}

/// Parse a request target's query string into decoded key/value pairs.
pub fn parse_query(target: &str) -> HashMap<String, String> {
    let Some((_, query)) = target.split_once('?') else {
        return HashMap::new();
    };
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Some((percent_decode(k)?, percent_decode(v)?))
        })
        .collect()
}

/// Percent-decode a query component, treating `+` as a space. Returns `None` on
/// a malformed escape rather than silently mangling a code.
pub fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let hex = input.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encode a value for a query string, leaving only the RFC 3986
/// unreserved set intact.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_request_target() {
        let req = "GET /callback?code=abc HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert_eq!(request_target(req).unwrap(), "/callback?code=abc");
        assert!(request_target("").is_none());
    }

    #[test]
    fn parses_and_decodes_the_query() {
        let q = parse_query("/callback?code=a%2Fb&state=x%20y&empty=");
        assert_eq!(q.get("code").unwrap(), "a/b");
        assert_eq!(q.get("state").unwrap(), "x y");
        assert_eq!(q.get("empty").unwrap(), "");
        // No query at all.
        assert!(parse_query("/callback").is_empty());
    }

    #[test]
    fn plus_decodes_to_space_and_bad_escapes_are_rejected() {
        assert_eq!(percent_decode("a+b").unwrap(), "a b");
        assert!(percent_decode("%ZZ").is_none());
        assert!(percent_decode("%A").is_none());
    }

    #[test]
    fn encode_preserves_only_the_unreserved_set() {
        assert_eq!(percent_encode("aZ0-._~"), "aZ0-._~");
        assert_eq!(percent_encode("a b/c?d=e&f"), "a%20b%2Fc%3Fd%3De%26f");
    }

    #[test]
    fn encode_decode_round_trips() {
        for s in ["simple", "with space", "sl/ash", "uni¢ode", "a=b&c"] {
            assert_eq!(percent_decode(&percent_encode(s)).unwrap(), s);
        }
    }

    #[tokio::test]
    async fn bind_takes_an_ephemeral_loopback_port() {
        let listener = bind(None).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback());
        assert_ne!(addr.port(), 0);
    }

    #[tokio::test]
    async fn wait_for_code_returns_the_code_on_a_matching_state() {
        let listener = bind(None).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move {
            let l = listener;
            wait_for_code(&l, "st4te").await
        });
        // A stray request first — must be ignored, not fail the flow.
        drive_request(port, "GET /favicon.ico HTTP/1.1\r\n\r\n").await;
        drive_request(
            port,
            "GET /callback?code=the-code&state=st4te HTTP/1.1\r\n\r\n",
        )
        .await;
        assert_eq!(waiter.await.unwrap().unwrap(), "the-code");
    }

    #[tokio::test]
    async fn wait_for_code_refuses_a_state_mismatch() {
        let listener = bind(None).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move {
            let l = listener;
            wait_for_code(&l, "expected").await
        });
        drive_request(port, "GET /callback?code=c&state=forged HTTP/1.1\r\n\r\n").await;
        let err = waiter.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("unexpected `state`"));
    }

    #[tokio::test]
    async fn wait_for_code_surfaces_the_servers_error() {
        let listener = bind(None).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move {
            let l = listener;
            wait_for_code(&l, "st").await
        });
        drive_request(
            port,
            "GET /callback?error=access_denied&error_description=nope HTTP/1.1\r\n\r\n",
        )
        .await;
        let err = waiter.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("access_denied"));
        assert!(err.to_string().contains("nope"));
    }

    /// Send one raw request to the listener and read the response so the
    /// server-side write completes before the connection drops.
    async fn drive_request(port: u16, raw: &str) {
        use tokio::net::TcpStream;
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        s.write_all(raw.as_bytes()).await.unwrap();
        s.flush().await.unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
    }
}
