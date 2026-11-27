//! `post_tool_use` inside `skutter`'s own tools (#968): each host tool runs
//! its hooks before its result returns, so a formatter hook finishes before
//! the agent's next call can read or edit the same file — a script's call
//! included, since it runs the same tool. A refused call never reaches the
//! tool, so it fires nothing. Calls to any other tool (an MCP server's)
//! still fire from the event stream ([`super::lifecycle`]).

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use entanglement_core::{ContentPart, SessionId};
use serde_json::{json, Map, Value};

use super::{parse_input, HookEvent, Hooks};
use crate::capability::Capability;
use crate::tool_vocab::HOST_TOOL_NAMES;
use crate::tools::{CallGrading, PermissionScope, Tool, ToolRegistry, ToolRun};

/// Re-register every host tool in `tools` behind its `post_tool_use` hooks;
/// a no-op when none is configured.
pub fn wrap_host_tools(tools: &mut ToolRegistry, hooks: &Arc<Hooks>) {
    if hooks.post_tool_use.is_empty() {
        return;
    }
    for name in HOST_TOOL_NAMES {
        if let Some(inner) = tools.get(name) {
            tools.register(PostHooked {
                inner,
                hooks: hooks.clone(),
            });
        }
    }
}

/// A tool that runs the `post_tool_use` hooks after each session call.
struct PostHooked {
    inner: Arc<dyn Tool>,
    hooks: Arc<Hooks>,
}

impl PostHooked {
    async fn fire(&self, session: &SessionId, input: &str, result: &anyhow::Result<ToolRun>) {
        // The name rules grade it under (`read_raw` → `read`), as for
        // `pre_tool_use`.
        let tool = self
            .inner
            .call_grading(input)
            .map_or_else(|| self.inner.name().into_owned(), |cg| cg.name);
        let (output, is_error, exit_code) = match result {
            Ok(run) => (
                entanglement_core::content_text(&run.content),
                false,
                run.exit_code,
            ),
            Err(e) => (format!("tool `{tool}` failed: {e}"), true, None),
        };
        let payload = json!({
            "event": HookEvent::PostToolUse.as_str(),
            "session": session.0,
            "tool": tool,
            "input": parse_input(input),
            "output": output,
            "is_error": is_error,
            "exit_code": exit_code,
        });
        self.hooks
            .run_observer(HookEvent::PostToolUse, session, Some(&tool), &payload)
            .await;
    }
}

#[async_trait]
impl Tool for PostHooked {
    fn name(&self) -> Cow<'static, str> {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn schema(&self) -> Value {
        self.inner.schema()
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        self.inner.run(input).await
    }
    async fn run_content(&self, input: &str) -> anyhow::Result<Vec<ContentPart>> {
        self.inner.run_content(input).await
    }
    async fn run_for_session(
        &self,
        session: &SessionId,
        request_id: &str,
        input: &str,
    ) -> anyhow::Result<Vec<ContentPart>> {
        Ok(self
            .run_with_meta(session, request_id, input)
            .await?
            .content)
    }
    async fn run_with_meta(
        &self,
        session: &SessionId,
        request_id: &str,
        input: &str,
    ) -> anyhow::Result<ToolRun> {
        let result = self.inner.run_with_meta(session, request_id, input).await;
        self.fire(session, input, &result).await;
        result
    }
    fn capabilities(&self) -> &'static [Capability] {
        self.inner.capabilities()
    }
    fn call_grading(&self, input: &str) -> Option<CallGrading> {
        self.inner.call_grading(input)
    }
    fn join_workdir(&self, args: &mut Map<String, Value>, base: &str) -> bool {
        self.inner.join_workdir(args, base)
    }
    fn escape_target(&self, input: &str) -> Option<String> {
        self.inner.escape_target(input)
    }
    fn permission_scope(&self) -> PermissionScope {
        self.inner.permission_scope()
    }
    fn permission_arg(&self, input: &str) -> Option<String> {
        self.inner.permission_arg(input)
    }
    fn script_unbounded(&self) -> bool {
        self.inner.script_unbounded()
    }
    fn advertised(&self) -> bool {
        self.inner.advertised()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::HookSpec;

    fn hooks(command: String, tools: &[&str]) -> Arc<Hooks> {
        Arc::new(Hooks {
            post_tool_use: vec![HookSpec {
                command,
                tools: tools.iter().map(|t| t.to_string()).collect(),
                timeout_secs: 10,
            }],
            ..Default::default()
        })
    }

    /// The hook has run — and seen the call — by the time the result is
    /// back; the wrapped tool keeps its spec and grading answers.
    #[tokio::test]
    async fn the_hook_runs_before_the_result_returns() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.json");
        std::fs::write(dir.path().join("a.txt"), "hi\n").unwrap();
        let mut tools = crate::host::host_tools(dir.path().to_path_buf());
        let plain = tools.specs();
        wrap_host_tools(&mut tools, &hooks(format!("cat > {}", log.display()), &[]));
        assert_eq!(
            format!("{:?}", tools.specs()),
            format!("{plain:?}"),
            "the spec is the inner tool's"
        );
        let read = tools.get("read").unwrap();
        assert_eq!(
            read.escape_target(r#"{"path":"a.txt"}"#).as_deref(),
            Some("a.txt")
        );
        read.run_with_meta(&SessionId::new("s"), "r1", r#"{"path":"a.txt"}"#)
            .await
            .unwrap();
        let payload: Value = serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(payload["event"], "post_tool_use");
        assert_eq!(payload["tool"], "read");
        assert_eq!(payload["input"]["path"], "a.txt");
        assert_eq!(payload["output"], "1: hi\n");
        assert_eq!(payload["is_error"], false);
    }

    /// A failed run fires with `is_error`; the tool filter still applies;
    /// with no hook configured nothing is wrapped.
    #[tokio::test]
    async fn a_failure_fires_and_the_filter_applies() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.json");
        let mut tools = crate::host::host_tools(dir.path().to_path_buf());
        wrap_host_tools(
            &mut tools,
            &hooks(format!("cat > {}", log.display()), &["read"]),
        );
        let s = SessionId::new("s");
        let glob = tools.get("glob").unwrap();
        glob.run_with_meta(&s, "r1", r#"{"pattern":"*"}"#)
            .await
            .unwrap();
        assert!(!log.exists(), "the filter skips glob");
        let read = tools.get("read").unwrap();
        assert!(read
            .run_with_meta(&s, "r2", r#"{"path":"nope"}"#)
            .await
            .is_err());
        let payload: Value = serde_json::from_str(&std::fs::read_to_string(&log).unwrap()).unwrap();
        assert_eq!(payload["is_error"], true);
        assert!(payload["output"]
            .as_str()
            .unwrap()
            .starts_with("tool `read` failed"));

        let mut bare = crate::host::host_tools(dir.path().to_path_buf());
        let before = Arc::as_ptr(&bare.get("read").unwrap());
        wrap_host_tools(&mut bare, &Arc::new(Hooks::default()));
        assert!(std::ptr::addr_eq(
            before,
            Arc::as_ptr(&bare.get("read").unwrap())
        ));
    }
}
