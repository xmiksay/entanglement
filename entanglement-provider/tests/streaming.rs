//! End-to-end streaming tests for the OpenAI-compatible client
//! ([`entanglement_provider::OpenAiLlm`]) driven against a hand-rolled local
//! mock HTTP server (no `mockito`/`wiremock` dependency — just a
//! `tokio::net::TcpListener` writing a raw HTTP/1.1 + SSE response).
//!
//! Covers the full path the unit tests in `src/openai.rs` can't: HTTP POST →
//! SSE frame parse → [`LlmEvent`] assembly, over the real `reqwest` transport.

use entanglement_core::{Llm, LlmEvent, LlmRequest, Message};
use entanglement_provider::{HttpClient, OpenAiLlm};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ── mock server ─────────────────────────────────────────────────────────────

/// Read one HTTP request off `stream` (headers + any `Content-Length` body) so
/// the client finishes sending before we reply — otherwise a premature close
/// can surface as a write error on the client side.
async fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // Read until the end of headers.
    let header_end = loop {
        let n = stream.read(&mut tmp).await.expect("read request");
        if n == 0 {
            return buf; // peer closed
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    // Honor Content-Length so the whole POST body is drained.
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

/// Wrap SSE `data:` frames (already `\n\n`-terminated by the caller) in a raw
/// HTTP/1.1 200 response.
fn sse_response(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
        .into_bytes()
}

/// Bind an ephemeral port and serve exactly one SSE response, then close.
/// Returns the base URL to hand to [`OpenAiLlm::new`].
async fn serve_sse_once(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut stream).await;
        stream
            .write_all(&sse_response(&body))
            .await
            .expect("write response");
        stream.flush().await.expect("flush");
    });
    format!("http://{addr}")
}

/// Assemble an SSE body from JSON chunk strings, appending the terminal
/// `[DONE]` sentinel.
fn sse_body(chunks: &[&str]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str("data: ");
        out.push_str(c);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

// ── request/collection helpers ──────────────────────────────────────────────

/// Drive one full turn against `base_url` and collect every streamed event
/// (failing on the first error item).
async fn collect_events(base_url: &str) -> Vec<LlmEvent> {
    let mut llm = OpenAiLlm::new(
        base_url,
        Some("test-key".into()),
        "glm-5.2",
        HttpClient::new(),
    );
    let messages = vec![Message::user("hello")];
    let req = LlmRequest {
        system: "be helpful",
        model: None,
        messages: &messages,
        tools: &[],
    };
    let stream = llm.stream(req).await.expect("stream should start");
    stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|r| r.expect("no error items expected"))
        .collect()
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_streams_text_deltas_and_finish_with_usage() {
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
        r#"{"choices":[{"delta":{"content":"lo, world"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}"#,
    ]);
    let base_url = serve_sse_once(body).await;

    let events = collect_events(&base_url).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world");

    let finish = events
        .iter()
        .find_map(|e| match e {
            LlmEvent::Finish {
                input_tokens,
                output_tokens,
            } => Some((*input_tokens, *output_tokens)),
            _ => None,
        })
        .expect("a Finish event");
    assert_eq!(finish, (Some(11), Some(3)));
}

#[tokio::test]
async fn tool_call_stream_assembles_and_emits_tool_call() {
    // `id` + name on the first delta, arguments streamed across two more, then
    // `finish_reason: tool_calls` flushes the assembled call.
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Prague\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let base_url = serve_sse_once(body).await;

    let events = collect_events(&base_url).await;

    let call = events
        .iter()
        .find_map(|e| match e {
            LlmEvent::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .expect("a ToolCall event");
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "get_weather");
    assert_eq!(call.input, r#"{"city":"Prague"}"#);

    assert!(
        events.iter().any(|e| matches!(e, LlmEvent::Finish { .. })),
        "stream must still terminate with Finish"
    );
}

// ── rate-limit / error surfacing path ───────────────────────────────────────
//
// NOTE on why the transient-retry path is NOT driven here:
//
// `HttpClient::execute_with_retry` retries only genuine `reqwest::Error`s that
// `is_transient_error` accepts — `is_timeout()`, `is_connect()`, a status-bearing
// 5xx/429, or an error string containing `"incomplete"`. But the client calls
// plain `.send()` (no `error_for_status`), so reqwest returns `Ok` for *any* HTTP
// status: a 5xx or 429 *response* is never a `reqwest::Error` and so is never
// retried — it's handled after the fact by the `!status().is_success()` branch in
// `openai.rs`. The only remaining retry triggers are real transport faults, and
// those can't be produced deterministically from a local mock: an empirical probe
// (accept the first connection, drop it without responding) showed reqwest yields
// a plain "error sending request" that `is_transient_error` classifies as
// **permanent** — the client makes exactly one connection and does not retry.
// Driving a true retry would require injecting a real connect/timeout fault at the
// socket layer, out of scope for an HTTP mock. So instead we assert the reachable
// end-to-end behavior: the client surfaces a 429 as a clear error.

/// Bind an ephemeral port and serve exactly one raw HTTP response, then close.
async fn serve_raw_once(response: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut stream).await;
        stream.write_all(&response).await.expect("write response");
        stream.flush().await.expect("flush");
    });
    format!("http://{addr}")
}

/// Start a turn and return the setup error string (the test expects `stream()`
/// itself to fail, before any stream item).
async fn stream_err(base_url: &str) -> String {
    let mut llm = OpenAiLlm::new(base_url, Some("k".into()), "glm-5.2", HttpClient::new());
    let messages = vec![Message::user("hi")];
    let req = LlmRequest {
        system: "s",
        model: None,
        messages: &messages,
        tools: &[],
    };
    match llm.stream(req).await {
        Ok(_) => panic!("expected stream() to fail on a 429 response"),
        Err(e) => format!("{e:#}"),
    }
}

#[tokio::test]
async fn rate_limit_429_without_retry_after_surfaces_http_error() {
    let response = b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":\"slow down\"}".to_vec();
    let base_url = serve_raw_once(response).await;

    let err = stream_err(&base_url).await;
    assert!(
        err.contains("429"),
        "error should surface the 429 status: {err}"
    );
    assert!(
        err.contains("slow down"),
        "error should surface the server body: {err}"
    );
}

#[tokio::test]
async fn rate_limit_429_with_retry_after_reports_backoff() {
    // The `Retry-After` header steers `openai.rs` into its dedicated
    // rate-limited branch rather than the generic HTTP-status bail.
    let response = b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":\"rate limited\"}".to_vec();
    let base_url = serve_raw_once(response).await;

    let err = stream_err(&base_url).await;
    assert!(
        err.contains("rate limited"),
        "error should report rate limiting: {err}"
    );
    assert!(
        err.contains('7'),
        "error should carry the Retry-After duration: {err}"
    );
}
