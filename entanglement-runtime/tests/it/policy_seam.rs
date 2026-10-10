//! Integration tests for the pluggable policy seams (#311): a custom
//! [`PermissionResolver`] decides each call's `Allow | Ask | Deny` grade, and a
//! custom [`GrantStore`] intercepts an `ApprovalScope::Always` write — both
//! without forking the tool executor. The default CLI path (byte-identical
//! profile clamp + file grants) is covered by `permission_dispatch.rs`.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::policy::{GrantStore, PermissionResolver};
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::{Tool, ToolRegistry};

/// Replays scripted LLM responses in order, then plain text.
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
                ..Default::default()
            });
        Ok(stream_from_response(resp))
    }
}

/// A trivial `bash` host tool advertised by the built-in `build` profile.
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

/// A resolver that answers a single fixed grade for every call and records what
/// it was asked — the multi-tenant embedder's DB lookup, stubbed.
struct FixedResolver {
    grade: Permission,
    seen: Arc<Mutex<Vec<(SessionId, String, String)>>>,
}
#[async_trait]
impl PermissionResolver for FixedResolver {
    async fn resolve(&self, session: &SessionId, tool: &str, input: &str) -> Permission {
        self.seen
            .lock()
            .unwrap()
            .push((session.clone(), tool.to_string(), input.to_string()));
        self.grade
    }
}

/// One recorded `GrantStore::record` call: `(session, tool, arg, scope, mode)`.
type Recorded = (SessionId, String, Option<String>, ApprovalScope, String);

/// A grant store that records `record` calls in memory and NEVER touches a file —
/// the multi-tenant embedder's DB write, stubbed. `is_granted` always says no (a
/// real embedder resolves grants through its `PermissionResolver`).
#[derive(Default)]
struct RecordingGrants {
    recorded: Arc<Mutex<Vec<Recorded>>>,
}
#[async_trait]
impl GrantStore for RecordingGrants {
    fn is_granted(
        &self,
        _session: &SessionId,
        _tool: &str,
        _arg: Option<&str>,
        _mode: &str,
    ) -> bool {
        false
    }
    async fn record(
        &self,
        session: &SessionId,
        tool: &str,
        arg: Option<&str>,
        scope: ApprovalScope,
        mode: &str,
    ) {
        self.recorded.lock().unwrap().push((
            session.clone(),
            tool.to_string(),
            arg.map(str::to_string),
            scope,
            mode.to_string(),
        ));
    }
    fn forget_session(&self, _session: &SessionId) {}
}

/// Spawn a Holly whose scripted LLM calls `bash` once, wired to the given custom
/// resolver + grant store via [`ToolExecutor::builder`]. The tool
/// mask is retired (ADR-0207 §8) — every advertised tool dispatches — so the
/// grade is entirely the resolver's, whatever mode the session happens to run
/// in.
fn spawn_with_policy(
    input: &str,
    resolver: Arc<dyn PermissionResolver>,
    grants: Arc<dyn GrantStore>,
) -> Holly {
    let scripted = Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: input.into(),
                provider_meta: None,
            }],
            ..Default::default()
        },
        LlmResponse {
            text: "ok".into(),
            tool_calls: vec![],
            ..Default::default()
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
    // `active` is folded by the executor (spawn gating/sandbox); the custom
    // resolver ignores it, as it does `perm_modes` (the session's permission
    // mode) — a custom resolver decides the grade its own way. Base ceiling
    // is allow-all, so it never clamps the resolver.
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let _executor = ToolExecutor::builder(&holly, reg.shared())
        .agents(profiles)
        .base(PermissionProfile::new(Permission::Allow))
        .active(active)
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(grants)
        .mode_table(Arc::new(
            entanglement_runtime::mode::ModeTable::builtin()
                .expect("built-in permission modes must parse"),
        ))
        .spawn();
    holly
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

#[tokio::test]
async fn custom_resolver_allow_runs_without_approval() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let resolver = Arc::new(FixedResolver {
        grade: Permission::Allow,
        seen: seen.clone(),
    });
    let holly = spawn_with_policy("echo hi", resolver, Arc::new(RecordingGrants::default()));
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "custom Allow must not ask for approval"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output == "ran: echo hi")),
        "custom Allow should run the tool; got {events:?}"
    );
    // The resolver was consulted for the concrete call (session + tool + input).
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter()
            .any(|(s, t, i)| s == &sid && t == "bash" && i.contains("echo hi")),
        "resolver should observe the call; saw {seen:?}"
    );
}

#[tokio::test]
async fn custom_resolver_deny_refuses_without_request() {
    let resolver = Arc::new(FixedResolver {
        grade: Permission::Deny,
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let holly = spawn_with_policy("rm -rf", resolver, Arc::new(RecordingGrants::default()));
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "rm")).await.unwrap();
    let events = collect(sub, &sid).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "no approval expected on custom Deny"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolOutput { output, .. } if output.contains("denied"))),
        "custom Deny should report a denial; got {events:?}"
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "custom Deny must not run the tool"
    );
}

#[tokio::test]
async fn custom_resolver_ask_then_always_routes_through_custom_grant_store() {
    // No env pin (#747): the custom store is the only grant store wired here,
    // so nothing can reach the managed grants file — and a stray write to it
    // would land in `make test`'s HOME/XDG sandbox and fail that gate.
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let grants = Arc::new(RecordingGrants {
        recorded: recorded.clone(),
    });
    let resolver = Arc::new(FixedResolver {
        grade: Permission::Ask,
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    // Valid JSON so the runtime extracts the argument-scoped grant key (#173).
    let holly = spawn_with_policy(r#"{"command":"ls"}"#, resolver, grants);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    let mut watch = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "run")).await.unwrap();

    // Custom Ask emits a ToolRequest.
    let mut got_request = false;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(2), watch.recv()).await {
        if matches!(&ev, OutEvent::ToolRequest { tool, .. } if tool == "bash") {
            got_request = true;
            break;
        }
    }
    assert!(got_request, "custom Ask should emit a ToolRequest");

    // Approve with `Always` — the write must route through the custom store.
    holly
        .send(InMsg::Approve {
            session: sid.clone(),
            request_id: "t1".into(),
            scope: ApprovalScope::Always,
            mode: None,
        })
        .await
        .unwrap();
    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(
            |e| matches!(e, OutEvent::ToolOutput { output, .. } if output.starts_with("ran:"))
        ),
        "approved tool should run; got {events:?}"
    );

    // The `Always` grant landed in the custom store, tagged with the
    // session's mode (ADR-0207 §8) — the default `DEFAULT_MODE`, "build",
    // since this harness never sends `SetMode`.
    let recorded = recorded.lock().unwrap();
    assert!(
        recorded.iter().any(|(s, t, arg, scope, mode)| s == &sid
            && t == "bash"
            && arg.as_deref() == Some("ls")
            && *scope == ApprovalScope::Always
            && mode == "build"),
        "Always approval should route through the custom GrantStore; saw {recorded:?}"
    );
}
