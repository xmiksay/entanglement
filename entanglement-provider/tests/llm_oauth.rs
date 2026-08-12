//! OAuth-protected LLM endpoints end-to-end (#684 edge d): each wire client
//! (OpenAI-compat, Anthropic, Gemini) authenticates with an `Authorization:
//! Bearer` token from an [`AccessTokenSource`] instead of its static key
//! header, and retries exactly once with a forced refresh on a `401`.
//! Hand-rolled `tokio::net::TcpListener` mock (no `wiremock` in this
//! workspace), capturing each request's headers so the assertions read the
//! server's view.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use entanglement_provider::oauth::AccessTokenSource;
use entanglement_provider::{
    AnthropicLlm, GeminiLlm, HttpClient, Llm, LlmRequest, OpenAiLlm, RetryConfig,
};

fn test_http_client() -> HttpClient {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        std::env::set_var("ENTANGLEMENT_NO_SHARED_ENDPOINT_STATE", "1");
    });
    // Unpaced + single-attempt ladder: the 401 is a *non-retryable* status for
    // the pool, so only the wire's own forced-refresh loop may retry it.
    HttpClient::with_config(RetryConfig {
        rpm: 1_000_000,
        ..RetryConfig::default()
    })
    .unwrap()
}

/// Serves scripted `(status, body)` responses in order (repeating the last),
/// capturing each request's raw head+body.
#[derive(Clone)]
struct MockLlm {
    captured: Arc<Mutex<Vec<String>>>,
    responses: Arc<Vec<(u16, String)>>,
    served: Arc<AtomicUsize>,
}

async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut tmp).await.expect("read request");
        if n == 0 {
            return String::from_utf8_lossy(&buf).into_owned();
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
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

async fn spawn_mock(responses: Vec<(u16, String)>) -> (String, MockLlm) {
    let mock = MockLlm {
        captured: Arc::new(Mutex::new(Vec::new())),
        responses: Arc::new(responses),
        served: Arc::new(AtomicUsize::new(0)),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accept = mock.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mock = accept.clone();
            tokio::spawn(async move {
                let raw = read_http_request(&mut stream).await;
                mock.captured.lock().unwrap().push(raw);
                let idx = mock.served.fetch_add(1, Ordering::SeqCst);
                let (status, body) = mock
                    .responses
                    .get(idx)
                    .or_else(|| mock.responses.last())
                    .cloned()
                    .unwrap_or((200, String::new()));
                let reason = if status == 200 { "OK" } else { "Unauthorized" };
                let content_type = if body.starts_with("data:") {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    (format!("http://{addr}"), mock)
}

/// Hands out `tok-1`, then `tok-N` counting up on every forced refresh.
struct CountingSource {
    fetches: AtomicUsize,
    forced: AtomicUsize,
}

impl CountingSource {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            fetches: AtomicUsize::new(0),
            forced: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl AccessTokenSource for CountingSource {
    async fn access_token(&self, force_refresh: bool) -> Result<String> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        if force_refresh {
            self.forced.fetch_add(1, Ordering::SeqCst);
        }
        Ok(format!(
            "tok-{}",
            self.forced.load(Ordering::SeqCst) + 1 // tok-1 until a refresh bumps it
        ))
    }
}

fn header_of(raw: &str, name: &str) -> Option<String> {
    raw.split("\r\n").find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
    })
}

fn sse_ok() -> (u16, String) {
    (200, "data: [DONE]\n\n".to_string())
}

fn req<'a>() -> LlmRequest<'a> {
    LlmRequest {
        system: "",
        model: None,
        messages: &[],
        tools: &[],
        generation: None,
        cache_key: None,
    }
}

// ── OpenAI-compat ────────────────────────────────────────────────────────────

#[tokio::test]
async fn openai_wire_sends_the_oauth_bearer_instead_of_the_static_key() {
    let (base, mock) = spawn_mock(vec![sse_ok()]).await;
    let source = CountingSource::new();
    let mut llm = OpenAiLlm::new(
        &base,
        None,
        "m",
        None,
        None,
        Arc::new(|_| None),
        None,
        false,
        test_http_client(),
    )
    .with_auth(source.clone());

    let _stream = llm.stream(req()).await.expect("stream");

    let captured = mock.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        header_of(&captured[0], "authorization").as_deref(),
        Some("Bearer tok-1")
    );
    assert_eq!(source.forced.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn openai_wire_retries_a_401_once_with_a_forced_refresh() {
    let (base, mock) = spawn_mock(vec![(401, "{}".to_string()), sse_ok()]).await;
    let source = CountingSource::new();
    let mut llm = OpenAiLlm::new(
        &base,
        None,
        "m",
        None,
        None,
        Arc::new(|_| None),
        None,
        false,
        test_http_client(),
    )
    .with_auth(source.clone());

    let _stream = llm.stream(req()).await.expect("stream after refresh");

    let captured = mock.captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "401 then the refreshed retry");
    assert_eq!(
        header_of(&captured[0], "authorization").as_deref(),
        Some("Bearer tok-1")
    );
    assert_eq!(
        header_of(&captured[1], "authorization").as_deref(),
        Some("Bearer tok-2"),
        "the retry must carry the force-refreshed token"
    );
    assert_eq!(source.forced.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn openai_wire_second_401_is_a_terminal_error_not_a_loop() {
    let (base, mock) = spawn_mock(vec![(401, "{}".to_string())]).await;
    let source = CountingSource::new();
    let mut llm = OpenAiLlm::new(
        &base,
        None,
        "m",
        None,
        None,
        Arc::new(|_| None),
        None,
        false,
        test_http_client(),
    )
    .with_auth(source.clone());

    let err = llm.stream(req()).await.err().expect("still unauthorized");
    assert!(err.to_string().contains("401"), "{err:#}");
    assert_eq!(mock.captured.lock().unwrap().len(), 2, "exactly one retry");
}

// ── Anthropic ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn anthropic_wire_replaces_x_api_key_with_the_bearer() {
    // `event: message_stop` terminates the SSE read; an empty body would too
    // (connection close), so keep it minimal.
    let (base, mock) = spawn_mock(vec![(
        200,
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
    )])
    .await;
    let source = CountingSource::new();
    let mut llm = AnthropicLlm::new(
        &base,
        "", // no static key for a purely OAuth endpoint
        "m",
        None,
        None,
        Arc::new(|_| None),
        None,
        None,
        Default::default(),
        true,
        test_http_client(),
    )
    .with_auth(source.clone());

    let mut stream = llm.stream(req()).await.expect("stream");
    // Anthropic issues the request lazily inside the stream body — drive it.
    while stream.next().await.is_some() {}

    let captured = mock.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        header_of(&captured[0], "authorization").as_deref(),
        Some("Bearer tok-1")
    );
    assert_eq!(header_of(&captured[0], "x-api-key"), None);
    assert!(header_of(&captured[0], "anthropic-version").is_some());
}

#[tokio::test]
async fn anthropic_wire_retries_a_401_once_with_a_forced_refresh() {
    let (base, mock) = spawn_mock(vec![
        (401, "{}".to_string()),
        (
            200,
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
        ),
    ])
    .await;
    let source = CountingSource::new();
    let mut llm = AnthropicLlm::new(
        &base,
        "",
        "m",
        None,
        None,
        Arc::new(|_| None),
        None,
        None,
        Default::default(),
        true,
        test_http_client(),
    )
    .with_auth(source.clone());

    let mut stream = llm.stream(req()).await.expect("stream");
    let mut errored = false;
    while let Some(ev) = stream.next().await {
        errored |= ev.is_err();
    }
    assert!(!errored, "the refreshed retry must succeed");

    let captured = mock.captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        header_of(&captured[1], "authorization").as_deref(),
        Some("Bearer tok-2")
    );
    assert_eq!(source.forced.load(Ordering::SeqCst), 1);
}

// ── Gemini ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn gemini_wire_replaces_x_goog_api_key_with_the_bearer() {
    let (base, mock) = spawn_mock(vec![sse_ok()]).await;
    let source = CountingSource::new();
    let mut llm = GeminiLlm::new(
        &base,
        "",
        "m",
        None,
        None,
        Arc::new(|_| None),
        test_http_client(),
    )
    .with_auth(source.clone());

    let _stream = llm.stream(req()).await.expect("stream");

    let captured = mock.captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        header_of(&captured[0], "authorization").as_deref(),
        Some("Bearer tok-1")
    );
    assert_eq!(header_of(&captured[0], "x-goog-api-key"), None);
}

#[tokio::test]
async fn gemini_wire_retries_a_401_once_with_a_forced_refresh() {
    let (base, mock) = spawn_mock(vec![(401, "{}".to_string()), sse_ok()]).await;
    let source = CountingSource::new();
    let mut llm = GeminiLlm::new(
        &base,
        "",
        "m",
        None,
        None,
        Arc::new(|_| None),
        test_http_client(),
    )
    .with_auth(source.clone());

    let _stream = llm.stream(req()).await.expect("stream after refresh");

    let captured = mock.captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(
        header_of(&captured[1], "authorization").as_deref(),
        Some("Bearer tok-2")
    );
    assert_eq!(source.forced.load(Ordering::SeqCst), 1);
}
