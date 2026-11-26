//! Each wire sends its catalog's static key in the header that wire expects
//! (#917 left the static key as the only LLM credential): a regression here is
//! a 401 on every request, so it is pinned against a local mock per wire.

use std::sync::{Arc, Mutex as StdMutex};

use entanglement_provider::{
    fixed_model_concurrency, fixed_thinking_spec, AnthropicLlm, AnthropicModelSpec, GeminiLlm,
    HttpClient, Llm, LlmRequest, Message, OpenAiLlm, OpenAiResponsesLlm, RetryConfig,
    SharedStateLocation, ThinkingSpec,
};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const KEY: &str = "static-test-key";

fn http() -> HttpClient {
    HttpClient::with_shared_state(SharedStateLocation::Disabled, RetryConfig::default())
        .expect("http client")
}

/// Serve every connection a non-retryable 400, recording each request's
/// lowercased header block — only the headers matter here.
async fn capture_server() -> (String, Arc<StdMutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
            let text = String::from_utf8_lossy(&buf).to_ascii_lowercase();
            let head = text
                .split("\r\n\r\n")
                .next()
                .unwrap_or_default()
                .to_string();
            sink.lock().unwrap().push(head);
            let body = r#"{"error":{"message":"bad request"}}"#;
            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    (format!("http://{addr}"), seen)
}

/// Run one turn to completion (the 400 makes it an error — irrelevant here).
async fn drive(mut llm: impl Llm) {
    let messages = vec![Message::user("hello")];
    let req = LlmRequest {
        system: "be helpful",
        model: None,
        messages: &messages,
        tools: &[],
        generation: None,
        cache_key: None,
        trailing_notice: None,
        retry: None,
    };
    if let Ok(stream) = llm.stream(req).await {
        let _ = stream.collect::<Vec<_>>().await;
    }
}

fn assert_header(seen: &Arc<StdMutex<Vec<String>>>, path: &str, header: &str) {
    let seen = seen.lock().unwrap();
    let request = seen
        .iter()
        .find(|h| {
            h.lines()
                .next()
                .is_some_and(|l| l.contains(&path.to_ascii_lowercase()))
        })
        .unwrap_or_else(|| panic!("no request to {path}; saw {seen:?}"));
    assert!(
        request.lines().any(|l| l.trim() == header),
        "`{header}` missing from:\n{request}"
    );
}

#[tokio::test]
async fn openai_compat_sends_the_static_key_as_bearer() {
    let (base, seen) = capture_server().await;
    drive(OpenAiLlm::new(
        &base,
        Some(KEY.into()),
        "m",
        None,
        None,
        fixed_model_concurrency(None),
        None,
        false,
        fixed_thinking_spec(ThinkingSpec::default()),
        http(),
    ))
    .await;
    assert_header(
        &seen,
        "/chat/completions",
        &format!("authorization: bearer {KEY}"),
    );
}

#[tokio::test]
async fn responses_sends_the_static_key_as_bearer() {
    let (base, seen) = capture_server().await;
    drive(OpenAiResponsesLlm::new(
        &base,
        Some(KEY.into()),
        "m",
        None,
        None,
        fixed_model_concurrency(None),
        fixed_thinking_spec(ThinkingSpec::default()),
        http(),
    ))
    .await;
    assert_header(&seen, "/responses", &format!("authorization: bearer {KEY}"));
}

#[tokio::test]
async fn anthropic_sends_the_static_key_as_x_api_key() {
    let (base, seen) = capture_server().await;
    drive(AnthropicLlm::new(
        &base,
        KEY,
        "m",
        None,
        None,
        fixed_model_concurrency(None),
        None,
        None,
        AnthropicModelSpec::default(),
        http(),
    ))
    .await;
    assert_header(&seen, "/v1/messages", &format!("x-api-key: {KEY}"));
}

#[tokio::test]
async fn gemini_sends_the_static_key_as_x_goog_api_key() {
    let (base, seen) = capture_server().await;
    drive(GeminiLlm::new(
        &base,
        KEY,
        "m",
        None,
        None,
        fixed_model_concurrency(None),
        http(),
    ))
    .await;
    assert_header(
        &seen,
        ":streamGenerateContent",
        &format!("x-goog-api-key: {KEY}"),
    );
}
