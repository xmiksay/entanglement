//! Provider-free stub backends: [`DummyLlm`] (a fixed reply) and
//! [`EchoLlm`] (a summary of the request it received).

use async_trait::async_trait;
use futures::stream;
use futures::StreamExt;

use super::{Llm, LlmEvent, LlmRequest, LlmStream, StopReason, Usage};

/// Deterministic stub backend. Emits a configured reply as a single text chunk
/// then `Finish` — ideal for bootstrap wiring and unit tests.
pub struct DummyLlm {
    reply: String,
}

impl DummyLlm {
    pub fn new(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
        }
    }
}

impl Default for DummyLlm {
    fn default() -> Self {
        Self::new("(dummy) thinking...")
    }
}

#[async_trait]
impl Llm for DummyLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let events = vec![
            Ok(LlmEvent::Text(self.reply.clone())),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

/// Echo-mode stub backend. Returns a text summary of the request it received,
/// making prompt assembly observable without a real provider. The reply reports
/// the total message count, every user-text snippet, the assembled system
/// prompt (its byte length + an 8-hex SHA-256 fingerprint) and the advertised
/// tool names — so a test (or a human in the TUI) can verify at a glance which
/// prompt/tool set actually reached the backend, not just whether prior turns
/// survived. Set `ENTANGLEMENT_ECHO_FULL=1` to append the full system text.
pub struct EchoLlm;

impl EchoLlm {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EchoLlm {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Llm for EchoLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let reply = echo_reply(
            &req,
            std::env::var("ENTANGLEMENT_ECHO_FULL").as_deref() == Ok("1"),
        );
        let events = vec![
            Ok(LlmEvent::Text(reply)),
            Ok(LlmEvent::Finish {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

/// Render the [`EchoLlm`] summary line for a request: message count, user-text
/// snippets, the system-prompt length + fingerprint, and the advertised tool
/// names. With `full`, the full system text is appended on its own line.
fn echo_reply(req: &LlmRequest<'_>, full: bool) -> String {
    let total = req.messages.len();
    let users: Vec<String> = req
        .messages
        .iter()
        .filter(|m| m.role == crate::MessageRole::User)
        .map(|m| m.text())
        .collect();
    let tools: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
    let mut reply = format!(
        "echo: messages={total}, users={users:?}, system_len={}, system_sha={}, tools={tools:?}, \
         trailing_notice={:?}",
        req.system.len(),
        sha8(req.system),
        req.trailing_notice,
    );
    if full {
        reply.push_str("\nsystem:\n");
        reply.push_str(req.system);
    }
    reply
}

/// First 8 hex chars of the SHA-256 of `s` — a short, stable fingerprint of the
/// system prompt so callers can diff assemblies without echoing the full text.
fn sha8(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(8);
    for byte in &digest[..4] {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolSpec;
    use crate::Message;

    fn req<'a>(system: &'a str, messages: &'a [Message], tools: &'a [ToolSpec]) -> LlmRequest<'a> {
        LlmRequest {
            system,
            model: None,
            messages,
            tools,
            generation: None,
            cache_key: None,
            retry: None,
            trailing_notice: None,
        }
    }

    #[test]
    fn sha8_is_eight_lowercase_hex() {
        let h = sha8("hello");
        assert_eq!(h.len(), 8);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // SHA-256("hello") starts with 2cf24dba…
        assert_eq!(h, "2cf24dba");
    }

    #[test]
    fn echo_reply_reports_system_and_tools() {
        let messages = [Message::user("hi"), Message::assistant("prev", vec![])];
        let tools = [
            ToolSpec::new("read", "read a file"),
            ToolSpec::new("bash", "run"),
        ];
        let out = echo_reply(&req("you are a bot", &messages, &tools), false);

        assert!(out.contains("messages=2"), "{out}");
        assert!(out.contains("users=[\"hi\"]"), "{out}");
        assert!(out.contains("system_len=13"), "{out}");
        assert!(out.contains("system_sha="), "{out}");
        assert!(out.contains("tools=[\"read\", \"bash\"]"), "{out}");
        // The full system text stays hidden unless explicitly requested.
        assert!(!out.contains("\nsystem:\n"), "{out}");
    }

    #[test]
    fn echo_reply_full_appends_system_text() {
        let out = echo_reply(&req("SECRET-PROMPT", &[], &[]), true);
        assert!(out.contains("system_len=13"), "{out}");
        assert!(out.ends_with("\nsystem:\nSECRET-PROMPT"), "{out}");
    }
}
