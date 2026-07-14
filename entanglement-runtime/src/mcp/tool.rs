//! [`McpTool`] — the runtime-side proxy that makes one external MCP tool look
//! like any other host [`Tool`] (#198). It carries the advertised name, the
//! description, and the server's `inputSchema` so the tool flows straight into
//! `EngineConfig.tool_specs`; its [`run`][Tool::run] round-trips the call over the
//! shared [`McpClient`]. No core change is needed — an MCP tool is just another
//! entry in the [`ToolRegistry`][crate::tools::ToolRegistry], governed by the same
//! permission profiles and the same `ToolExec` round-trip as `read`/`bash`.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::{McpClient, McpToolDef};
use crate::tools::Tool;

/// A proxy for one tool on one MCP server.
pub struct McpTool {
    client: Arc<McpClient>,
    /// The advertised, collision-free name (`mcp__<server>__<tool>`), leaked to
    /// `&'static str` because the [`Tool`] trait keys the registry on it. Leaking
    /// is bounded: it happens once per tool at startup and the tool lives for the
    /// whole process.
    name: &'static str,
    /// The bare tool name the server knows it by (what `tools/call` sends).
    remote_name: String,
    description: String,
    schema: Value,
}

impl McpTool {
    /// Build a proxy for `def` on `server`. The advertised name is namespaced and
    /// sanitized so it can never collide with a host tool (`read`) or another
    /// server's tool, and stays within providers' `^[A-Za-z0-9_-]+$` tool-name rule.
    pub fn new(client: Arc<McpClient>, server: &str, def: McpToolDef) -> Self {
        let advertised = sanitize(&format!("mcp__{server}__{}", def.name));
        let name: &'static str = Box::leak(advertised.into_boxed_str());
        Self {
            client,
            name,
            remote_name: def.name,
            description: def.description,
            schema: def.input_schema,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn run(&self, input: &str) -> Result<String> {
        // The model sends the tool input as a JSON object string; MCP wants it as
        // the `arguments` object. An empty input is a no-arg call.
        let arguments: Value = if input.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(input).context("MCP tool arguments must be a JSON object")?
        };
        let result = self.client.call_tool(&self.remote_name, arguments).await?;
        Ok(render_result(&result))
    }
}

/// Flatten a `tools/call` result into text the model reads. Text blocks are
/// concatenated; non-text blocks (image/resource) are noted but not inlined
/// (v1 keeps MCP results text-only). An `isError` result is prefixed so the model
/// understands the tool reported a failure rather than a normal answer.
fn render_result(result: &Value) -> String {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut out = String::new();
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                        out.push('\n');
                    }
                }
                Some(other) => out.push_str(&format!("[{other} content omitted]\n")),
                None => {}
            }
        }
    }
    let body = out.trim_end();
    if is_error {
        format!("MCP tool reported an error: {body}")
    } else if body.is_empty() {
        "(no content)".to_string()
    } else {
        body.to_string()
    }
}

/// Replace any character outside `[A-Za-z0-9_-]` with `_` so the advertised tool
/// name satisfies the OpenAI/Anthropic tool-name constraint regardless of what a
/// server named itself or its tool.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str) -> McpToolDef {
        McpToolDef {
            name: name.to_string(),
            description: "a tool".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    // A client is needed to build an McpTool; a dead duplex is enough for the
    // pure-name/schema assertions that never call `run`.
    fn dead_client() -> Arc<McpClient> {
        let (client_end, server_end) = tokio::io::duplex(64);
        drop(server_end);
        let (r, w) = tokio::io::split(client_end);
        McpClient::new("srv".to_string(), w, r, None)
    }

    #[tokio::test]
    async fn namespaces_and_sanitizes_the_name() {
        let t = McpTool::new(dead_client(), "my server", def("read.file"));
        assert_eq!(t.name(), "mcp__my_server__read_file");
    }

    #[test]
    fn renders_text_blocks() {
        let r = json!({ "content": [ { "type": "text", "text": "hello" }, { "type": "text", "text": "world" } ] });
        assert_eq!(render_result(&r), "hello\nworld");
    }

    #[test]
    fn flags_error_results() {
        let r = json!({ "isError": true, "content": [ { "type": "text", "text": "boom" } ] });
        assert!(render_result(&r).starts_with("MCP tool reported an error:"));
    }

    #[test]
    fn notes_non_text_and_empty() {
        let img = json!({ "content": [ { "type": "image", "data": "…" } ] });
        assert_eq!(render_result(&img), "[image content omitted]");
        assert_eq!(render_result(&json!({ "content": [] })), "(no content)");
    }

    #[tokio::test]
    async fn schema_and_description_pass_through() {
        let t = McpTool::new(dead_client(), "srv", def("x"));
        assert_eq!(t.description(), "a tool");
        assert_eq!(t.schema(), json!({ "type": "object", "properties": {} }));
    }
}
