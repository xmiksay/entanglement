//! Integration tests for `InMsg::ListPending` (#726): a reconnecting head
//! asks which tool calls are parked awaiting approval instead of rebuilding
//! that from logs. Queries go over `send_from_wire` to also prove the frame is
//! wire-allowed end to end.

use std::borrow::Cow;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, ApprovalKind, ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, MessageRole, OutEvent, PendingApproval, Permission, PermissionProfile,
    SessionId, ToolCall,
};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{GrantStore, ModeResolver, PermissionResolver};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_runner::spawn_tool_executor_with_policy;
use entanglement_runtime::{Tool, ToolRegistry};
use tokio::sync::broadcast::Receiver;

use crate::mode_support::perm_modes;

const WAIT: Duration = Duration::from_secs(5);

fn reply(text: &str, call: Option<(&str, &str, &str)>) -> LlmStream {
    stream_from_response(LlmResponse {
        text: text.into(),
        tool_calls: call
            .map(|(id, name, input)| ToolCall {
                id: id.into(),
                name: name.into(),
                input: input.into(),
                provider_meta: None,
            })
            .into_iter()
            .collect(),
        ..Default::default()
    })
}

/// A root prompted `"direct"` calls `bash` itself; one prompted `"spawn"`
/// launches a background child whose prompt `"child-task"` calls `bash`.
/// Either ends its turn once it has seen a tool result.
struct PendingLlm;

#[async_trait]
impl Llm for PendingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        if req.messages.iter().any(|m| m.role == MessageRole::Tool) {
            return Ok(reply("done", None));
        }
        let asked = |s: &str| {
            req.messages
                .iter()
                .any(|m| m.role == MessageRole::User && m.text().contains(s))
        };
        Ok(if asked("spawn") {
            let input = r#"{"agent":"general","prompt":"child-task","background":true}"#;
            reply("", Some(("a1", "agent", input)))
        } else {
            reply("", Some(("b1", "bash", r#"{"command":"ls"}"#)))
        })
    }
}

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

/// No grants, nothing persisted — keeps the test off the real config dir.
struct NoGrants;
#[async_trait]
impl GrantStore for NoGrants {
    fn is_granted(&self, _: &SessionId, _: &str, _: Option<&str>, _: &str) -> bool {
        false
    }
    async fn record(&self, _: &SessionId, _: &str, _: Option<&str>, _: ApprovalScope, _: &str) {}
    fn forget_session(&self, _: &SessionId) {}
}

/// An engine whose single `build` mode grades every graded tool `Ask`.
fn spawn_engine() -> Holly {
    let agents =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(|| Box::new(PendingLlm) as Box<dyn Llm>),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    let mode_table = Arc::new(
        ModeTable::new(vec![Mode {
            name: "build".to_string(),
            default: Permission::Ask,
            rules: Rules::default(),
            limits: Limits::default(),
            sandbox: None,
            sandbox_network: false,
        }])
        .expect("single-mode table is valid"),
    );
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    let tools = reg.shared();
    let modes = perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        mode_table.clone(),
        tools.clone(),
        PermissionProfile::new(Permission::Allow),
        None,
    ));
    let _executor = spawn_tool_executor_with_policy(
        &holly,
        tools,
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(agents)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        PermissionProfile::new(Permission::Allow),
        Default::default(),
        modes,
        resolver,
        Arc::new(NoGrants),
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

/// Wait for the next `bash` `ToolRequest`, returning `(session, request_id, input)`.
async fn next_bash_request(sub: &mut Receiver<OutEvent>) -> (SessionId, String, String) {
    while let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await {
        if let OutEvent::ToolRequest {
            session,
            request_id,
            tool,
            input,
            ..
        } = ev
        {
            if tool == "bash" {
                return (session, request_id, input);
            }
        }
    }
    panic!("expected a bash ToolRequest");
}

/// Send `ListPending` over the untrusted wire path and await its reply.
async fn list(
    holly: &Holly,
    sub: &mut Receiver<OutEvent>,
    correlation: &str,
    root: Option<&SessionId>,
) -> Vec<PendingApproval> {
    holly
        .send_from_wire(InMsg::ListPending {
            correlation_id: correlation.into(),
            root: root.cloned(),
        })
        .await
        .expect("ListPending is wire-allowed");
    while let Ok(Ok(ev)) = tokio::time::timeout(WAIT, sub.recv()).await {
        if let OutEvent::PendingList {
            correlation_id,
            approvals,
        } = ev
        {
            if correlation_id == correlation {
                return approvals;
            }
        }
    }
    panic!("expected a PendingList reply for {correlation}");
}

#[tokio::test]
async fn list_pending_is_empty_when_nothing_is_parked() {
    let holly = spawn_engine();
    let mut sub = holly.subscribe();
    assert!(list(&holly, &mut sub, "c-empty", None).await.is_empty());
}

#[tokio::test]
async fn parked_ask_is_listed_then_gone_once_approved() {
    let holly = spawn_engine();
    let mut sub = holly.subscribe();
    let sid = SessionId::new("direct-root");
    holly
        .send(InMsg::prompt(sid.clone(), "direct"))
        .await
        .unwrap();
    let (session, request_id, input) = next_bash_request(&mut sub).await;
    assert_eq!(session, sid);

    let listed = list(&holly, &mut sub, "c1", Some(&sid)).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    let p = &listed[0];
    assert_eq!((&p.session, &p.request_id), (&sid, &request_id));
    assert_eq!(p.kind, ApprovalKind::Tool);
    assert_eq!(p.tool, "bash");
    // Exactly the prompt the head would have seen on the ToolRequest.
    assert_eq!(p.input, input);
    assert!(p.reason.contains("mode `build`"), "{}", p.reason);
    assert_eq!(
        p.scopes,
        vec![
            ApprovalScope::Once,
            ApprovalScope::Session,
            ApprovalScope::Always
        ]
    );
    assert!(p.since_ms > 0);

    // A root outside this tree sees nothing.
    let other = SessionId::new("unrelated");
    assert!(list(&holly, &mut sub, "c2", Some(&other)).await.is_empty());

    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id,
            scope: ApprovalScope::Once,
            mode: None,
        })
        .await
        .unwrap();
    // The router handles Approve and ListPending in order on one loop, so
    // the very next listing already reflects the resolution.
    assert!(list(&holly, &mut sub, "c3", None).await.is_empty());
}

#[tokio::test]
async fn root_filter_covers_a_spawned_childs_parked_call() {
    let holly = spawn_engine();
    let mut sub = holly.subscribe();
    let parent = SessionId::new("spawn-root");
    holly
        .send(InMsg::prompt(parent.clone(), "spawn"))
        .await
        .unwrap();
    let (child, request_id, _) = next_bash_request(&mut sub).await;
    assert_ne!(child, parent, "the bash call comes from the child");

    let listed = list(&holly, &mut sub, "c-tree", Some(&parent)).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(
        (&listed[0].session, &listed[0].request_id),
        (&child, &request_id)
    );

    let unrelated = SessionId::new("unrelated");
    assert!(list(&holly, &mut sub, "c-none", Some(&unrelated))
        .await
        .is_empty());
}
