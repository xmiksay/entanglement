//! The host-tool vocabulary: the [`Tool`] trait every concrete host tool
//! implements and the [`ToolRegistry`] that owns them. Both live in the
//! **runtime** — core holds no executable tools, only advertises tool *schemas*
//! and round-trips each call back here (#58/#59, #206, ADR-0006/0010/0053). The
//! concrete tools (`read`/`glob`/`grep`/`edit`/`write`/`bash`/`call`/`rhai`, …)
//! live in [`crate::host`] and [`crate::script`]; [`crate::tool_runner`] resolves
//! permission and executes the cleared call against a registry.
//!
//! [`ToolSpec`]/[`ToolCall`] are the LLM ABI DTOs; they ride in
//! `entanglement-provider` (carried by `LlmRequest`/`LlmResponse`) and core
//! re-exports them, so the runtime pulls them from `entanglement_core` — keeping
//! the lean library free of a direct provider dependency (ADR-0025/0053).

use async_trait::async_trait;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use entanglement_core::{ContentPart, ToolCall, ToolSpec};

/// A single capability the engine can execute on the host.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's registry key and advertised name. Built-in tools return a
    /// `Cow::Borrowed` static literal (allocation-free); dynamically-named tools
    /// like [`McpTool`][crate::mcp::McpTool] return an owned `Cow::Owned` — no
    /// `Box::leak` needed (#314).
    fn name(&self) -> Cow<'static, str>;

    /// Short description surfaced to the model. Default empty so simple tools
    /// don't have to bother.
    fn description(&self) -> &str {
        ""
    }

    /// JSON Schema for the tool's input object (surfaces as Anthropic's
    /// `input_schema` / OpenAI's `parameters`). Default is a permissive
    /// empty-object schema; structured tools override it so the model knows the
    /// exact arguments to send.
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn run(&self, input: &str) -> anyhow::Result<String>;

    /// Multimodal execution — what the tool executor actually calls. The default
    /// wraps [`run`][Tool::run]'s text in a single text part (empty text → no
    /// parts, matching a text-only tool result). `read` overrides it to emit an
    /// image content block when it opens an image file (#221); every other tool
    /// keeps the plain-text `run`.
    async fn run_content(&self, input: &str) -> anyhow::Result<Vec<ContentPart>> {
        let text = self.run(input).await?;
        Ok(text_parts(text))
    }
}

/// One text part for a non-empty string, none for an empty one — the same fold
/// [`entanglement_core::Message::tool`] applies, so a text-only result keeps its
/// exact prior shape.
pub(crate) fn text_parts(text: String) -> Vec<ContentPart> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(text)]
    }
}

/// Named lookup of tools. Cloning is cheap (tools are shared behind `Arc`), so
/// one registry built in config is cloned into every session.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<Cow<'static, str>, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.name();
        self.tools.insert(name, Arc::new(tool));
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Specs advertised to the model (for the `tools` field of an LLM request).
    /// Each carries the tool's [`Tool::schema`] so the model sees the real
    /// `input_schema`, not an empty object.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|t| ToolSpec::with_schema(t.name(), t.description(), t.schema()))
            .collect()
    }

    /// Execute a model-requested [`ToolCall`], returning the result as multimodal
    /// [`ContentPart`]s (#221) — text for most tools, an image block for `read` on
    /// an image. Unknown tools and failures yield a text part the engine feeds
    /// back to the model rather than erroring the run.
    pub async fn execute(&self, call: &ToolCall) -> Vec<ContentPart> {
        match self.tools.get(call.name.as_str()) {
            Some(tool) => match tool.run_content(&call.input).await {
                Ok(content) => content,
                Err(e) => text_parts(format!("tool `{}` failed: {e}", call.name)),
            },
            None => text_parts(format!("unknown tool: `{}`", call.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("echo")
        }
        fn description(&self) -> &str {
            "echo its input"
        }
        async fn run(&self, input: &str) -> anyhow::Result<String> {
            Ok(input.to_string())
        }
    }

    #[tokio::test]
    async fn runs_registered_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let out = reg
            .execute(&ToolCall {
                id: "1".into(),
                name: "echo".into(),
                input: "hi".into(),
            })
            .await;
        assert_eq!(out, vec![ContentPart::text("hi")]);
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_not_fatal() {
        let reg = ToolRegistry::new();
        let out = reg
            .execute(&ToolCall {
                id: "1".into(),
                name: "nope".into(),
                input: "".into(),
            })
            .await;
        assert_eq!(out.len(), 1);
        assert!(out[0].as_text().unwrap().contains("unknown tool"));
    }

    #[test]
    fn specs_advertise_name_description_and_schema() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let specs = reg.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert_eq!(specs[0].description, "echo its input");
        // Default schema is a permissive empty object.
        assert_eq!(
            specs[0].schema,
            serde_json::json!({"type":"object","properties":{}})
        );
    }
}
