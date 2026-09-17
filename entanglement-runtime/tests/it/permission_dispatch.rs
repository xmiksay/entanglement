//! Integration tests for permission dispatch (#59). Core emits a `ToolExec`
//! for every host tool; `spawn_tool_executor`/`spawn_tool_executor_with_policy`
//! resolve `Allow | Ask | Deny` from the session's permission **mode**
//! (ADR-0207 stage 4 — `ModeResolver` grades from `crate::mode::Mode`, not
//! `Agent`) and drive the approval round-trip on `Ask`.
//!
//! Every fixture below is a single-mode [`entanglement_runtime::mode::ModeTable`]
//! named `"build"` — matching `entanglement_core::DEFAULT_MODE`, so a fresh
//! session resolves against it with no `SetMode` needed, mirroring how this
//! file used to wire a single custom `Agent` and `SetAgent` to it
//! before ADR-0207 moved permission off the agent. The curated-read-only
//! section near the bottom is the one exception: it exercises the real
//! built-in `research` mode via `ModeTable::builtin()` + `InMsg::SetMode`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_runner::spawn_tool_executor_with_policy;
use entanglement_runtime::{Tool, ToolRegistry};

/// An LLM that replays scripted responses in order, then plain text (so a turn
/// loop that re-prompts after a tool call terminates).
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

/// A trivial host tool named `bash`. Declares `Capability::Exec` explicitly
/// — the `Tool::capabilities` default is `Write` (fail-closed for an
/// un-annotated tool, ADR-0207 §3), which would silently mis-grade every
/// bare-`exec`/`write`-class mode rule this file's `research`-mode tests
/// exercise.
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

/// Build a single-mode table named `"build"` (matching `DEFAULT_MODE`, so a
/// fresh session resolves against it without `SetMode`) from a `default`
/// grade plus `deny`/`allow` rule lists — the `Mode`-based analog of this
/// file's old per-test `Agent` fixtures.
fn one_mode_table(default: Permission, deny: &[&str], allow: &[&str]) -> Arc<ModeTable> {
    let mode = Mode {
        name: "build".to_string(),
        default,
        rules: Rules::from_lists(
            &deny.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &allow.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &[],
        ),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"))
}

fn allow_all_table() -> Arc<ModeTable> {
    one_mode_table(Permission::Allow, &[], &[])
}

/// A mode with no allow/deny rules and `default: Ask` — none of the built-in
/// four modes default to `Ask` on every tool (research/plan/build default to
/// `prompt` but carry rules, auto defaults to `deny`), so the plain-`Ask`
/// dispatch path needs a dedicated fixture.
fn ask_mode_table() -> Arc<ModeTable> {
    one_mode_table(Permission::Ask, &[], &[])
}

fn deny_mode_table() -> Arc<ModeTable> {
    one_mode_table(Permission::Deny, &[], &[])
}

/// A mode that grades `bash` by its command (#173): `git *` runs outright,
/// `rm *` is denied, anything else asks.
fn scoped_bash_mode_table() -> Arc<ModeTable> {
    one_mode_table(Permission::Ask, &["bash(rm *)"], &["bash(git *)"])
}

/// A mode that grades `bash` by its `workdir` (#425): `/tmp/*` runs outright,
/// `/etc/*` is denied, anything else asks.
fn scoped_workdir_mode_table() -> Arc<ModeTable> {
    one_mode_table(Permission::Ask, &["bash{/etc/*}"], &["bash{/tmp/*}"])
}

/// A mode that grades `read` by an arg-scoped rule authored root-relative
/// (#173): `src/*` runs outright, anything else asks.
fn scoped_read_mode_table() -> Arc<ModeTable> {
    one_mode_table(Permission::Ask, &[], &["read(src/*)"])
}

fn perm_modes() -> Arc<Mutex<HashMap<SessionId, String>>> {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Collect events for `sid` until `Done`, with a safety timeout.
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

/// Build a Holly whose scripted LLM calls `bash` once, plus a registry with the
/// `EchoBash` tool and the runtime tool executor graded from `mode_table`.
fn spawn_with_bash_call_using(input: &str, mode_table: Arc<ModeTable>) -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: input.into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    spawn_with_policy_over(&holly, reg, profiles, mode_table, None);
    holly
}

/// Wire the built-in `build` profile, allow-all mode (mirrors the old
/// `build` agent's `default: allow`).
fn spawn_with_bash_call(input: &str) -> Holly {
    spawn_with_bash_call_using(input, allow_all_table())
}

/// Shared plumbing every harness in this file uses: registers `reg` as the
/// shared tool registry, wires a `ModeResolver` graded from `mode_table`
/// (allow-all config ceiling), and spawns the real executor via
/// `spawn_tool_executor_with_policy`. `root` (#485, ADR-0125) is threaded
/// into both the resolver and (when `Some`) the executor's escape-root
/// policy, matching `main.rs`'s production wiring.
fn spawn_with_policy_over(
    holly: &Holly,
    reg: ToolRegistry,
    agents: entanglement_core::AgentCatalog,
    mode_table: Arc<ModeTable>,
    root: Option<&Path>,
) {
    let shared_tools = reg.shared();
    let active = Arc::new(Mutex::new(HashMap::new()));
    let modes = perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        mode_table.clone(),
        shared_tools.clone(),
        PermissionProfile::new(Permission::Allow),
        root.map(Path::to_path_buf),
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let escape_root = root.map(|root| entanglement_runtime::tool_runner::EscapeRoot {
        root: root.to_path_buf(),
        store: Arc::new(entanglement_runtime::extra_roots::ExtraRootStore::ephemeral()),
    });
    let _executor = spawn_tool_executor_with_policy(
        holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(agents)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
        active,
        modes,
        resolver,
        grants,
        Default::default(),
        escape_root,
        mode_table,
        Arc::new(PlanFileRegistry::new()),
        // No per-user MCP scopes (#684) — single-user.
        None,
        // No tool-advertising inputs (ADR-0196) — resolves tool_search.
        None,
        None,
    );
}

#[tokio::test]
async fn allow_runs_without_approval() {
    // Allow-all mode: bash runs directly, no ToolRequest.
    let holly = spawn_with_bash_call("echo hi");
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "Allow must not ask for approval"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: echo hi")),
        "Allow should run the tool; got {events:?}"
    );
    // #636/ADR-0176: a cleared tool's structured side channel says so — no
    // parsing `output` for a `[exit N]`/failure marker required.
    let out = events
        .iter()
        .find(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: echo hi"))
        .unwrap();
    match out {
        OutEvent::ToolOutput {
            is_error,
            duration_ms,
            ..
        } => {
            assert!(!is_error, "a cleared tool run must not be is_error");
            assert!(duration_ms.is_some(), "duration_ms must be measured");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn deny_refuses_without_request() {
    // A `default: deny` mode exercises the `Deny` dispatch path.
    let holly = spawn_with_bash_call_using("rm -rf", deny_mode_table());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "rm")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "no approval expected on deny"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "Deny should report a denial; got {events:?}"
    );
    // #636/ADR-0176: the denial is now also a real field, not just the
    // `` denied by permission profile `` marker in `output`.
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput {
                output,
                is_error: true,
                ..
            } if output.contains("denied")
        )),
        "Deny must set is_error; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "Deny must not run the tool"
    );
}

#[tokio::test]
async fn ask_emits_request_then_runs_on_approve() {
    // `Ask`-default mode: approve after the request; the tool then runs.
    let holly = spawn_with_bash_call_using("ls", ask_mode_table());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(got_request, "expected a ToolRequest under the Ask mode");

    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: Default::default(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: ls")));
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn argument_scoped_allow_runs_matching_command_without_approval() {
    // `git status` matches `bash(git *): allow` → runs directly, no ToolRequest.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "git status" }).to_string(),
        scoped_bash_mode_table(),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "an argument-scoped Allow must not ask for approval; got {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("git status"))
        ),
        "the matching command should run; got {events:?}"
    );
}

#[tokio::test]
async fn argument_scoped_deny_blocks_matching_command() {
    // `rm -rf /` matches `bash(rm *): deny` → refused, never runs.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "rm -rf /" }).to_string(),
        scoped_bash_mode_table(),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "rm")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "the matching command should be denied; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "a denied command must not run"
    );
}

#[tokio::test]
async fn argument_scoped_falls_through_to_coarse_ask() {
    // `ls` matches neither refined rule → the coarse `bash: ask` grade applies.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "ls -la" }).to_string(),
        scoped_bash_mode_table(),
    );
    let sid = SessionId::new("s1");
    let mut watch = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "list"))
        .await
        .unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "a command matching no refined rule should fall through to the coarse Ask"
    );
}

#[tokio::test]
async fn workdir_scoped_allow_runs_matching_workdir_without_approval() {
    // A `bash` call under `/tmp` matches `bash{/tmp/*}: allow` → runs directly.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "ls", "workdir": "/tmp/scratch" }).to_string(),
        scoped_workdir_mode_table(),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a workdir-scoped Allow must not ask for approval; got {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("/tmp/scratch"))
        ),
        "the matching call should run; got {events:?}"
    );
}

#[tokio::test]
async fn workdir_scoped_deny_blocks_matching_workdir() {
    // A `bash` call under `/etc` matches `bash{/etc/*}: deny` → refused.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "ls", "workdir": "/etc/cron.d" }).to_string(),
        scoped_workdir_mode_table(),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "the matching call should be denied; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "a denied call must not run"
    );
}

#[tokio::test]
async fn workdir_scoped_falls_through_to_coarse_ask_outside_every_pattern() {
    // A `bash` call under neither scoped workdir falls to the coarse `ask`.
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "ls", "workdir": "/home/x" }).to_string(),
        scoped_workdir_mode_table(),
    );
    let sid = SessionId::new("s1");
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "a workdir matching no scoped rule should fall through to the coarse Ask"
    );
}

#[tokio::test]
async fn ask_rejected_reports_rejection() {
    // `Ask`-default mode: reject the request; the tool never runs.
    let holly = spawn_with_bash_call_using("ls", ask_mode_table());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();

    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { .. }) {
            break;
        }
    }
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id: "t1".into(),
            reason: Some("nope".into()),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("rejected") && output.contains("nope"))
        ),
        "reject should surface a rejection with the reason; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "reject must not run the tool"
    );
}

/// Spawn a Holly whose scripted LLM calls `bash` once per turn (ids `t1`, `t2`)
/// with the given `command`, so two prompts drive two identical calls. Wired to
/// an `Ask`-default mode so both calls would prompt absent a grant.
fn spawn_two_ask_bash_calls(command: &str) -> Holly {
    let call = |id: &str| LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": command }).to_string(),
            provider_meta: None,
        }],
    };
    let ok = || LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call("t1"), ok(), call("t2"), ok()]);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    spawn_with_policy_over(&holly, reg, profiles, ask_mode_table(), None);
    holly
}

/// A hallucinated tool name must never reach the `Ask` approval ladder (#437):
/// the registry miss is now discovered *before* permission resolution, so an
/// unknown-tool call gets an immediate `ToolOutput` — no `ToolRequest`, no wait
/// for a human — even under a mode that would otherwise ask for `bash`.
#[tokio::test]
async fn unknown_tool_is_rejected_before_the_permission_ladder() {
    let call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "bsah".into(),
            input: "{}".into(),
            provider_meta: None,
        }],
    };
    let ok = LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call, ok]);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    spawn_with_policy_over(&holly, reg, profiles, ask_mode_table(), None);

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "an unknown tool must never reach the Ask approval prompt; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. }
            if output.contains("unknown tool") && output.contains("did you mean `bash`"))),
        "expected an immediate unknown-tool reply with a closest-match hint; got {events:?}"
    );
    // #636/ADR-0176: unknown-tool is a structural failure — is_error must say so.
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput {
                output,
                is_error: true,
                ..
            } if output.contains("unknown tool")
        )),
        "unknown tool must set is_error; got {events:?}"
    );
}

/// An `Approve { scope: Session }` (#174) records an in-memory grant, so the next
/// *identical* call in the same session runs without a second `ToolRequest`.
#[tokio::test]
async fn session_grant_skips_the_second_prompt() {
    let holly = spawn_two_ask_bash_calls("ls");
    let sid = SessionId::new("s1");

    // Turn 1: the Ask prompts; approve it for the session.
    let sub1 = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            asked = true;
            break;
        }
    }
    assert!(asked, "turn 1 should prompt for approval");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: entanglement_core::ApprovalScope::Session,
        })
        .await
        .unwrap();
    let turn1 = collect(sub1, &sid).await;
    assert!(
        turn1
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("ls"))),
        "turn 1 should run the approved command; got {turn1:?}"
    );

    // Turn 2: the identical call must NOT prompt again — the session grant runs it.
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "run again"))
        .await
        .unwrap();
    let turn2 = collect(sub2, &sid).await;
    assert!(
        !turn2
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a session-granted call must not ask again; got {turn2:?}"
    );
    assert!(
        turn2
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("ls"))),
        "turn 2 should still run the command; got {turn2:?}"
    );
}

// --- #485, ADR-0125: absolute-inside-root path args grade like the relative
// spelling ------------------------------------------------------------------

/// A trivial `read` host tool, standing in for the real filesystem tool — this
/// module only exercises permission grading, never actual file I/O.
struct EchoRead;
#[async_trait]
impl Tool for EchoRead {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("read")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
    fn capabilities(&self) -> &'static [entanglement_runtime::capability::Capability] {
        &[entanglement_runtime::capability::Capability::Read]
    }
}

/// Build a Holly whose scripted LLM calls `read` twice — `input1` (id `t1`)
/// then `input2` (id `t2`) — wired to `mode_table` through a `ModeResolver`
/// with `root` set (#485, ADR-0125), mirroring `main.rs`'s production wiring.
fn spawn_two_read_calls_rooted(
    root: &Path,
    input1: &str,
    input2: &str,
    mode_table: Arc<ModeTable>,
) -> Holly {
    let call = |id: &str, input: &str| LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "read".into(),
            input: input.into(),
            provider_meta: None,
        }],
    };
    let ok = || LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call("t1", input1), ok(), call("t2", input2), ok()]);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoRead);
    spawn_with_policy_over(&holly, reg, profiles, mode_table, Some(root));
    holly
}

/// (a) An absolute path resolving inside root must match the same
/// root-relative arg-scoped rule its relative spelling matches — the bug: the
/// verbatim `/root/src/main.rs` used to fall through `read(src/*)` to the
/// coarse `ask` default, prompting for a call the relative spelling would run
/// outright.
#[tokio::test]
async fn absolute_inside_root_read_matches_the_relative_scoped_rule() {
    let root = Path::new("/home/user/project");
    let holly = spawn_two_read_calls_rooted(
        root,
        &serde_json::json!({ "path": "/home/user/project/src/main.rs" }).to_string(),
        &serde_json::json!({ "path": "/home/user/project/src/main.rs" }).to_string(),
        scoped_read_mode_table(),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "an absolute in-root path matching a root-relative rule must not ask; got {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("main.rs"))
        ),
        "the matching call should run; got {events:?}"
    );
}

/// (b) Grant-key stability: a Session grant recorded against the relative
/// spelling of a call must also cover the absolute spelling of the identical
/// file — the bug: the two spellings keyed different grants, so the second
/// (absolute) call still prompted.
#[tokio::test]
async fn session_grant_on_relative_spelling_covers_the_absolute_spelling() {
    let root = Path::new("/home/user/project");
    // An `Ask`-default mode with no arg-scoped rule, so both calls would
    // prompt absent a grant, isolating this test from arg-scoped-rule
    // behavior.
    let holly = spawn_two_read_calls_rooted(
        root,
        &serde_json::json!({ "path": "src/main.rs" }).to_string(),
        &serde_json::json!({ "path": "/home/user/project/src/main.rs" }).to_string(),
        ask_mode_table(),
    );
    let sid = SessionId::new("s1");

    // Turn 1: the relative spelling prompts; approve it for the session.
    let sub1 = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "read") {
            asked = true;
            break;
        }
    }
    assert!(asked, "turn 1 (relative spelling) should prompt");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Session,
        })
        .await
        .unwrap();
    let turn1 = collect(sub1, &sid).await;
    assert!(
        turn1.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("main.rs"))
        ),
        "turn 1 should run the approved read; got {turn1:?}"
    );

    // Turn 2: the absolute spelling of the SAME file must NOT prompt again.
    let sub2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "run again"))
        .await
        .unwrap();
    let turn2 = collect(sub2, &sid).await;
    assert!(
        !turn2
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "the absolute spelling of an already-granted file must not ask again; got {turn2:?}"
    );
    assert!(
        turn2.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("main.rs"))
        ),
        "turn 2 should still run the read; got {turn2:?}"
    );
}

/// (c) An absolute path resolving OUTSIDE root must stay verbatim and
/// therefore keep asking — a root-relative rule matching an outside path
/// would be a privilege escalation, not a convenience.
#[tokio::test]
async fn absolute_outside_root_still_prompts() {
    let root = Path::new("/home/user/project");
    let holly = spawn_two_read_calls_rooted(
        root,
        &serde_json::json!({ "path": "/etc/passwd" }).to_string(),
        &serde_json::json!({ "path": "/etc/passwd" }).to_string(),
        scoped_read_mode_table(),
    );
    let sid = SessionId::new("s1");
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "read") {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "an out-of-root absolute path must not silently match a root-relative rule"
    );
}

// --- #486, ADR-0126: ApprovalScope::SessionDir --------------------------

/// A trivial host tool that just echoes its input, parameterized by name —
/// standing in for `read`/`grep`/`glob`/`edit` in the `SessionDir` tests
/// below; this module only exercises permission grading, never real file I/O.
struct EchoNamed(&'static str);
#[async_trait]
impl Tool for EchoNamed {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.0)
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran {}: {input}", self.0))
    }
    fn capabilities(&self) -> &'static [entanglement_runtime::capability::Capability] {
        use entanglement_runtime::capability::Capability;
        match self.0 {
            "read" | "grep" | "glob" => &[Capability::Read],
            _ => &[Capability::Write],
        }
    }
}

/// Build a Holly wired exactly like [`spawn_two_read_calls_rooted`] (a
/// `root`-aware `ModeResolver` + escape-root policy + `DefaultGrantStore`),
/// but scripted with an arbitrary `(id, tool, input)` call sequence, each
/// followed by a tool-less "ok" turn. Registers `read`/`grep`/`glob`/`edit`
/// `EchoNamed` tools — every tool the `SessionDir` tests below exercise. The
/// scripted queue is a single shared template: each *session* gets its own
/// fresh clone from `llm_factory` (starting at element 0), so a second,
/// independent session that sends exactly one prompt naturally replays this
/// list's *first* call — the deliberate mechanism the cross-session test
/// below relies on to re-ask the identical call a first session had granted.
fn spawn_scripted_calls_rooted(
    root: &Path,
    calls: &[(&str, &str, String)],
    mode_table: Arc<ModeTable>,
) -> Holly {
    let mut responses = Vec::new();
    for (id, tool, input) in calls {
        responses.push(LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: (*id).into(),
                name: (*tool).into(),
                input: input.clone(),
                provider_meta: None,
            }],
        });
        responses.push(LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        });
    }
    let scripted = Arc::new(responses);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoNamed("read"));
    reg.register(EchoNamed("grep"));
    reg.register(EchoNamed("glob"));
    reg.register(EchoNamed("edit"));
    spawn_with_policy_over(&holly, reg, profiles, mode_table, Some(root));
    holly
}

/// Approving a `read` call with `Approve { scope: SessionDir }` widens the
/// grant to every later `read`/`grep`/`glob` call whose path falls under the
/// approved call's directory — a sibling file, a nested subdirectory, and a
/// directory-rooted `grep`/`glob` all skip the prompt — but `edit` (not in the
/// read-only triad) still asks, and a second session never inherits the grant.
#[tokio::test]
async fn session_dir_grant_widens_the_read_only_triad_but_not_edit_or_other_sessions() {
    let root = Path::new("/home/user/project");
    let calls: Vec<(&str, &str, String)> = vec![
        (
            "t1",
            "read",
            serde_json::json!({ "path": "src/a.rs" }).to_string(),
        ),
        (
            "t2",
            "read",
            serde_json::json!({ "path": "src/b/c.rs" }).to_string(),
        ),
        (
            "t3",
            "grep",
            serde_json::json!({ "path": "src" }).to_string(),
        ),
        (
            "t4",
            "edit",
            serde_json::json!({ "path": "src/a.rs" }).to_string(),
        ),
    ];
    let holly = spawn_scripted_calls_rooted(root, &calls, ask_mode_table());
    let sid = SessionId::new("s1");

    // Turn 1: `read src/a.rs` prompts; approve it with SessionDir scope.
    let sub1 = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "read") {
            asked = true;
            break;
        }
    }
    assert!(asked, "turn 1 (read src/a.rs) should prompt");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::SessionDir,
        })
        .await
        .unwrap();
    let turn1 = collect(sub1, &sid).await;
    assert!(
        turn1
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("a.rs"))),
        "turn 1 should run the approved read; got {turn1:?}"
    );

    // Turn 2: `read src/b/c.rs` — a nested subdirectory under "src" — must not
    // prompt again.
    let sub2 = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let turn2 = collect(sub2, &sid).await;
    assert!(
        !turn2
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a read under the granted directory must not ask again; got {turn2:?}"
    );

    // Turn 3: `grep {path: "src"}` — the directory itself — must not prompt.
    let sub3 = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let turn3 = collect(sub3, &sid).await;
    assert!(
        !turn3
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a grep rooted at the granted directory must not ask again; got {turn3:?}"
    );

    // Turn 4: `edit src/a.rs` is NOT in the read-only triad — still prompts.
    let mut watch4 = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut edit_asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch4.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "edit") {
            edit_asked = true;
            break;
        }
    }
    assert!(
        edit_asked,
        "a SessionDir grant must never widen a mutation tool like edit"
    );

    // A second, independent session replays the same first call (`read
    // src/a.rs`, per `spawn_scripted_calls_rooted`'s doc) and must still be
    // asked — a directory grant never crosses sessions.
    let sid2 = SessionId::new("s2");
    let mut watch2 = holly.subscribe();
    holly.send(InMsg::prompt(sid2.clone(), "go")).await.unwrap();
    let mut other_session_asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch2.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { session, tool, .. } if session == &sid2 && tool == "read")
        {
            other_session_asked = true;
            break;
        }
    }
    assert!(
        other_session_asked,
        "a SessionDir grant must never be inherited by a different session"
    );
}

// --- ADR-0195 §3 / ADR-0207: curated read-only Allow rules in the built-in
// `research` mode --------------------------------------------------------

/// A trivial `call` host tool, mirroring `EchoBash` — the curated set reaches
/// both exec tools, so the dispatch path must be exercised for each.
struct EchoCall;
#[async_trait]
impl Tool for EchoCall {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("call")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
    fn capabilities(&self) -> &'static [entanglement_runtime::capability::Capability] {
        &[entanglement_runtime::capability::Capability::Exec]
    }
}

/// `bash`/`call` scripted once, wired to `mode_table` — used by the curated
/// read-only tests below with `ModeTable::builtin()`, so they exercise the
/// real `research` mode's allowlist rather than a fabricated fixture.
fn spawn_with_exec_tools_using(tool: &str, input: &str, mode_table: Arc<ModeTable>) -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: tool.into(),
                input: input.into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    reg.register(EchoCall);
    spawn_with_policy_over(&holly, reg, profiles, mode_table, None);
    holly
}

fn builtin_modes() -> Arc<ModeTable> {
    Arc::new(ModeTable::builtin().expect("built-in modes must parse"))
}

async fn set_research_mode(holly: &Holly, session: &SessionId) {
    holly
        .send(InMsg::SetMode {
            session: session.clone(),
            mode: "research".into(),
        })
        .await
        .unwrap();
}

/// ADR-0195/ADR-0207: `research` mode's curated read-only allowlist pre-
/// approves `bash find .`, so inspection no longer costs an approval
/// round-trip — while a non-curated command under the same mode still
/// escalates (the following tests).
#[tokio::test]
async fn curated_read_only_bash_find_runs_without_approval_under_research_mode() {
    let holly = spawn_with_exec_tools_using(
        "bash",
        &serde_json::json!({ "command": "find ." }).to_string(),
        builtin_modes(),
    );
    let sid = SessionId::new("s1");
    set_research_mode(&holly, &sid).await;
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "`bash find .` is curated read-only — no approval expected; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("find ."))),
        "the curated command should run; got {events:?}"
    );
}

#[tokio::test]
async fn curated_read_only_call_rg_runs_without_approval_under_research_mode() {
    let holly = spawn_with_exec_tools_using(
        "call",
        &serde_json::json!({ "command": "rg", "args": ["pattern", "src"] }).to_string(),
        builtin_modes(),
    );
    let sid = SessionId::new("s1");
    set_research_mode(&holly, &sid).await;
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "`call rg …` is curated read-only — no approval expected; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("rg"))),
        "the curated call should run; got {events:?}"
    );
}

/// A non-curated command under the same mode still escalates — the curated
/// set is exact-prefix, so `git status` (a read-only *operation* but not on
/// the list) keeps its `Ask`.
#[tokio::test]
async fn a_non_curated_command_still_escalates_under_research_mode() {
    let holly = spawn_with_exec_tools_using(
        "bash",
        &serde_json::json!({ "command": "git status" }).to_string(),
        builtin_modes(),
    );
    let sid = SessionId::new("s1");
    set_research_mode(&holly, &sid).await;
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "`git status` is not in the curated set — research mode must still ask"
    );
}

/// The config ceiling still clamps the curated rules down (#172): a `bash:
/// deny` ceiling turns a curated `bash find .` Allow into a refusal, exactly
/// as it clamps any mode's grade.
#[tokio::test]
async fn a_bash_deny_ceiling_clamps_the_curated_read_only_rules() {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({ "command": "find ." }).to_string(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    reg.register(EchoCall);
    let shared_tools = reg.shared();
    let active = Arc::new(Mutex::new(HashMap::new()));
    let modes = perm_modes();
    // The ceiling from a `permissions: bash: deny` config layer.
    let ceiling = PermissionProfile::new(Permission::Allow).with("bash", Permission::Deny);
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        builtin_modes(),
        shared_tools.clone(),
        ceiling.clone(),
        None,
    ));
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        ceiling,
        active,
        modes,
        resolver,
        Arc::new(DefaultGrantStore::load()),
        Default::default(),
        None,
        builtin_modes(),
        Arc::new(PlanFileRegistry::new()),
        None,
        None,
        None,
    );

    let sid = SessionId::new("s1");
    set_research_mode(&holly, &sid).await;
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "a Deny ceiling never prompts; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "the ceiling must clamp the curated Allow down to a refusal; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "the clamped command must not run"
    );
}

// --- ADR-0197: compound bash commands grade per segment ---------------------

/// A compound pipeline built entirely from curated read-only verbs runs with
/// no approval round-trip — the curated Allow rules grade each top-level
/// segment instead of only the whole raw string.
#[tokio::test]
async fn compound_pipeline_of_curated_verbs_runs_without_approval() {
    let holly = spawn_with_exec_tools_using(
        "bash",
        &serde_json::json!({ "command": "find . | grep x | wc -l" }).to_string(),
        builtin_modes(),
    );
    let sid = SessionId::new("s1");
    set_research_mode(&holly, &sid).await;
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "every segment of `find . | grep x | wc -l` is curated read-only — no approval expected; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("find ."))),
        "the pipeline should run; got {events:?}"
    );
}

/// The over-match regression this ADR closes: a trailing `*` on
/// `bash(find *)` must never authorize an `&&`-appended command it doesn't
/// cover.
#[tokio::test]
async fn compound_over_match_regression_still_escalates() {
    let holly = spawn_with_exec_tools_using(
        "bash",
        &serde_json::json!({ "command": "find . && rm -rf /tmp/x" }).to_string(),
        builtin_modes(),
    );
    let sid = SessionId::new("s1");
    set_research_mode(&holly, &sid).await;
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();

    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(
        got_request,
        "`find . && rm -rf /tmp/x` must not ride `bash(find *): allow` past the `&&`"
    );
}

/// A deny rule matching only the trailing segment of a compound still denies
/// the whole command — deny is never weakened by splitting.
#[tokio::test]
async fn compound_command_deny_on_trailing_segment_denies_via_dispatch() {
    let holly = spawn_with_bash_call_using(
        &serde_json::json!({ "command": "git status && rm x" }).to_string(),
        scoped_bash_mode_table(),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "the trailing `rm x` segment matches `bash(rm *): deny` — the whole \
         command must be denied; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "a denied compound must not run"
    );
}

/// Grants stay exact whole-string match (`GrantKey`, unchanged): approving
/// `ls` as a standalone command does not widen to a compound that merely
/// contains `ls` as one of its segments — that compound still asks (rule-
/// based per-segment Allow, not grant widening, is ADR-0197's fix for the
/// common case).
#[tokio::test]
async fn session_grant_does_not_widen_to_a_compound_containing_the_granted_segment() {
    let call = |id: &str, command: &str| LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": command }).to_string(),
            provider_meta: None,
        }],
    };
    let ok = || LlmResponse {
        text: "ok".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call("t1", "ls"), ok(), call("t2", "ls && pwd"), ok()]);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    spawn_with_policy_over(&holly, reg, profiles, ask_mode_table(), None);

    let sid = SessionId::new("s1");

    // Turn 1: approve the exact standalone command `ls` for the session.
    let sub1 = holly.subscribe();
    let mut watch1 = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();
    let mut asked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch1.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            asked = true;
            break;
        }
    }
    assert!(asked, "turn 1 should prompt for approval");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: entanglement_core::ApprovalScope::Session,
        })
        .await
        .unwrap();
    let _turn1 = collect(sub1, &sid).await;

    // Turn 2: `ls && pwd` contains the granted segment but is a different
    // whole string, and no rule covers either segment — must still ask.
    let mut watch2 = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "run again"))
        .await
        .unwrap();
    let mut asked_again = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch2.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            asked_again = true;
            break;
        }
    }
    assert!(
        asked_again,
        "a compound merely containing a granted segment must still ask"
    );
}

/// A trivial host tool named `mcp_enable`, echoing its input — stands in for
/// the real `McpEnableTool` (`entanglement_runtime::mcp::McpEnableTool`,
/// covered end-to-end by `mcp::available_tests`).
struct EchoMcpEnable;
#[async_trait]
impl Tool for EchoMcpEnable {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("mcp_enable")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("enabled: {input}"))
    }
}

/// A trivial host tool named like a namespaced MCP tool, standing in for a
/// server's real tool once connected.
struct EchoMcpSearch;
#[async_trait]
impl Tool for EchoMcpSearch {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("mcp__testserver__search")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("searched: {input}"))
    }
}

/// `mcp_enable` and a namespaced MCP tool both dispatch cleanly end to end
/// under an allow-all mode — a plain regression pin for the dispatch path
/// itself (unit coverage for `McpCapabilityIndex`/tier plumbing lives in
/// `entanglement_runtime::agents::mod::tests` and
/// `entanglement_runtime::mcp::mod::tests`). Retired since ADR-0207: this
/// used to also prove the old per-profile tool *mask* never blocked
/// `mcp_enable` even under the narrow `explore` profile — there is no mask
/// left to prove that about.
#[tokio::test]
async fn mcp_enable_and_a_namespaced_mcp_tool_run_under_an_allow_all_mode() {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "mcp_enable".into(),
                input: r#"{"server":"testserver"}"#.into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t2".into(),
                name: "mcp__testserver__search".into(),
                input: "{}".into(),
                provider_meta: None,
            }],
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
        },
    ]);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        agents: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(EchoMcpEnable);
    reg.register(EchoMcpSearch);
    spawn_with_policy_over(&holly, reg, profiles, allow_all_table(), None);

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "search the web"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "neither call should need approval under an allow-all mode; got {events:?}"
    );
    let outs: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();
    assert!(
        outs.iter().any(|o| o.starts_with("enabled:")),
        "mcp_enable must run; got {outs:?}"
    );
    assert!(
        outs.iter().any(|o| o.starts_with("searched:")),
        "the namespaced MCP tool must run; got {outs:?}"
    );
}
