//! End-to-end streaming tests for the OpenAI-compatible client
//! ([`entanglement_provider::OpenAiLlm`]) driven against a hand-rolled local
//! mock HTTP server (no `mockito`/`wiremock` dependency — just a
//! `tokio::net::TcpListener` writing a raw HTTP/1.1 + SSE response).
//!
//! Covers the full path the unit tests in `src/openai.rs` can't: HTTP POST →
//! SSE frame parse → [`LlmEvent`] assembly, over the real `reqwest` transport.

use entanglement_provider::{HttpClient, OpenAiLlm, RetryConfig};
use entanglement_provider::{Llm, LlmEvent, LlmRequest, Message};
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
        None,
        None,
        HttpClient::new(),
    );
    let messages = vec![Message::user("hello")];
    let req = LlmRequest {
        system: "be helpful",
        model: None,
        messages: &messages,
        tools: &[],
        generation: None,
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
            LlmEvent::Finish { stop_reason, usage } => {
                Some((usage.input_tokens, usage.output_tokens, *stop_reason))
            }
            _ => None,
        })
        .expect("a Finish event");
    assert_eq!(
        finish,
        (
            Some(11),
            Some(3),
            Some(entanglement_provider::StopReason::EndTurn)
        )
    );
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

// ── rate-limit / retry path (per endpoint, #217) ────────────────────────────
//
// Since #217 the retry loop classifies the *response* status, not just
// `reqwest::Error` — a 429/5xx response now drives a real retry per endpoint,
// honoring `Retry-After`. The 429-surfacing tests below therefore use a
// `no_retry` client so a single mock response is enough to assert the surfaced
// error; `retryable_500_then_success_retries` drives the retry path with a
// two-response mock (500 then a good SSE stream).

/// Bind an ephemeral port and serve exactly one raw HTTP response, then close.
async fn serve_raw_once(response: Vec<u8>) -> String {
    serve_raw_seq(vec![response]).await
}

/// Bind an ephemeral port and serve `responses` in order, one per accepted
/// connection (reqwest opens a fresh connection per attempt with `Connection:
/// close`), then stop. Lets a test drive the retry loop deterministically.
async fn serve_raw_seq(responses: Vec<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        for response in responses {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let _ = read_http_request(&mut stream).await;
            let _ = stream.write_all(&response).await;
            let _ = stream.flush().await;
        }
    });
    format!("http://{addr}")
}

/// Start a turn and return the setup error string (the test expects `stream()`
/// itself to fail, before any stream item). Uses a no-retry client so a single
/// failing response is surfaced immediately.
async fn stream_err(base_url: &str) -> String {
    let mut llm = OpenAiLlm::new(
        base_url,
        Some("k".into()),
        "glm-5.2",
        None,
        None,
        HttpClient::with_config(RetryConfig::no_retry()),
    );
    let messages = vec![Message::user("hi")];
    let req = LlmRequest {
        system: "s",
        model: None,
        messages: &messages,
        tools: &[],
        generation: None,
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

#[tokio::test]
async fn retryable_500_then_success_retries_and_streams() {
    // #193/#217: a 500 *response* (not a reqwest::Error) is now classified inside
    // the retry loop and retried per endpoint. The first connection gets a 500,
    // the retry gets a clean SSE stream.
    let err500 = b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":\"boom\"}".to_vec();
    let ok = sse_response(&sse_body(&[
        r#"{"choices":[{"delta":{"content":"recovered"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ]));
    let base_url = serve_raw_seq(vec![err500, ok]).await;

    // Default retry config (5 attempts, 200ms initial backoff) retries the 500.
    let events = collect_events(&base_url).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "recovered",
        "the retry should surface the good stream"
    );
}
