//! Integration tests: pre-dispatch argument validation (#560, ADR-0196 §6)
//! wired through the real engine — the three-way error taxonomy
//! (`tool_search.md` §7) plus its guards, exercised at the `ToolExec`
//! dispatch level rather than as unit tests of `arg_validate::validate`
//! alone (those live in `entanglement-runtime/src/arg_validate/tests.rs`).

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use entanglement_runtime::{Tool, ToolRegistry};

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
        let resp = {
            let mut responses = self.responses.lock().unwrap();
            responses.pop().unwrap_or_else(|| LlmResponse {
                text: "ok".into(),
                tool_calls: vec![],
            })
        };
        Ok(stream_from_response(resp))
    }
}

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

fn tool_outputs(events: &[OutEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// A tool requiring a `path` string — the schema-violation cases exercise it.
struct Greet;
#[async_trait]
impl Tool for Greet {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("greet")
    }
    fn description(&self) -> &str {
        "greet the file at path"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("hello: {input}"))
    }
}

/// A schema-clean tool that always fails at runtime — a parameter error
/// (file not found), never a schema violation.
struct AlwaysFails;
#[async_trait]
impl Tool for AlwaysFails {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("always_fails")
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn run(&self, _input: &str) -> anyhow::Result<String> {
        anyhow::bail!("boom: file not found")
    }
}

/// Run `calls` (one `ToolCall` per scripted LLM turn) against a registry
/// carrying `Greet` + `AlwaysFails` under the `build` profile (default-allow,
/// #560's kernel tools all run this way) — returns every `ToolOutput` in call
/// order.
async fn run_calls(calls: Vec<(&str, &str, &str)>) -> Vec<(String, bool)> {
    let responses: Vec<LlmResponse> = calls
        .iter()
        .map(|(id, name, input)| LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                input: (*input).into(),
                provider_meta: None,
            }],
        })
        .chain(std::iter::once(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
        .collect();
    let scripted = Arc::new(responses);
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(Greet);
    reg.register(AlwaysFails);
    let _executor = spawn_tool_executor(
        &holly,
        reg,
        profiles,
        PermissionProfile::new(Permission::Allow),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    tool_outputs(&collect(sub, &sid).await)
}

#[tokio::test]
async fn missing_required_param_gets_schema_and_example_decline() {
    let outputs = run_calls(vec![("t1", "greet", "{}")]).await;
    let (output, is_error) = &outputs[0];
    assert!(*is_error, "schema violation must set is_error");
    assert!(output.contains("missing required: path"), "{output}");
    assert!(output.contains("correct usage"), "{output}");
    assert!(output.contains("\"name\": \"greet\""), "{output}");
    assert!(output.contains("example call"), "{output}");
}

#[tokio::test]
async fn unexpected_param_gets_closest_match_hint_decline() {
    let outputs = run_calls(vec![("t1", "greet", r#"{"path":"a.rs","pathh":"b"}"#)]).await;
    let (output, is_error) = &outputs[0];
    assert!(*is_error);
    assert!(
        output.contains("unexpected: pathh — did you mean `path`?"),
        "{output}"
    );
}

#[tokio::test]
async fn repeat_violation_omits_the_full_schema() {
    let outputs = run_calls(vec![
        ("t1", "greet", "{}"),
        ("t2", "greet", r#"{"extra":1}"#),
    ])
    .await;
    let (first, _) = &outputs[0];
    assert!(first.contains("correct usage"), "{first}");
    let (second, is_error) = &outputs[1];
    assert!(*is_error);
    assert!(
        second.contains("already provided above"),
        "second violation should point back instead of resending: {second}"
    );
    assert!(
        !second.contains("correct usage"),
        "second violation must not resend the schema: {second}"
    );
}

#[tokio::test]
async fn loop_breaker_notes_the_second_identical_schema_violation() {
    let outputs = run_calls(vec![("t1", "greet", "{}"), ("t2", "greet", "{}")]).await;
    let (first, _) = &outputs[0];
    assert!(
        !first.contains("same call failed twice"),
        "first failure isn't a repeat: {first}"
    );
    let (second, is_error) = &outputs[1];
    assert!(*is_error);
    assert!(
        second.contains("same call failed twice — the schema is not the problem"),
        "{second}"
    );
}

#[tokio::test]
async fn loop_breaker_fires_on_repeated_runtime_failures_too() {
    // Two identical calls to a schema-clean tool that fails at runtime both
    // times — the loop-breaker guard is generic across failure kinds, not
    // only schema violations.
    let outputs = run_calls(vec![
        ("t1", "always_fails", "{}"),
        ("t2", "always_fails", "{}"),
    ])
    .await;
    let (first, is_error1) = &outputs[0];
    assert!(*is_error1);
    assert!(first.contains("boom: file not found"), "{first}");
    assert!(!first.contains("same call failed twice"), "{first}");
    let (second, is_error2) = &outputs[1];
    assert!(*is_error2);
    assert!(second.contains("boom: file not found"), "{second}");
    assert!(second.contains("same call failed twice"), "{second}");
}

#[tokio::test]
async fn runtime_failure_is_returned_verbatim_with_nothing_appended() {
    let outputs = run_calls(vec![("t1", "always_fails", "{}")]).await;
    let (output, is_error) = &outputs[0];
    assert!(*is_error);
    assert_eq!(output, "tool `always_fails` failed: boom: file not found");
}

#[tokio::test]
async fn valid_call_is_unaffected_by_validation() {
    let outputs = run_calls(vec![("t1", "greet", r#"{"path":"a.rs"}"#)]).await;
    let (output, is_error) = &outputs[0];
    assert!(!is_error);
    assert_eq!(output, "hello: {\"path\":\"a.rs\"}");
}

/// Decision 4: a user's `Reject` carries only the denial reason — never a
/// schema, even for a tool whose schema requires a parameter.
#[tokio::test]
async fn user_denial_carries_only_the_reason_no_schema() {
    // A single mode named `"build"` (matching `DEFAULT_MODE`, so no `SetMode`
    // call is needed) with `default: Ask` — the mode-based analog of the old
    // `askgreet` `AgentProfile` fixture (ADR-0207 stage 4 grades from the
    // session's mode, not its agent).
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let mode = entanglement_runtime::mode::Mode {
        name: "build".to_string(),
        default: Permission::Ask,
        rules: entanglement_runtime::mode::Rules::default(),
        limits: entanglement_runtime::mode::Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    let mode_table = Arc::new(
        entanglement_runtime::mode::ModeTable::new(vec![mode]).expect("single-mode table is valid"),
    );
    let call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "t1".into(),
            name: "greet".into(),
            input: "{}".into(),
            provider_meta: None,
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call, finish]);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        profiles: profiles.clone(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut reg = ToolRegistry::new();
    reg.register(Greet);
    let shared_tools = reg.shared();
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let resolver: Arc<dyn entanglement_runtime::policy::PermissionResolver> =
        Arc::new(entanglement_runtime::policy::ProfileResolver::new(
            perm_modes.clone(),
            mode_table.clone(),
            shared_tools.clone(),
            PermissionProfile::new(Permission::Allow),
            None,
        ));
    let grants: Arc<dyn entanglement_runtime::policy::GrantStore> =
        Arc::new(entanglement_runtime::policy::DefaultGrantStore::load());
    let _executor = entanglement_runtime::tool_runner::spawn_tool_executor_with_policy(
        &holly,
        shared_tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(std::sync::RwLock::new(profiles)),
        Arc::new(std::sync::RwLock::new(Arc::new(
            entanglement_runtime::skills::SkillRegistry::default(),
        ))),
        PermissionProfile::new(Permission::Allow),
        active,
        perm_modes,
        resolver,
        grants,
        Default::default(),
        None,
        mode_table,
        Arc::new(entanglement_runtime::plan_files::PlanFileRegistry::new()),
        None,
        None,
        None,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { .. }) {
            break;
        }
    }
    holly
        .send(InMsg::Reject {
            session: sid.clone(),
            request_id: "t1".into(),
            reason: Some("not now".into()),
        })
        .await
        .unwrap();

    let outputs = tool_outputs(&collect(sub, &sid).await);
    let (output, is_error) = &outputs[0];
    assert!(*is_error);
    assert_eq!(output, "tool `greet` rejected: not now");
    assert!(!output.contains("correct usage"), "{output}");
    assert!(!output.contains("schema"), "{output}");
}
