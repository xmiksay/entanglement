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
use std::sync::{Arc, RwLock};

use entanglement_core::{ContentPart, SessionId, ToolCall, ToolSpec};

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

    /// Session-aware entry point (#360) — what [`ToolRegistry::execute`] actually
    /// calls. Default delegates to [`run_content`][Tool::run_content], so an
    /// existing single-tenant tool is unaffected; a multi-tenant embedder's tool
    /// (e.g. one that picks a per-tenant `HttpClient`/DB scope by session) overrides
    /// this instead.
    async fn run_for_session(
        &self,
        _session: &SessionId,
        input: &str,
    ) -> anyhow::Result<Vec<ContentPart>> {
        self.run_content(input).await
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

/// A [`ToolRegistry`] shared mutably across the engine's lifetime (#372,
/// ADR-0096) — the tool executor's replacement for owning a `ToolRegistry` by
/// value. `std::sync::RwLock` (not `tokio::sync`) so it can be read
/// synchronously from the [`entanglement_core::ToolSpecResolver`] closure,
/// which is deliberately sync (ADR-0076) and must not block on I/O; a registry
/// read is in-memory only, so the brief sync lock is never held across an
/// `.await`. A writer (live MCP add/remove, #4) briefly excludes readers, which
/// is fine — registration is rare compared to dispatch.
pub type SharedRegistry = Arc<RwLock<ToolRegistry>>;

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        let name = tool.name();
        self.tools.insert(name, Arc::new(tool));
    }

    /// Drop a registered tool by name, returning it if it was present. The
    /// dynamic counterpart to [`register`][Self::register] — the seam live MCP
    /// server removal (#4) needs to retract a server's tools without rebuilding
    /// the whole registry.
    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.remove(name)
    }

    /// Drop every tool whose name starts with `prefix` — e.g. `mcp__<server>__`
    /// to retract an entire MCP server's tools in one call (#4).
    pub fn unregister_prefix(&mut self, prefix: &str) {
        self.tools.retain(|name, _| !name.starts_with(prefix));
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Every registered tool name, for a listing surface (e.g. `/mcp list`).
    /// Unsorted — callers that need a stable order sort it themselves.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().map(|n| n.to_string()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Wrap into the shared, mutably-lockable form the tool executor dispatches
    /// against (#372, ADR-0096): cheap to clone (an `Arc`), read-locked per
    /// dispatch to snapshot an owned [`ToolRegistry`] without holding the lock
    /// across a tool's `.await`.
    pub fn shared(self) -> SharedRegistry {
        Arc::new(RwLock::new(self))
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

    /// Execute a model-requested [`ToolCall`] for a given session, returning the
    /// result as multimodal [`ContentPart`]s (#221) — text for most tools, an
    /// image block for `read` on an image. Unknown tools and failures yield a text
    /// part the engine feeds back to the model rather than erroring the run.
    /// `session` (#360) lets a session-aware tool (e.g. a multi-tenant embedder's
    /// per-tenant MCP dispatch) tell callers apart; the executor already has it at
    /// every call site (`resolve_effective` takes it too).
    pub async fn execute(&self, call: &ToolCall, session: &SessionId) -> Vec<ContentPart> {
        match self.tools.get(call.name.as_str()) {
            Some(tool) => match tool.run_for_session(session, &call.input).await {
                Ok(content) => content,
                Err(e) => text_parts(format!("tool `{}` failed: {e}", call.name)),
            },
            None => text_parts(self.unknown_tool_message(&call.name)),
        }
    }

    /// Format the "unknown tool" error for a name that isn't registered (#437),
    /// enriched with a closest-match hint (smallest edit distance over the
    /// registered names) and — when the roster is short — the full name list, so
    /// a weak model that hallucinated a name can self-correct in one round
    /// instead of guessing again.
    pub fn unknown_tool_message(&self, name: &str) -> String {
        let mut names: Vec<&str> = self.tools.keys().map(AsRef::as_ref).collect();
        names.sort_unstable();
        let mut msg = format!("unknown tool: `{name}`");
        if let Some(hint) = closest_name(name, &names) {
            msg.push_str(&format!(" — did you mean `{hint}`?"));
        }
        if !names.is_empty() && names.len() <= 12 {
            msg.push_str(&format!(" (registered tools: {})", names.join(", ")));
        }
        msg
    }
}

/// Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for (j, &bc) in b.iter().enumerate() {
            let cost = usize::from(a[i - 1] != bc);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// The closest registered name to `name` by edit distance, capped so a wildly
/// different name (or an empty/tiny registry) doesn't surface a useless hint.
fn closest_name<'a>(name: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let max_distance = (name.chars().count() / 2).max(2);
    candidates
        .iter()
        .map(|&c| (c, levenshtein(name, c)))
        .filter(|(_, d)| *d <= max_distance)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
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

    fn dummy_session() -> SessionId {
        SessionId::new("test-session")
    }

    #[tokio::test]
    async fn runs_registered_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let out = reg
            .execute(
                &ToolCall {
                    id: "1".into(),
                    name: "echo".into(),
                    input: "hi".into(),
                    provider_meta: None,
                },
                &dummy_session(),
            )
            .await;
        assert_eq!(out, vec![ContentPart::text("hi")]);
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_not_fatal() {
        let reg = ToolRegistry::new();
        let out = reg
            .execute(
                &ToolCall {
                    id: "1".into(),
                    name: "nope".into(),
                    input: "".into(),
                    provider_meta: None,
                },
                &dummy_session(),
            )
            .await;
        assert_eq!(out.len(), 1);
        assert!(out[0].as_text().unwrap().contains("unknown tool"));
    }

    #[test]
    fn unknown_tool_message_hints_the_closest_registered_name() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let msg = reg.unknown_tool_message("ecko");
        assert!(msg.contains("unknown tool: `ecko`"), "{msg}");
        assert!(msg.contains("did you mean `echo`?"), "{msg}");
        assert!(msg.contains("registered tools: echo"), "{msg}");
    }

    #[test]
    fn unknown_tool_message_omits_hint_when_nothing_is_close() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let msg = reg.unknown_tool_message("completely_different_name");
        assert!(!msg.contains("did you mean"), "{msg}");
    }

    /// A session-aware tool overriding `run_for_session` (#360) sees the caller's
    /// [`SessionId`] and can branch on it — the seam a multi-tenant embedder needs
    /// to dispatch per-tenant (distinct MCP endpoints, DB-scoped writes) through one
    /// shared registry instead of one registry per user.
    struct WhoAmI;
    #[async_trait]
    impl Tool for WhoAmI {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("whoami")
        }
        async fn run(&self, _input: &str) -> anyhow::Result<String> {
            unreachable!("run_for_session is overridden; run/run_content are never called")
        }
        async fn run_for_session(
            &self,
            session: &SessionId,
            _input: &str,
        ) -> anyhow::Result<Vec<ContentPart>> {
            Ok(text_parts(session.0.clone()))
        }
    }

    #[tokio::test]
    async fn session_aware_tool_sees_caller_session() {
        let mut reg = ToolRegistry::new();
        reg.register(WhoAmI);
        let call = ToolCall {
            id: "1".into(),
            name: "whoami".into(),
            input: "".into(),
            provider_meta: None,
        };
        let alice = reg.execute(&call, &SessionId::new("alice")).await;
        let bob = reg.execute(&call, &SessionId::new("bob")).await;
        assert_eq!(alice[0].as_text().unwrap(), "alice");
        assert_eq!(bob[0].as_text().unwrap(), "bob");
        assert_ne!(alice, bob);
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

    #[test]
    fn unregister_removes_and_returns_the_tool() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        assert!(reg.contains("echo"));
        let removed = reg.unregister("echo");
        assert!(removed.is_some());
        assert!(!reg.contains("echo"));
        assert!(reg.unregister("echo").is_none());
    }

    #[test]
    fn contains_and_names_reflect_registered_tools() {
        let mut reg = ToolRegistry::new();
        assert!(!reg.contains("echo"));
        assert_eq!(reg.names(), Vec::<String>::new());
        reg.register(Echo);
        assert!(reg.contains("echo"));
        assert_eq!(reg.names(), vec!["echo".to_string()]);
    }

    struct NamedTool(&'static str);
    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed(self.0)
        }
        async fn run(&self, _input: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
    }

    #[test]
    fn unregister_prefix_drops_only_matching_names() {
        let mut reg = ToolRegistry::new();
        reg.register(NamedTool("mcp__github__list_issues"));
        reg.register(NamedTool("mcp__github__create_issue"));
        reg.register(NamedTool("mcp__slack__post"));
        reg.register(Echo);
        reg.unregister_prefix("mcp__github__");
        let mut names = reg.names();
        names.sort();
        assert_eq!(
            names,
            vec!["echo".to_string(), "mcp__slack__post".to_string()]
        );
    }

    #[test]
    fn shared_registry_reads_reflect_writes() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let shared = reg.shared();
        assert!(shared.read().unwrap().contains("echo"));
        shared.write().unwrap().register(NamedTool("extra"));
        assert!(shared.read().unwrap().contains("extra"));
        shared.write().unwrap().unregister("echo");
        assert!(!shared.read().unwrap().contains("echo"));
    }
}
