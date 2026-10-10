//! Integration tests for [`ToolExecutor::builder`] (#724): a bare builder
//! behaves exactly like the deprecated 4-arg `spawn_tool_executor` it
//! replaces, and a `.grants(..)` seam is actually wired through to the
//! dispatch path. The other seams have their own suites over the builder:
//! `.hooks` in `hooks.rs`, `.resolver`/custom `.grants` in `policy_seam.rs`,
//! `.escape_root` in `permission_dispatch.rs`, `.mcp_scopes` in `mcp_scoped.rs`.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, AgentCatalog, ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::policy::DefaultGrantStore;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::{Tool, ToolRegistry};

struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
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
                ..Default::default()
            });
        Ok(stream_from_response(resp))
    }
}

/// A `bash` stand-in, so grading runs against the real built-in `build`
/// mode's `bash(...)` rules without spawning a shell.
struct EchoBash;

#[async_trait]
impl Tool for EchoBash {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("bash")
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("ran: {input}"))
    }
}

fn bash_call(id: &str, command: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "bash".into(),
        input: serde_json::json!({ "command": command }).to_string(),
        provider_meta: None,
    }
}

/// An engine whose one model round issues `calls` as a batch, plus the
/// registry the executor dispatches against.
fn engine(calls: Vec<ToolCall>) -> (Holly, ToolRegistry) {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let specs = reg.specs();
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm {
                responses: Mutex::new(vec![LlmResponse {
                    tool_calls: calls.clone(),
                    ..Default::default()
                }]),
            }) as Box<dyn Llm>
        }),
        tool_specs: specs,
        ..EngineConfig::default()
    });
    (holly, reg)
}

/// The tool-facing outcome of one turn: each `ToolRequest`'s tool and each
/// `ToolOutput`'s `(call id, output, is_error)`, sorted so batch completion
/// order can't make two equivalent runs differ.
async fn outcome(holly: &Holly, sid: &SessionId) -> Vec<String> {
    let mut sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut out = Vec::new();
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
        match ev {
            OutEvent::ToolRequest { tool, .. } => out.push(format!("request {tool}")),
            OutEvent::ToolOutput {
                request_id,
                output,
                is_error,
                ..
            } => out.push(format!("output {request_id} {is_error} {output}")),
            OutEvent::Done { session, .. } if &session == sid => break,
            _ => {}
        }
    }
    out.sort();
    out
}

/// Built-in `build` mode: `ls` is class-allowed, `rm -rf /*` is on the
/// absolute deny list — one call of each grade that never parks.
fn allow_and_deny() -> Vec<ToolCall> {
    vec![bash_call("a", "ls"), bash_call("d", "rm -rf /tmp")]
}

#[tokio::test]
async fn bare_builder_matches_the_deprecated_four_arg_constructor() {
    let sid = SessionId::new("s1");

    let (holly, reg) = engine(allow_and_deny());
    #[allow(deprecated)]
    let _old = entanglement_runtime::tool_runner::spawn_tool_executor(
        &holly,
        reg,
        AgentCatalog::new(),
        PermissionProfile::new(Permission::Allow),
    );
    let old = outcome(&holly, &sid).await;

    let (holly, reg) = engine(allow_and_deny());
    let _new = ToolExecutor::builder(&holly, reg.shared()).spawn();
    let new = outcome(&holly, &sid).await;

    assert_eq!(old, new, "builder defaults must equal the old 4-arg path");
    assert!(
        new.iter().any(|o| o.starts_with("output a false ran:")),
        "allowed call should run; got {new:?}"
    );
    assert!(
        new.iter()
            .any(|o| o.starts_with("output d true") && o.contains("denied")),
        "denied call should be refused; got {new:?}"
    );
}

#[tokio::test]
async fn grants_seam_persists_an_always_approval_to_the_given_file() {
    let dir = std::env::temp_dir().join(format!("entanglement-724-grants-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("grants.yml");
    let _ = std::fs::remove_file(&file);

    // `git push*` is on `build`'s prompt list, so it parks an approval.
    let (holly, reg) = engine(vec![bash_call("p", "git push origin")]);
    let _exec = ToolExecutor::builder(&holly, reg.shared())
        .grants(Arc::new(DefaultGrantStore::load_from(Some(file.clone()))))
        .spawn();

    let sid = SessionId::new("s1");
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let mut parked = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(3), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            parked = true;
            break;
        }
    }
    assert!(parked, "`git push` should park an approval under build");
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "p".into(),
            scope: ApprovalScope::Always,
            mode: None,
        })
        .await
        .unwrap();

    let mut persisted = String::new();
    for _ in 0..60 {
        persisted = std::fs::read_to_string(&file).unwrap_or_default();
        if persisted.contains("git push origin") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        persisted.contains("git push origin"),
        "Always grant should land in the wired DefaultGrantStore's file; got {persisted:?}"
    );
}
