//! Integration tests for ADR-0207 §11 (stage 5c): making a mode with a
//! finite `question_timeout`/`max_turns`/`max_duration` — `auto`, or any
//! other mode tuned the same way — genuinely unattended.
//!
//! Mirrors `permission_dispatch.rs`'s harness shape (its helpers are private
//! to that module, hence the local copies here) but exercises `Limits`
//! rather than the grade table.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_names::ASK_USER_TOOL;
use entanglement_runtime::tool_runner::spawn_tool_executor_with_policy;
use entanglement_runtime::{Tool, ToolRegistry};

use crate::mode_support::perm_modes;

/// Replays scripted responses in order, then plain text (so the turn ends
/// instead of looping once the fixture's own script runs out).
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
        }
    }
}
#[async_trait]
impl Llm for ScriptedLlm {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| LlmResponse {
                text: "done".into(),
                tool_calls: vec![],
            });
        Ok(stream_from_response(resp))
    }
}

/// A trivial `bash` tool. Declares `Capability::Exec` explicitly, mirroring
/// `permission_dispatch.rs`'s own `EchoBash` (the `Tool::capabilities`
/// default is `Write`, which would mis-grade an `exec`-class mode rule).
struct EchoBash;
#[async_trait]
impl Tool for EchoBash {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
    fn capabilities(&self) -> &'static [entanglement_runtime::capability::Capability] {
        &[entanglement_runtime::capability::Capability::Exec]
    }
}

/// A single-mode table named `"build"` (matches `DEFAULT_MODE`, so a fresh
/// session resolves against it without `SetMode`) carrying the `Limits`
/// under test.
fn one_mode_table(default: Permission, limits: Limits) -> Arc<ModeTable> {
    let mode = Mode {
        name: "build".to_string(),
        default,
        rules: Rules::default(),
        limits,
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"))
}

fn bash_call(id: &str, input: &str) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "bash".into(),
            input: input.into(),
            provider_meta: None,
        }],
    }
}

fn ask_user_call(id: &str, input: &str) -> LlmResponse {
    LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: ASK_USER_TOOL.into(),
            input: input.into(),
            provider_meta: None,
        }],
    }
}

fn done_text() -> LlmResponse {
    LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    }
}

/// Collect `sid`'s events until `Done`, bounded by an idle timeout — mirrors
/// `permission_dispatch.rs::collect`.
async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        if ev.session() == Some(sid) {
            let done = matches!(ev, OutEvent::Done { .. });
            out.push(ev);
            if done {
                break;
            }
        }
    }
    out
}

/// Collect `sid`'s events for a fixed wall-clock window instead of stopping
/// at `Done` — the budget-watcher tests need to observe what happens *after*
/// a turn already ended (a breach detected on the periodic sweep) or need to
/// keep listening past a `Stop` that unwinds with no `Done` of its own.
async fn collect_for(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
    window: Duration,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, sub.recv()).await {
            Ok(Ok(ev)) if ev.session() == Some(sid) => out.push(ev),
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    out
}

/// Wire the real executor (`spawn_tool_executor_with_policy`) against
/// `mode_table`, replaying `scripted` as the model's responses.
fn spawn_with_policy(
    reg: ToolRegistry,
    mode_table: Arc<ModeTable>,
    scripted: Vec<LlmResponse>,
) -> Holly {
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let scripted = Arc::new(scripted);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let shared_tools = reg.shared();
    let active = Arc::new(Mutex::new(HashMap::new()));
    let modes = perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        mode_table.clone(),
        shared_tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
        active,
        modes,
        resolver,
        grants,
        Default::default(),
        None,
        mode_table,
        Arc::new(PlanFileRegistry::new()),
        None,
        None,
        None,
    );
    holly
}

#[tokio::test]
async fn collapsed_ask_denies_silently_the_first_time() {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let limits = Limits {
        question_timeout: 30,
        ..Limits::default()
    };
    let holly = spawn_with_policy(
        reg,
        one_mode_table(Permission::Ask, limits),
        vec![bash_call("t1", "echo hi"), done_text()],
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "an unattended mode's first Ask must never park a prompt; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, is_error: true, .. }
                if output.contains("denied") && output.contains("unattended")
        )),
        "expected a collapsed denial naming the posture; got {events:?}"
    );
}

#[tokio::test]
async fn a_second_identical_call_parks_an_approval() {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let limits = Limits {
        question_timeout: 30,
        ..Limits::default()
    };
    let holly = spawn_with_policy(
        reg,
        one_mode_table(Permission::Ask, limits),
        vec![
            bash_call("t1", "echo hi"),
            bash_call("t2", "echo hi"),
            done_text(),
        ],
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut request_id = None;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), watch.recv()).await {
        if let OutEvent::ToolRequest {
            request_id: rid,
            tool,
            ..
        } = &ev
        {
            if tool == "bash" {
                request_id = Some(rid.clone());
                break;
            }
        }
    }
    let request_id =
        request_id.expect("the repeated identical call must park an approval, not deny again");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: Default::default(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    let denials = events
        .iter()
        .filter(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied")))
        .count();
    assert_eq!(
        denials, 1,
        "only the first identical call should be silently denied; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: echo hi")),
        "the approved repeat should have run; got {events:?}"
    );
}

#[tokio::test]
async fn a_parked_repeat_expires_to_denial_when_nobody_answers() {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let limits = Limits {
        question_timeout: 1,
        ..Limits::default()
    };
    let holly = spawn_with_policy(
        reg,
        one_mode_table(Permission::Ask, limits),
        vec![
            bash_call("t1", "echo hi"),
            bash_call("t2", "echo hi"),
            done_text(),
        ],
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    // Nobody ever answers the parked repeat — it must expire to a denial
    // within `question_timeout`, not hang.
    let events = collect_for(sub, &sid, Duration::from_secs(5)).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "the repeat must still have parked; got {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, is_error: true, .. }
                if output.contains("denied") && output.contains("no response within")
        )),
        "the parked repeat must expire to a denial; got {events:?}"
    );
}

#[tokio::test]
async fn ask_user_timeout_defaults_to_the_first_option() {
    let limits = Limits {
        question_timeout: 1,
        ..Limits::default()
    };
    let holly = spawn_with_policy(
        ToolRegistry::new(),
        one_mode_table(Permission::Allow, limits),
        vec![
            ask_user_call(
                "q1",
                r#"{"questions":[{"question":"Which?","options":[{"label":"A"},{"label":"B"}]}]}"#,
            ),
            LlmResponse {
                text: "acknowledged".into(),
                tool_calls: vec![],
            },
        ],
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, is_error: false, .. } if output == "A")
        ),
        "a timed-out options question must default to its first option; got {events:?}"
    );
}

#[tokio::test]
async fn ask_user_timeout_with_no_options_is_an_error() {
    let limits = Limits {
        question_timeout: 1,
        ..Limits::default()
    };
    let holly = spawn_with_policy(
        ToolRegistry::new(),
        one_mode_table(Permission::Allow, limits),
        vec![
            ask_user_call(
                "q1",
                r#"{"questions":[{"question":"Describe?","options":[]}]}"#,
            ),
            done_text(),
        ],
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, is_error: true, .. } if output.contains("Describe?")
        )),
        "a free-text question with no default must fail as is_error, not invent an answer; \
         got {events:?}"
    );
}

#[tokio::test]
async fn max_turns_ends_the_run_with_a_stated_reason() {
    let limits = Limits {
        max_turns: Some(1),
        ..Limits::default()
    };
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    // Three rounds queued; only the first is allowed to complete.
    let holly = spawn_with_policy(
        reg,
        one_mode_table(Permission::Allow, limits),
        vec![
            bash_call("t1", "1"),
            bash_call("t2", "2"),
            bash_call("t3", "3"),
        ],
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect_for(sub, &sid, Duration::from_secs(3)).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::Error { message, .. } if message.contains("max_turns")
        )),
        "expected a stated max_turns reason; got {events:?}"
    );
    let bash_calls = events
        .iter()
        .filter(|e| matches!(e, OutEvent::ToolExec { tool, .. } if tool == "bash"))
        .count();
    assert!(
        bash_calls <= 1,
        "the run must stop at the turn budget instead of continuing past it; \
         got {bash_calls} bash call(s) in {events:?}"
    );
}

#[tokio::test]
async fn max_duration_ends_the_run_with_a_stated_reason() {
    let limits = Limits {
        max_duration: Some(1),
        ..Limits::default()
    };
    let holly = spawn_with_policy(
        ToolRegistry::new(),
        one_mode_table(Permission::Allow, limits),
        vec![done_text()],
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    // The turn itself finishes almost instantly; the budget watcher's own
    // periodic sweep (every 5s) is what has to catch the stale session, so
    // this window has to clear that sweep at least once past the 1s budget.
    let events = collect_for(sub, &sid, Duration::from_secs(8)).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::Error { message, .. } if message.contains("max_duration")
        )),
        "expected a stated max_duration reason; got {events:?}"
    );
}

/// `--yes` is retired (ADR-0207 §11): it must fail loudly with a pointer to
/// its replacement, not silently parse-and-ignore, panic, or fall through to
/// clap's own unknown-flag error. Guards the `main.rs` wiring the in-crate
/// unit tests don't reach — mirrors `provider_selection.rs`'s own
/// binary-level pattern for a clean, non-panicking CLI exit.
#[test]
fn retired_yes_flag_errors_with_a_pointer_to_mode_auto() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_skutter"))
        .args(["run", "hi", "--yes"])
        .output()
        .expect("failed to spawn skutter");
    assert_eq!(out.status.code(), Some(2), "expected a clean exit code 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--mode auto"),
        "stderr should point to --mode auto, got: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "must not panic, got: {stderr}"
    );
}
