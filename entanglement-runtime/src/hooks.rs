//! Lifecycle hooks (#199, ADR-0066) — user-configured external commands run
//! around tool execution and on prompt ingress, for policy, telemetry, and
//! formatting side-effects.
//!
//! Hooks are a **runtime interceptor**, not a core concept: core neither knows
//! nor cares that a command runs before a tool. They hang off the two seams the
//! runtime already owns — the `tool_runner` dispatch of a `ToolExec` and the
//! inbound `InMsg::Prompt` fan-out — so no new protocol surface is added. Three
//! lifecycle points, mirroring the shape of Claude Code's hooks:
//!
//! - **`pre_tool_use`** — fires *before* the generic dispatch runs a tool
//!   ([`crate::tool_runner`]'s `Intercept::Permission` route). A hook that exits
//!   **non-zero blocks the call**: the tool never runs and the model gets a
//!   `ToolResult` explaining the block (the hook's own output). This is the
//!   policy gate — a hook can veto a `bash rm …` or an `edit` outside a path.
//! - **`post_tool_use`** — fires *after* a tool produces its result.
//!   Observational: the exit code is logged but never fed back to the model, so
//!   a formatter/telemetry hook runs as a pure side-effect (e.g. `prettier` on a
//!   just-written file). It cannot rewrite the `ToolResult`.
//! - **`user_prompt_submit`** — fires when an `InMsg::Prompt` reaches the engine.
//!   Observational (telemetry/logging); it does not gate the prompt.
//! - **`session_start`/`session_end`/`stop`/`pre_compact`/`subagent_stop`/
//!   `notification`** (#734) — observe-only points derived from the `OutEvent`
//!   stream by a runtime subscriber ([`lifecycle`]); none can block.
//!
//! Each hook is an `sh -c <command>` child. It receives a JSON payload on stdin
//! (`{event, session, tool?, input?, output?, prompt?}`) and a few env vars
//! (`ENTANGLEMENT_HOOK_EVENT`, `ENTANGLEMENT_SESSION_ID`, `ENTANGLEMENT_TOOL_NAME`
//! for tool hooks). It runs under a timeout in its **own process group** — the
//! same containment the exec tools use ([`crate::host::exec`]) — so a hook that
//! spawns children can't orphan them past the timeout.
//!
//! Hooks are scoped to the **generic host-tool dispatch** only: the orchestration
//! routes (`agent`/`ask_user`/`propose_plan`) and the self-permissioning `rhai`
//! tool bypass them, matching the issue's "around `tool_runner::dispatch`" scope.

use entanglement_core::SessionId;
use serde::Deserialize;
use serde_json::json;

mod invoke;
pub(crate) mod lifecycle;

use invoke::invoke;
#[cfg(test)]
use invoke::Outcome;

/// Default per-hook wall-clock timeout. A hook that hangs must not stall the turn
/// forever; on timeout the process group is killed (see [`invoke`]).
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// The lifecycle point a hook fires at. The string form is exposed to the hook
/// command as `ENTANGLEMENT_HOOK_EVENT` and echoed in the JSON payload's `event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    PreCompact,
    SubagentStop,
    Notification,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "pre_tool_use",
            HookEvent::PostToolUse => "post_tool_use",
            HookEvent::UserPromptSubmit => "user_prompt_submit",
            HookEvent::SessionStart => "session_start",
            HookEvent::SessionEnd => "session_end",
            HookEvent::Stop => "stop",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::SubagentStop => "subagent_stop",
            HookEvent::Notification => "notification",
        }
    }
}

/// The `hooks:` section of the user config (#199). Each list holds the commands
/// registered for one lifecycle point, run in listed order. `deny_unknown_fields`
/// keeps a typo'd event name a loud error, matching the rest of the config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(default)]
    pub pre_tool_use: Vec<HookSpec>,
    #[serde(default)]
    pub post_tool_use: Vec<HookSpec>,
    #[serde(default)]
    pub user_prompt_submit: Vec<HookSpec>,
    /// The observe-only lifecycle points (#734), driven off the `OutEvent`
    /// stream by [`lifecycle`] — see that module for when each fires.
    #[serde(default)]
    pub session_start: Vec<HookSpec>,
    #[serde(default)]
    pub session_end: Vec<HookSpec>,
    #[serde(default)]
    pub stop: Vec<HookSpec>,
    #[serde(default)]
    pub pre_compact: Vec<HookSpec>,
    #[serde(default)]
    pub subagent_stop: Vec<HookSpec>,
    #[serde(default)]
    pub notification: Vec<HookSpec>,
}

/// One configured hook command.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSpec {
    /// Shell command run via `sh -c`.
    pub command: String,
    /// Tool-name filter for the tool hooks (`pre_`/`post_tool_use`). Empty ⇒ every
    /// tool. Ignored by `user_prompt_submit`.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Per-hook wall-clock timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl HookSpec {
    /// Whether this hook applies to `tool`: an empty filter matches everything.
    fn matches_tool(&self, tool: &str) -> bool {
        self.tools.is_empty() || self.tools.iter().any(|t| t == tool)
    }
}

impl Hooks {
    /// Whether any lifecycle point has a registered command — lets the executor
    /// skip all hook plumbing when the config left the section empty (the norm).
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.user_prompt_submit.is_empty()
            && !self.has_lifecycle()
    }

    /// Whether any `OutEvent`-driven lifecycle point (#734) has a command —
    /// the executor only subscribes the [`lifecycle`] watcher when one does.
    pub fn has_lifecycle(&self) -> bool {
        lifecycle::EVENTS.iter().any(|e| !self.specs(*e).is_empty())
    }

    /// The commands registered for `event`.
    fn specs(&self, event: HookEvent) -> &[HookSpec] {
        match event {
            HookEvent::PreToolUse => &self.pre_tool_use,
            HookEvent::PostToolUse => &self.post_tool_use,
            HookEvent::UserPromptSubmit => &self.user_prompt_submit,
            HookEvent::SessionStart => &self.session_start,
            HookEvent::SessionEnd => &self.session_end,
            HookEvent::Stop => &self.stop,
            HookEvent::PreCompact => &self.pre_compact,
            HookEvent::SubagentStop => &self.subagent_stop,
            HookEvent::Notification => &self.notification,
        }
    }

    /// Run every command registered for an observe-only lifecycle point, in
    /// order. A failure (non-zero, timeout, spawn error) is logged, never
    /// acted on: none of these points can veto anything (#734).
    async fn run_observer(
        &self,
        event: HookEvent,
        session: &SessionId,
        tool: Option<&str>,
        payload: &serde_json::Value,
    ) {
        for spec in self.specs(event) {
            let outcome = invoke(spec, event, tool, session, payload.clone()).await;
            if !outcome.succeeded() {
                tracing::warn!(
                    event = event.as_str(),
                    detail = outcome.reason(),
                    "lifecycle hook failed"
                );
            }
        }
    }

    /// Run the `pre_tool_use` hooks for `tool` in order. Returns `Some(reason)` as
    /// soon as one **blocks** (non-zero exit or timeout) — the tool must not run
    /// and the reason is folded back as the tool result; `None` means every hook
    /// cleared and the tool may proceed.
    pub async fn run_pre_tool_use(
        &self,
        session: &SessionId,
        tool: &str,
        input: &str,
    ) -> Option<String> {
        for spec in self.pre_tool_use.iter().filter(|s| s.matches_tool(tool)) {
            let payload = json!({
                "event": HookEvent::PreToolUse.as_str(),
                "session": session.0,
                "tool": tool,
                "input": parse_input(input),
            });
            let outcome = invoke(spec, HookEvent::PreToolUse, Some(tool), session, payload).await;
            if !outcome.succeeded() {
                let detail = outcome.reason();
                tracing::info!(%tool, detail, "pre_tool_use hook blocked tool");
                return Some(format!(
                    "tool `{tool}` blocked by pre_tool_use hook: {detail}"
                ));
            }
        }
        None
    }

    /// Run the `post_tool_use` hooks for `tool` in order, observing the tool's
    /// `output` and `is_error` (#636, ADR-0176). Purely side-effecting: a
    /// non-zero exit is logged, never fed back to the model — and `is_error`
    /// itself cannot rewrite `content` either, it only lets a hook branch on the
    /// outcome instead of re-parsing `output`'s text. `exit_code` (#681,
    /// ADR-0186) is the observed process exit status for `bash`/`call` — JSON
    /// `null` for every other tool and for a killed process — so a hook can
    /// branch on a specific status without string-matching `[exit N]`.
    pub async fn run_post_tool_use(
        &self,
        session: &SessionId,
        tool: &str,
        input: &str,
        output: &str,
        is_error: bool,
        exit_code: Option<i32>,
    ) {
        for spec in self.post_tool_use.iter().filter(|s| s.matches_tool(tool)) {
            let payload = json!({
                "event": HookEvent::PostToolUse.as_str(),
                "session": session.0,
                "tool": tool,
                "input": parse_input(input),
                "output": output,
                "is_error": is_error,
                "exit_code": exit_code,
            });
            let outcome = invoke(spec, HookEvent::PostToolUse, Some(tool), session, payload).await;
            if !outcome.succeeded() {
                tracing::warn!(%tool, detail = outcome.reason(), "post_tool_use hook failed");
            }
        }
    }

    /// Run the `user_prompt_submit` hooks for a submitted prompt. Observational.
    pub async fn run_user_prompt_submit(&self, session: &SessionId, prompt: &str) {
        for spec in &self.user_prompt_submit {
            let payload = json!({
                "event": HookEvent::UserPromptSubmit.as_str(),
                "session": session.0,
                "prompt": prompt,
            });
            let outcome = invoke(spec, HookEvent::UserPromptSubmit, None, session, payload).await;
            if !outcome.succeeded() {
                tracing::warn!(detail = outcome.reason(), "user_prompt_submit hook failed");
            }
        }
    }
}

/// Parse a tool's `input` (already-JSON text) into a value for the payload,
/// falling back to the raw string if it isn't valid JSON.
fn parse_input(input: &str) -> serde_json::Value {
    serde_json::from_str(input).unwrap_or_else(|_| serde_json::Value::String(input.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str) -> HookSpec {
        HookSpec {
            command: command.to_string(),
            tools: Vec::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    #[test]
    fn empty_filter_matches_every_tool() {
        assert!(spec("true").matches_tool("bash"));
    }

    #[test]
    fn non_empty_filter_scopes_to_named_tools() {
        let s = HookSpec {
            command: "true".into(),
            tools: vec!["bash".into(), "call".into()],
            timeout_secs: 30,
        };
        assert!(s.matches_tool("bash"));
        assert!(s.matches_tool("call"));
        assert!(!s.matches_tool("edit"));
    }

    #[test]
    fn default_hooks_are_empty() {
        assert!(Hooks::default().is_empty());
    }

    #[tokio::test]
    async fn pre_hook_exit_zero_does_not_block() {
        let hooks = Hooks {
            pre_tool_use: vec![spec("exit 0")],
            ..Default::default()
        };
        let blocked = hooks
            .run_pre_tool_use(&SessionId::new("s"), "bash", "{}")
            .await;
        assert!(blocked.is_none());
    }

    #[tokio::test]
    async fn pre_hook_non_zero_blocks_with_reason() {
        let hooks = Hooks {
            pre_tool_use: vec![spec("echo nope >&2; exit 3")],
            ..Default::default()
        };
        let blocked = hooks
            .run_pre_tool_use(&SessionId::new("s"), "bash", "{}")
            .await
            .expect("a non-zero pre hook must block");
        assert!(blocked.contains("bash"));
        assert!(
            blocked.contains("nope"),
            "reason should carry hook output: {blocked}"
        );
    }

    #[tokio::test]
    async fn pre_hook_filter_skips_unmatched_tool() {
        let hooks = Hooks {
            pre_tool_use: vec![HookSpec {
                command: "exit 1".into(),
                tools: vec!["edit".into()],
                timeout_secs: 30,
            }],
            ..Default::default()
        };
        // The blocking hook only targets `edit`, so a `bash` call clears.
        assert!(hooks
            .run_pre_tool_use(&SessionId::new("s"), "bash", "{}")
            .await
            .is_none());
        assert!(hooks
            .run_pre_tool_use(&SessionId::new("s"), "edit", "{}")
            .await
            .is_some());
    }

    #[tokio::test]
    async fn pre_hook_receives_payload_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payload.json");
        let hooks = Hooks {
            pre_tool_use: vec![spec(&format!("cat > {}", out.display()))],
            ..Default::default()
        };
        hooks
            .run_pre_tool_use(&SessionId::new("sess-1"), "bash", r#"{"command":"ls"}"#)
            .await;
        let written = std::fs::read_to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["event"], "pre_tool_use");
        assert_eq!(v["session"], "sess-1");
        assert_eq!(v["tool"], "bash");
        assert_eq!(v["input"]["command"], "ls");
    }

    #[tokio::test]
    async fn post_hook_runs_as_side_effect() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let hooks = Hooks {
            post_tool_use: vec![spec(&format!("touch {}", marker.display()))],
            ..Default::default()
        };
        hooks
            .run_post_tool_use(
                &SessionId::new("s"),
                "write",
                "{}",
                "wrote file",
                false,
                None,
            )
            .await;
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn post_hook_payload_carries_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.json");
        let hooks = Hooks {
            post_tool_use: vec![spec(&format!("cat > {}", out.display()))],
            ..Default::default()
        };
        hooks
            .run_post_tool_use(
                &SessionId::new("s"),
                "bash",
                "{}",
                "tool `bash` denied by permission profile",
                true,
                None,
            )
            .await;
        let written = std::fs::read_to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["is_error"], true);
        assert_eq!(v["exit_code"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn post_hook_payload_carries_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.json");
        let hooks = Hooks {
            post_tool_use: vec![spec(&format!("cat > {}", out.display()))],
            ..Default::default()
        };
        hooks
            .run_post_tool_use(
                &SessionId::new("s"),
                "bash",
                "{}",
                "[exit 3]\nboom",
                false,
                Some(3),
            )
            .await;
        let written = std::fs::read_to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["exit_code"], 3);
        assert_eq!(v["is_error"], false);
    }

    #[tokio::test]
    async fn prompt_hook_receives_prompt_text() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("prompt.json");
        let hooks = Hooks {
            user_prompt_submit: vec![spec(&format!("cat > {}", out.display()))],
            ..Default::default()
        };
        hooks
            .run_user_prompt_submit(&SessionId::new("s"), "hello there")
            .await;
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["event"], "user_prompt_submit");
        assert_eq!(v["prompt"], "hello there");
    }

    /// #586: a genuine stdout/stderr read error must veto the hook even on a
    /// clean exit 0 — the captured output can't be trusted to be the whole
    /// story.
    #[test]
    fn outcome_with_io_error_never_succeeds_even_on_clean_exit() {
        let outcome = Outcome {
            code: Some(0),
            timed_out: false,
            text: String::new(),
            io_error: Some("stdout read failed: broken pipe".to_string()),
        };
        assert!(!outcome.succeeded());
        assert!(outcome.reason().contains("broken pipe"));
    }

    #[test]
    fn outcome_reason_surfaces_io_error_alongside_hook_output() {
        let outcome = Outcome {
            code: Some(1),
            timed_out: false,
            text: "some hook output".to_string(),
            io_error: Some("stdout read failed: oops".to_string()),
        };
        let reason = outcome.reason();
        assert!(reason.contains("stdout read failed: oops"));
        assert!(reason.contains("some hook output"));
    }
}
