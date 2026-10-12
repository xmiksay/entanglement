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

use entanglement_core::{content_text, ContentPart, SessionId};

use super::client::{McpClient, McpToolDef};
use super::result::{render_result, McpResult};
use crate::capability::Capability;
use crate::tools::{Tool, ToolRun};

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
        Ok(content_text(&self.call(input).await?.content))
    }

    async fn run_content(&self, input: &str) -> Result<Vec<ContentPart>> {
        Ok(self.call(input).await?.content)
    }

    /// What the registry actually calls: carries the server's `isError` onto
    /// the structured `is_error` side channel (ADR-0176, #727).
    async fn run_with_meta(
        &self,
        _session: &SessionId,
        _request_id: &str,
        input: &str,
    ) -> Result<ToolRun> {
        let McpResult { content, is_error } = self.call(input).await?;
        Ok(ToolRun {
            is_error,
            ..ToolRun::new(content)
        })
    }
}

impl McpTool {
    /// One `tools/call` round-trip, rendered (text, images, resources) by
    /// [`render_result`].
    async fn call(&self, input: &str) -> Result<McpResult> {
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
        Ok(render_result(&result))
    }
}

/// The advertised, collision-free, sanitized name for `tool` on `server`
/// (`mcp__<server>__<tool>`) — the single naming rule [`McpTool::new`] and the
/// config-side capability index ([`super::capability_index`], #426) both build
/// against, so a `capabilities:` annotation naming a raw tool always matches
/// the name the registered tool actually advertises.
pub(crate) fn namespaced_tool_name(server: &str, tool: &str) -> String {
    entanglement_core::mcp_tool_name(server, tool)
}

/// The overlay/rule pattern covering every tool of `server` — built from the
/// registered spelling, so `my.docs` yields `mcp__my_docs__*`. Matches no
/// other server's tools because the roster refuses names whose prefixes
/// nest or coincide ([`names_clash`]).
pub fn server_tools_pattern(server: &str) -> String {
    format!("{}*", namespaced_tool_name(server, ""))
}

/// Whether two server names map onto overlapping registered tool names:
/// identical after sanitizing (`my.docs` / `my_docs`), or one's
/// `mcp__<name>__` prefix nesting the other's (`a` / `a__b`). Either makes a
/// tool name — and an overlay or rule pattern like `mcp__a__*` — ambiguous
/// between the two, so only the first such name is registered — by the global roster and
/// within a per-user scope alike.
pub(crate) fn names_clash(a: &str, b: &str) -> bool {
    let (pa, pb) = (namespaced_tool_name(a, ""), namespaced_tool_name(b, ""));
    pa.starts_with(&pb) || pb.starts_with(&pa)
}

/// Which of `servers` registered tool name `tool` belongs to, by the same
/// sanitized `mcp__<server>__` prefix [`McpTool::new`] registers under — a
/// naive split on `__` gets `my.docs` (registered `my_docs`) or `a__b` wrong.
/// The longest prefix wins, so `a__b`'s tools never resolve to a server `a`.
pub(crate) fn owning_server<'a>(
    servers: impl IntoIterator<Item = &'a str>,
    tool: &str,
) -> Option<&'a str> {
    servers
        .into_iter()
        .map(|name| (name, namespaced_tool_name(name, "")))
        .filter(|(_, prefix)| tool.starts_with(prefix.as_str()))
        .max_by_key(|(_, prefix)| prefix.len())
        .map(|(name, _)| name)
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

    /// Registration and core's `mcp` unwrap must spell a name identically, or
    /// an envelope call lands on a name nothing registered (ADR-0226).
    #[test]
    fn registration_matches_the_envelope_unwrap_spelling() {
        for (server, tool, want) in [
            ("my.docs", "get.page", "mcp__my_docs__get_page"),
            ("my docs", "read", "mcp__my_docs__read"),
            ("a__b", "c", "mcp__a__b__c"),
            ("gh", "search-issues", "mcp__gh__search-issues"),
        ] {
            assert_eq!(namespaced_tool_name(server, tool), want);
            assert_eq!(entanglement_core::mcp_tool_name(server, tool), want);
        }
    }

    #[test]
    fn a_server_pattern_uses_the_registered_spelling() {
        let pattern = server_tools_pattern("my.docs");
        assert_eq!(pattern, "mcp__my_docs__*");
        let deny = entanglement_core::ToolOverlayEntry::deny(pattern);
        assert!(deny.matches("mcp__my_docs__read"));
        assert!(!deny.matches("mcp__other__read"));
    }

    #[test]
    fn owning_server_matches_the_sanitized_prefix_longest_first() {
        let servers = ["my.docs", "a", "a__b", "gh"];
        assert_eq!(
            owning_server(servers, "mcp__my_docs__read"),
            Some("my.docs")
        );
        assert_eq!(owning_server(servers, "mcp__a__b__x"), Some("a__b"));
        assert_eq!(owning_server(servers, "mcp__a__x"), Some("a"));
        assert_eq!(owning_server(servers, "mcp__nope__x"), None);
        assert_eq!(owning_server(servers, "read"), None);
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
