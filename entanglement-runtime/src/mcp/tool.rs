//! [`McpTool`] — the runtime-side proxy that makes one external MCP tool look
//! like any other host [`Tool`] (#198). It carries the advertised name, the
//! description, and the server's `inputSchema` so the tool flows straight into
//! `EngineConfig.tool_specs`; its [`run`][Tool::run] round-trips the call over the
//! shared [`McpClient`]. No core change is needed — an MCP tool is just another
//! entry in the [`ToolRegistry`][crate::tools::ToolRegistry], governed by the same
//! permission profiles and the same `ToolExec` round-trip as `read`/`bash`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::{McpClient, McpToolDef};
use crate::capability::Capability;
use crate::host::truncate_output;
use crate::tools::Tool;

/// A proxy for one tool on one MCP server.
pub struct McpTool {
    client: Arc<McpClient>,
    /// The advertised, collision-free name (`mcp__<server>__<tool>`). Owned, so a
    /// multi-tenant embedder rebuilding registries never leaks a string per rename
    /// (#314); [`Tool::name`] hands it back as `Cow::Owned`.
    name: String,
    /// The bare tool name the server knows it by (what `tools/call` sends).
    remote_name: String,
    description: String,
    schema: Value,
    /// This tool's graded capability, resolved once at construction (ADR-0207
    /// §3 grading, closing the gap ADR-0117 deferred). See
    /// [`resolve_capability`] for where it comes from.
    capabilities: &'static [Capability],
}

impl McpTool {
    /// Build a proxy for `def` on `server`. The advertised name is namespaced and
    /// sanitized so it can never collide with a host tool (`read`) or another
    /// server's tool, and stays within providers' `^[A-Za-z0-9_-]+$` tool-name rule.
    ///
    /// `capabilities` is the server's own config-side `capabilities:` map
    /// (raw, un-namespaced tool name → `read`/`write`/`call`, ADR-0117),
    /// exactly the field [`super::capability_index`] derives its index from —
    /// read directly here, at each connect site, rather than through that
    /// aggregated global index. That matters for a per-scope config
    /// (`McpScope::servers`, ADR-0188) which never joins the process-global
    /// `mcp:` map the index is built from: resolving locally means a scoped
    /// server's own annotation still grades correctly instead of silently
    /// falling through to the fail-safe default.
    pub fn new(
        client: Arc<McpClient>,
        server: &str,
        def: McpToolDef,
        capabilities: &HashMap<String, String>,
    ) -> Self {
        let name = namespaced_tool_name(server, &def.name);
        let capabilities = resolve_capability(capabilities, &def.name);
        Self {
            client,
            name,
            remote_name: def.name,
            description: def.description,
            schema: def.input_schema,
            capabilities,
        }
    }
}

/// `read`/`write`/`call` → `Capability::{Read,Write,Exec}` (ADR-0117's own
/// three-name vocabulary). Absent *or* an unrecognized string both fall back
/// to `Write` — fail-safe, since an MCP server is the one tool source the
/// runtime doesn't author: an unannotated tool must still be refused by a
/// read-only mode, never silently allowed. A malformed string can't actually
/// reach here today (`capability_index` bails on one at startup, ADR-0117),
/// but this stays defensive rather than leaning on that upstream check.
fn resolve_capability(
    capabilities: &HashMap<String, String>,
    remote_name: &str,
) -> &'static [Capability] {
    match capabilities.get(remote_name).map(String::as_str) {
        Some("read") => &[Capability::Read],
        Some("write") => &[Capability::Write],
        Some("call") => &[Capability::Exec],
        _ => &[Capability::Write],
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Owned(self.name.clone())
    }

    fn capabilities(&self) -> &'static [Capability] {
        self.capabilities
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
        // #594's required-param pre-check used to live here; it's now
        // subsumed by the uniform pre-dispatch validation in
        // `crate::arg_validate` (ADR-0196 §6), which runs against this same
        // `self.schema` (the server's `inputSchema`) before `run` is ever
        // called — so a missing/malformed argument never reaches this point.
        let result = self.client.call_tool(&self.remote_name, arguments).await?;
        // MCP servers are the one tool source the runtime doesn't author — cap
        // their results with the same 32 KiB bound every host tool honors, so a
        // chatty server can't flood the context in a single call.
        Ok(truncate_output(render_result(&result)))
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

/// The advertised, collision-free, sanitized name for `tool` on `server`
/// (`mcp__<server>__<tool>`) — the single naming rule [`McpTool::new`] and the
/// config-side capability index ([`super::capability_index`], #426) both build
/// against, so a `capabilities:` annotation naming a raw tool always matches
/// the name the registered tool actually advertises.
pub(crate) fn namespaced_tool_name(server: &str, tool: &str) -> String {
    sanitize(&format!("mcp__{server}__{tool}"))
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

// A client is needed to build an `McpTool`; a dead duplex is enough for
// assertions that never call `run`. Shared with `scoped_tests.rs` (#684).
#[cfg(test)]
pub(crate) fn dead_client() -> Arc<McpClient> {
    use crate::mcp::stdio::StdioClient;
    let (client_end, server_end) = tokio::io::duplex(64);
    drop(server_end);
    let (r, w) = tokio::io::split(client_end);
    Arc::new(McpClient::Stdio(StdioClient::new(
        "srv".to_string(),
        w,
        r,
        None,
    )))
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

    #[tokio::test]
    async fn capability_is_write_the_fail_safe_default() {
        let t = McpTool::new(
            dead_client(),
            "my server",
            def("read.file"),
            &HashMap::new(),
        );
        assert_eq!(t.capabilities(), &[Capability::Write]);
    }

    #[tokio::test]
    async fn capability_resolves_from_the_configured_annotation() {
        let caps = HashMap::from([("search".to_string(), "read".to_string())]);
        let t = McpTool::new(dead_client(), "docs", def("search"), &caps);
        assert_eq!(t.capabilities(), &[Capability::Read]);
    }

    #[tokio::test]
    async fn call_annotation_resolves_to_exec() {
        let caps = HashMap::from([("run".to_string(), "call".to_string())]);
        let t = McpTool::new(dead_client(), "docs", def("run"), &caps);
        assert_eq!(t.capabilities(), &[Capability::Exec]);
    }

    #[tokio::test]
    async fn unrecognized_annotation_string_falls_back_to_write() {
        let caps = HashMap::from([("search".to_string(), "bogus".to_string())]);
        let t = McpTool::new(dead_client(), "docs", def("search"), &caps);
        assert_eq!(t.capabilities(), &[Capability::Write]);
    }

    #[tokio::test]
    async fn annotation_is_keyed_by_the_remote_name_not_the_namespaced_one() {
        // A hint for a *different* tool on the same server must not leak.
        let caps = HashMap::from([("other".to_string(), "read".to_string())]);
        let t = McpTool::new(dead_client(), "docs", def("search"), &caps);
        assert_eq!(t.capabilities(), &[Capability::Write]);
    }

    #[tokio::test]
    async fn namespaces_and_sanitizes_the_name() {
        let t = McpTool::new(
            dead_client(),
            "my server",
            def("read.file"),
            &HashMap::new(),
        );
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

    #[test]
    fn oversized_results_are_byte_capped() {
        let big = "x".repeat(crate::host::MAX_OUTPUT_BYTES + 1024);
        let r = json!({ "content": [ { "type": "text", "text": big } ] });
        let out = truncate_output(render_result(&r));
        assert!(
            out.contains("[truncated:") && out.ends_with("bytes total]"),
            "expected a truncation notice, got tail: …{}",
            &out[out.len().saturating_sub(60)..]
        );
        // Bounded: the cap plus the short notice, nothing near the input size.
        assert!(
            out.len() < crate::host::MAX_OUTPUT_BYTES + 128,
            "len={}",
            out.len()
        );
    }

    #[tokio::test]
    async fn schema_and_description_pass_through() {
        let t = McpTool::new(dead_client(), "srv", def("x"), &HashMap::new());
        assert_eq!(t.description(), "a tool");
        assert_eq!(t.schema(), json!({ "type": "object", "properties": {} }));
    }

    // The required-param pre-check that used to live in `run` (#594) moved to
    // `crate::arg_validate`'s uniform pre-dispatch validation (ADR-0196 §6),
    // which runs against `McpTool::schema()` (the server's own `inputSchema`,
    // unmodified) before `run` is ever called — this is that same mechanism
    // exercised registry-level, the way `tool_runner::run_and_reply` actually
    // calls it (`tools.spec_for` → `arg_validate::validate`/`decline_text`).
    #[tokio::test]
    async fn wrong_args_decline_carries_the_servers_input_schema_verbatim() {
        let t = McpTool::new(
            dead_client(),
            "chess",
            McpToolDef {
                name: "get_puzzle".to_string(),
                description: "fetch a puzzle".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "id": { "type": "string" } },
                    "required": ["id"],
                }),
            },
            &HashMap::new(),
        );
        let mut reg = crate::tools::ToolRegistry::new();
        reg.register_arc(std::sync::Arc::new(t));
        let spec = reg.spec_for("mcp__chess__get_puzzle").unwrap();
        let violation = crate::arg_validate::validate(&spec.schema, "{}").unwrap();
        let decline = crate::arg_validate::decline_text(&spec, &violation, false);
        assert!(decline.contains("missing required: id"), "{decline}");
        // The server's own `inputSchema` surfaces verbatim in the decline, not
        // a re-derived shape.
        assert!(decline.contains("\"required\""), "{decline}");
        assert!(decline.contains("\"id\""), "{decline}");
    }

    /// End to end: a read-annotated MCP tool grades through the real
    /// built-in `research` mode exactly like the `read` capability class,
    /// and an unannotated one (the fail-safe `Write`) is refused there
    /// (`research` class-denies `write`) but allowed under `build` — the
    /// concrete regression this fix closes: a bundled read-only server
    /// (e.g. z.ai's web search) must not be refused wholesale just because
    /// `research` denies writes.
    #[tokio::test]
    async fn read_annotated_tool_runs_in_research_unannotated_is_refused_there_and_allowed_in_build(
    ) {
        use crate::capability::capability_of;
        use crate::mode::ModeTable;
        use entanglement_core::Permission;

        let caps = HashMap::from([("webSearch".to_string(), "read".to_string())]);
        let read_tool = McpTool::new(dead_client(), "search", def("webSearch"), &caps);
        let unannotated_tool =
            McpTool::new(dead_client(), "search", def("mystery"), &HashMap::new());

        let mut registry = crate::tools::ToolRegistry::new();
        registry.register(read_tool);
        registry.register(unannotated_tool);

        let table = ModeTable::builtin().expect("built-in modes must parse");
        let research = table.get("research").expect("research exists");
        let build = table.get("build").expect("build exists");

        let read_caps =
            capability_of("mcp__search__webSearch", &registry).expect("read tool registered");
        let unannotated_caps =
            capability_of("mcp__search__mystery", &registry).expect("unannotated tool registered");

        assert_eq!(
            research.resolve("mcp__search__webSearch", read_caps, None, None),
            Permission::Allow,
            "a read-annotated MCP tool must run in research mode"
        );
        assert_eq!(
            research.resolve("mcp__search__mystery", unannotated_caps, None, None),
            Permission::Deny,
            "an unannotated MCP tool is class-denied write, so research refuses it"
        );
        assert_eq!(
            build.resolve("mcp__search__mystery", unannotated_caps, None, None),
            Permission::Allow,
            "the same unannotated tool is allowed under build"
        );
    }
}
