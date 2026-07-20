//! End-to-end streaming tests for the OpenAI-compatible client
//! ([`entanglement_provider::OpenAiLlm`]) driven against a hand-rolled local
//! mock HTTP server (no `mockito`/`wiremock` dependency — just a
//! `tokio::net::TcpListener` writing a raw HTTP/1.1 + SSE response).
//!
//! Covers the full path the unit tests in `src/openai.rs` can't: HTTP POST →
//! SSE frame parse → [`LlmEvent`] assembly, over the real `reqwest` transport.

use std::time::Duration;

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

/// Bind an ephemeral port and serve one SSE response split into two raw
/// writes at `split_at` (a byte offset into `body`, not necessarily a char
/// boundary), with a flush + short sleep between them so the two halves are
/// near-certain to arrive as separate `reqwest` chunks — reproducing a network
/// chunk boundary landing mid multi-byte UTF-8 character (#443).
async fn serve_sse_split(body: String, split_at: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let _ = read_http_request(&mut stream).await;
        let full = sse_response(&body);
        // `split_at` indexes into `body`; offset by the header length so it
        // still lands at the intended byte inside the SSE body.
        let header_len = full.len() - body.len();
        let cut = header_len + split_at;
        stream
            .write_all(&full[..cut])
            .await
            .expect("write first half");
        stream.flush().await.expect("flush first half");
        tokio::time::sleep(Duration::from_millis(20)).await;
        stream
            .write_all(&full[cut..])
            .await
            .expect("write second half");
        stream.flush().await.expect("flush second half");
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

#[tokio::test]
async fn tool_call_with_malformed_json_args_is_skipped_on_explicit_finish_reason() {
    // #445: the explicit `finish_reason: "tool_calls"` flush must apply the
    // same JSON-object validation as the no-finish-reason fallback — a call
    // whose streamed `arguments` never parse as a JSON object is dropped
    // instead of forwarded with malformed `input`, and with nothing valid
    // actually emitted this turn, `stop_reason` degrades (ADR-0118) instead
    // of falsely reporting a confident `ToolUse` stop.
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"not json"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let base_url = serve_sse_once(body).await;

    let events = collect_events(&base_url).await;

    assert!(
        !events.iter().any(|e| matches!(e, LlmEvent::ToolCall(_))),
        "malformed tool call must not be emitted: {events:?}"
    );
    let stop_reason = events.iter().find_map(|e| match e {
        LlmEvent::Finish { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    assert_eq!(
        stop_reason,
        Some(None),
        "no valid tool call emitted ⇒ stop_reason degrades instead of a confident ToolUse"
    );
}

// ── rate-limit / retry path (per endpoint, ADR-0050 + ADR-0111) ─────────────
//
// A 429 is "wait your turn", not a failure: the client parks the endpoint and
// retries **until it clears**, never surfacing it as an error (ADR-0111). These
// tests drive a `[429, ok]` mock and assert the client rides past the 429 to the
// good stream. A high-`rpm`, tiny-429-backoff test config keeps the pacing gate
// and retry wait from dominating wall-clock. `retryable_500_then_success` drives
// the *bounded* 5xx retry path with a two-response mock (500 then a good stream).

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

/// A retry config with a tiny 429 schedule and a high RPM, so a `[429, ok]`
/// sequence retries near-instantly instead of on the production ≈5s→10min ramp.
fn fast_rate_limit_config() -> RetryConfig {
    RetryConfig {
        rpm: 60_000, // base pacing ≈1ms — pacing must not dominate the assertion
        rate_limit_initial_backoff: Duration::from_millis(10),
        rate_limit_max_backoff: Duration::from_millis(50),
        ..RetryConfig::default()
    }
}

/// Drive one turn with a custom retry `config` and collect every streamed event.
async fn collect_events_with(base_url: &str, config: RetryConfig) -> Vec<LlmEvent> {
    let mut llm = OpenAiLlm::new(
        base_url,
        Some("k".into()),
        "glm-5.2",
        None,
        None,
        None,
        HttpClient::with_config(config),
    );
    let messages = vec![Message::user("hi")];
    let req = LlmRequest {
        system: "s",
        model: None,
        messages: &messages,
        tools: &[],
        generation: None,
    };
    let stream = llm
        .stream(req)
        .await
        .expect("stream should start after retrying past the 429");
    stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|r| r.expect("no error items expected"))
        .collect()
}

fn streamed_text(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn rate_limit_429_without_retry_after_retries_until_clear() {
    // A headerless 429 must NOT surface as an error — it parks the endpoint and
    // is retried until the next response succeeds.
    let err429 = b"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":\"slow down\"}".to_vec();
    let ok = sse_response(&sse_body(&[
        r#"{"choices":[{"delta":{"content":"recovered"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ]));
    let base_url = serve_raw_seq(vec![err429, ok]).await;

    let events = collect_events_with(&base_url, fast_rate_limit_config()).await;
    assert_eq!(
        streamed_text(&events),
        "recovered",
        "a headerless 429 should be retried until the stream succeeds"
    );
}

#[tokio::test]
async fn rate_limit_429_honors_retry_after_then_succeeds() {
    // A server `Retry-After: 1` steers the wait (≈1s, overriding the tiny test
    // backoff) and the client then rides past to the good stream.
    let err429 = b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":\"rate limited\"}".to_vec();
    let ok = sse_response(&sse_body(&[
        r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ]));
    let base_url = serve_raw_seq(vec![err429, ok]).await;

    let start = std::time::Instant::now();
    let events = collect_events_with(&base_url, fast_rate_limit_config()).await;
    assert!(
        start.elapsed() >= Duration::from_millis(900),
        "the server Retry-After: 1 wait should be honored, elapsed {:?}",
        start.elapsed()
    );
    assert_eq!(streamed_text(&events), "ok");
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

// ── multi-byte UTF-8 split across a network chunk boundary (#443) ──────────

#[tokio::test]
async fn text_delta_survives_multibyte_char_split_across_chunks() {
    // "🎉" (U+1F389) is 4 bytes; split the raw response so the boundary falls
    // inside the emoji, exactly like an arbitrary TCP/HTTP-chunk boundary.
    let body = sse_body(&[r#"{"choices":[{"delta":{"content":"party 🎉 time"}}]}"#]);
    let emoji_byte_offset = body.find('🎉').expect("emoji present in body");
    let split_at = emoji_byte_offset + 2; // inside the 4-byte sequence

    let base_url = serve_sse_split(body, split_at).await;
    let events = collect_events(&base_url).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "party 🎉 time");
    assert!(
        !text.contains('\u{FFFD}'),
        "split emoji must reassemble losslessly, got {text:?}"
    );
}

#[tokio::test]
async fn tool_call_argument_survives_multibyte_char_split_across_chunks() {
    // A non-ASCII value ("Curaçao") inside the streamed `function.arguments`
    // fragment, split mid-character across the raw chunk boundary.
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\": \"Curaçao\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let c_cedilla_offset = body.find('ç').expect("ç present in body");
    let split_at = c_cedilla_offset + 1; // inside the 2-byte sequence

    let base_url = serve_sse_split(body, split_at).await;
    let events = collect_events(&base_url).await;

    let call = events
        .iter()
        .find_map(|e| match e {
            LlmEvent::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .expect("a ToolCall event, not dropped as malformed JSON");
    assert_eq!(call.input, r#"{"city": "Curaçao"}"#);
}
