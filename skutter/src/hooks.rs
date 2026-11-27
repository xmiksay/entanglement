//! Lifecycle hooks (#199, ADR-0066) — user-configured external commands run
//! around tool execution and on prompt ingress, for policy, telemetry, and
//! notifications.
//!
//! Hooks are `skutter`'s, not the library's (#964): the runtime executor runs
//! none. They hang off the two seams every embedder already has — the
//! pluggable [`PermissionResolver`](entanglement_runtime::policy::PermissionResolver)
//! and the `InMsg`/`OutEvent` fan-out — so neither the library nor core gains
//! a hook seam:
//!
//! - **`pre_tool_use`** — [`HookResolver`] wraps the permission resolver and
//!   runs it while a call is graded. A hook that exits **non-zero denies the
//!   call** (graded `Deny`, the hook's output logged): the tool never runs.
//!   This is the policy gate — a hook can veto a `bash rm …` or an `edit`
//!   outside a path.
//! - **`post_tool_use`** — fires on the `ToolOutput` of a host-tool call the
//!   runtime was asked to run (`ToolExec`; not `agent`/`poll`/`ask_user`/…).
//!   Observational: the exit code is logged, never fed back to the model.
//! - **`user_prompt_submit`** — fires on an inbound `InMsg::Prompt`.
//!   Observational (telemetry/logging); it does not gate the prompt.
//! - **`session_start`/`session_end`/`stop`/`pre_compact`/`subagent_stop`/
//!   `notification`** (#734) — observe-only points derived from the `OutEvent`
//!   stream ([`lifecycle`]); none can block.
//!
//! Each hook is an `sh -c <command>` child. It receives a JSON payload on stdin
//! (`{event, session, tool?, input?, output?, prompt?}`) and a few env vars
//! (`ENTANGLEMENT_HOOK_EVENT`, `ENTANGLEMENT_SESSION_ID`, `ENTANGLEMENT_TOOL_NAME`
//! for tool hooks). It runs under a timeout in its **own process group** — the
//! same containment the exec tools use (`entanglement_runtime::exec`) — so a
//! hook that spawns children can't orphan them past the timeout.

use anyhow::Context;
use entanglement_core::SessionId;
use serde::Deserialize;
use serde_json::json;

mod invoke;
mod lifecycle;
mod resolver;

use invoke::invoke;
#[cfg(test)]
use invoke::Outcome;
pub use lifecycle::spawn;
pub use resolver::HookResolver;

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
    /// tool. Ignored by every other point.
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
    /// Parse the config's `hooks:` section, which the library carries
    /// through unparsed (`Config::hooks`). Absent (`Null`) ⇒ no hooks.
    pub fn parse(section: &serde_yaml::Value) -> anyhow::Result<Self> {
        if section.is_null() {
            return Ok(Self::default());
        }
        serde_yaml::from_value(section.clone()).context("parsing config.yml `hooks:`")
    }

    /// Whether any lifecycle point has a registered command.
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty() && !self.has_observers()
    }

    /// Whether any point the [`spawn`]ed watcher drives has a command — every
    /// point but `pre_tool_use`, which [`HookResolver`] runs.
    pub fn has_observers(&self) -> bool {
        lifecycle::OBSERVED
            .iter()
            .any(|e| !self.specs(*e).is_empty())
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

    /// The commands that apply to one firing: `post_tool_use` honors each
    /// spec's tool filter, every other observer point ignores it.
    fn applicable<'a>(
        &'a self,
        event: HookEvent,
        tool: Option<&'a str>,
    ) -> impl Iterator<Item = &'a HookSpec> {
        self.specs(event).iter().filter(move |s| {
            event != HookEvent::PostToolUse || s.matches_tool(tool.unwrap_or_default())
        })
    }

    /// Run every command registered for an observe-only point, in order. A
    /// failure (non-zero, timeout, spawn error) is logged, never acted on:
    /// none of these points can veto anything (#734).
    async fn run_observer(
        &self,
        event: HookEvent,
        session: &SessionId,
        tool: Option<&str>,
        payload: &serde_json::Value,
    ) {
        for spec in self.applicable(event, tool) {
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
    /// soon as one **blocks** (non-zero exit or timeout) — the tool must not
    /// run; `None` means every hook cleared and the tool may proceed.
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
                return Some(format!(
                    "tool `{tool}` blocked by pre_tool_use hook: {}",
                    outcome.reason()
                ));
            }
        }
        None
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
            tools: vec!["bash".into(), "write".into()],
            timeout_secs: 30,
        };
        assert!(s.matches_tool("bash"));
        assert!(s.matches_tool("write"));
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
    async fn post_hook_runs_as_side_effect_for_a_matching_tool() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let hooks = Hooks {
            post_tool_use: vec![HookSpec {
                command: format!("touch {}", marker.display()),
                tools: vec!["write".into()],
                timeout_secs: 30,
            }],
            ..Default::default()
        };
        let s = SessionId::new("s");
        hooks
            .run_observer(HookEvent::PostToolUse, &s, Some("read"), &json!({}))
            .await;
        assert!(!marker.exists(), "the tool filter must skip `read`");
        hooks
            .run_observer(HookEvent::PostToolUse, &s, Some("write"), &json!({}))
            .await;
        assert!(marker.exists());
    }

    #[test]
    fn an_absent_section_parses_to_no_hooks() {
        assert!(Hooks::parse(&serde_yaml::Value::Null).unwrap().is_empty());
        let empty: serde_yaml::Value = serde_yaml::from_str("{}").unwrap();
        assert!(Hooks::parse(&empty).unwrap().is_empty());
    }

    #[test]
    fn every_point_parses_from_the_section() {
        let section: serde_yaml::Value = serde_yaml::from_str(
            "pre_tool_use:\n  - command: \"echo hi\"\n    tools: [bash, edit]\n\
             user_prompt_submit: [{command: logger}]\n\
             session_start: [{command: a}]\n\
             session_end: [{command: b}]\n\
             stop: [{command: c, timeout_secs: 5}]\n\
             pre_compact: [{command: d}]\n\
             subagent_stop: [{command: e}]\n\
             notification: [{command: notify-send}]\n",
        )
        .unwrap();
        let h = Hooks::parse(&section).unwrap();
        assert_eq!(h.pre_tool_use[0].command, "echo hi");
        assert_eq!(h.pre_tool_use[0].tools, vec!["bash", "edit"]);
        assert!(h.post_tool_use.is_empty());
        assert_eq!(h.user_prompt_submit.len(), 1);
        assert_eq!(h.session_start[0].command, "a");
        assert_eq!(h.session_end[0].command, "b");
        assert_eq!(h.stop[0].timeout_secs, 5);
        assert_eq!(h.pre_compact[0].command, "d");
        assert_eq!(h.subagent_stop[0].command, "e");
        assert_eq!(h.notification[0].command, "notify-send");
        assert!(h.has_observers());
    }

    #[test]
    fn an_unknown_hook_point_is_a_loud_error() {
        let section: serde_yaml::Value =
            serde_yaml::from_str("pre_tool_yoos:\n  - command: x\n").unwrap();
        let err = Hooks::parse(&section).unwrap_err();
        assert!(format!("{err:#}").contains("hooks:"), "{err:#}");
    }

    /// B1 (ADR-0227): a hook is a child like any other — an `http_auth`
    /// secret never reaches it.
    #[tokio::test]
    async fn a_hidden_secret_never_reaches_a_hook() {
        // A name no other test sets: the hidden set is process-wide.
        let var = "SKUTTER_TEST_HOOK_HIDDEN_SECRET";
        std::env::set_var(var, "s3cret");
        entanglement_runtime::secret_env::hide([var.to_string()]);
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("env.out");
        let hooks = Hooks {
            stop: vec![spec(&format!("printf '%s' \"${var}\" > {}", out.display()))],
            ..Default::default()
        };
        hooks
            .run_observer(HookEvent::Stop, &SessionId::new("s"), None, &json!({}))
            .await;
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "");
        std::env::remove_var(var);
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
