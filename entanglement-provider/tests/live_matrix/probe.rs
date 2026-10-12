//! Raw z.ai Chat Completions client for the matrix. The probe deliberately
//! bypasses the provider's own client: each cell controls the exact `tools`
//! array and message history it sends, and that array *is* the variable
//! under test.

use std::time::Duration;

use serde_json::{json, Value};

pub const DEFAULT_BASE: &str = "https://api.z.ai/api/coding/paas/v4";

pub struct Probe {
    http: reqwest::Client,
    url: String,
    key: String,
}

impl Probe {
    /// `None` when `ZAI_API_KEY` is unset — the probe is opt-in.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("ZAI_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())?;
        let base = std::env::var("ZAI_API_BASE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        Some(Self {
            // Never a bare `Client::new()`: under `rustls-no-provider` it panics.
            http: entanglement_provider::client::client_builder()
                .build()
                .expect("reqwest client builds"),
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            key,
        })
    }

    /// One non-streaming completion. A 429/5xx is retried three times with a
    /// growing pause; any other failure comes back as text so the cell is
    /// recorded as an error instead of aborting the whole matrix.
    pub async fn chat(
        &self,
        model: &str,
        thinking: bool,
        messages: &[Value],
        tools: &Value,
        max_tokens: u32,
    ) -> Result<Value, String> {
        let body = json!({
            "model": model,
            "messages": messages,
            "tools": tools,
            "max_tokens": max_tokens,
            "stream": false,
            "thinking": { "type": if thinking { "enabled" } else { "disabled" } },
        });
        let mut last = String::new();
        for (attempt, pause) in [5u64, 15, 30, 0].into_iter().enumerate() {
            let sent = self
                .http
                .post(&self.url)
                .bearer_auth(&self.key)
                .json(&body)
                .timeout(Duration::from_secs(180))
                .send()
                .await;
            let resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    last = format!("transport: {e}");
                    tokio::time::sleep(Duration::from_secs(pause)).await;
                    continue;
                }
            };
            let status = resp.status();
            let text = resp.text().await.map_err(|e| format!("body: {e}"))?;
            if status.is_success() {
                return serde_json::from_str(&text).map_err(|e| format!("json: {e}: {text}"));
            }
            last = format!(
                "HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
            let retryable = status.as_u16() == 429 || status.is_server_error();
            if !retryable || attempt == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(pause)).await;
        }
        Err(last)
    }
}

/// `completion_tokens` of one response (0 when absent).
pub fn completion_tokens(resp: &Value) -> u64 {
    resp.pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// `cached_tokens / prompt_tokens` of one response (0 when either is absent).
pub fn cached_share(resp: &Value) -> f64 {
    let prompt = resp
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = resp
        .pointer("/usage/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if prompt == 0 {
        0.0
    } else {
        cached as f64 / prompt as f64
    }
}
