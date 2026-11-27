//! Per-call grading and host script namespaces (ADR-0249): a tool with
//! `call_grading` is graded as the target; a host namespace's `<ns>::<fn>()`
//! dispatches to the backing tool, graded per-call.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::{
    EngineConfig, Holly, LlmResponse, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::capability::Capability;
use entanglement_runtime::host::host_tools;
use entanglement_runtime::host_ns::{HostFn, HostNamespace};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::policy::{
    Grade, GrantStore, InMemoryGrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::tool_runner::ToolExecutor;
use entanglement_runtime::tools::{CallGrading, Tool};

use super::{collect, collect_auto_approving, rhai_output, ScriptedLlm, TempDir};

// -- The envelope tool: dispatches by target, grades per call ----------------

struct InvokeTool {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Tool for InvokeTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("invoke")
    }
    fn description(&self) -> &str {
        "dispatch to a host function"
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Write]
    }
    fn call_grading(&self, input: &str) -> Option<CallGrading> {
        let v: serde_json::Value = serde_json::from_str(input).ok()?;
        let target = v.get("tool")?.as_str()?;
        let caps: &'static [Capability] = if target.starts_with("read") {
            &[Capability::Read]
        } else {
            &[Capability::Write]
        };
        Some(CallGrading {
            name: target.to_string(),
            capabilities: caps,
        })
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push(input.to_string());
        let v: serde_json::Value = serde_json::from_str(input)?;
        let target = v["tool"].as_str().unwrap_or("?");
        Ok(format!("invoked:{target}"))
    }
}

// -- Helpers -----------------------------------------------------------------

fn ask_write_table() -> Arc<ModeTable> {
    let allow = vec!["read".to_string()];
    let prompt = vec!["write".to_string()];
    let mode = Mode {
        name: "build".to_string(),
        default: Permission::Allow,
        rules: Rules::from_lists(&[], &allow, &prompt),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("valid"))
}

fn spawn_rhai_with_ns(
    script: &str,
    root: &std::path::Path,
    calls: Arc<Mutex<Vec<String>>>,
    mode_table: Arc<ModeTable>,
    resolver: Option<Arc<dyn PermissionResolver>>,
    ns_resolver: Option<entanglement_runtime::host_ns::HostFnResolver>,
) -> Holly {
    let input = serde_json::json!({ "script": script }).to_string();
    let scripted = Arc::new(vec![
        LlmResponse {
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: RHAI_TOOL.into(),
                input,
                provider_meta: None,
            }],
            ..Default::default()
        },
        LlmResponse {
            text: "ok".into(),
            ..Default::default()
        },
    ]);
    let agents = entanglement_runtime::agents::built_in_registry().expect("built-in agents parse");
    let holly = Holly::spawn(EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn entanglement_core::Llm>
        }),
        agents: agents.clone(),
        ..EngineConfig::default()
    });
    super::answer_launch_cards(&holly);
    let mut tools = host_tools(root.to_path_buf());
    tools.register(InvokeTool { calls });
    let shared = tools.shared();
    let modes: Arc<Mutex<HashMap<SessionId, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let base = PermissionProfile::new(Permission::Allow);
    let resolver = resolver.unwrap_or_else(|| {
        Arc::new(ModeResolver::new(
            modes.clone(),
            mode_table.clone(),
            shared.clone(),
            base.clone(),
            None,
        ))
    });
    let grants: Arc<dyn GrantStore> = Arc::new(InMemoryGrantStore::new());
    let default_ns_resolver: entanglement_runtime::host_ns::HostFnResolver =
        Arc::new(|_: &SessionId| {
            Some(vec![
                HostFn {
                    name: "read_page".to_string(),
                    description: "read".to_string(),
                },
                HostFn {
                    name: "write_page".to_string(),
                    description: "write".to_string(),
                },
            ])
        });
    let ns = HostNamespace {
        name: "app".to_string(),
        tool: "invoke".to_string(),
        resolver: ns_resolver.unwrap_or(default_ns_resolver),
    };
    let _exec = ToolExecutor::builder(&holly, shared)
        .agents(agents)
        .base(base)
        .perm_modes(modes)
        .resolver(resolver)
        .grants(grants)
        .mode_table(mode_table)
        .host_namespace(ns)
        .spawn();
    holly
}

// -- Tests -------------------------------------------------------------------

/// A read-graded target runs with no card; a write-graded target is asked.
#[tokio::test]
async fn call_grading_read_target_allowed_write_target_asked() {
    let dir = TempDir::new("call-grade-rw");
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let holly = spawn_rhai_with_ns(
        r#"
        let r = app::read_page(#{id: 1});
        let w = app::write_page(#{id: 2});
        r + "|" + w
        "#,
        &dir.path,
        calls.clone(),
        ask_write_table(),
        None,
        None,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    super::prompt(&holly, &sid, "general").await;
    let events = collect_auto_approving(&holly, sub, &sid).await;
    let out = rhai_output(&events).expect("rhai output");
    assert!(out.contains("invoked:read_page"), "read ran: {out}");
    assert!(
        out.contains("invoked:write_page"),
        "write ran after approval: {out}"
    );
    // The read target ran without a card; the write target asked.
    let cards: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            entanglement_core::OutEvent::ToolRequest { tool, input, .. } => {
                Some((tool.clone(), input.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(cards.len(), 1, "only the write target asked: {cards:?}");
    assert!(
        cards[0].1.contains("write_page"),
        "the card is for write_page: {:?}",
        cards[0]
    );
}

/// A custom PermissionResolver that denies a specific target name sees the
/// target from call_grading, not the envelope tool name.
#[tokio::test]
async fn custom_resolver_sees_the_target_name() {
    struct DenyTarget;
    #[async_trait::async_trait]
    impl PermissionResolver for DenyTarget {
        async fn resolve(&self, _session: &SessionId, tool: &str, _input: &str) -> Grade {
            if tool == "read_page" {
                Grade::Deny(None)
            } else {
                Grade::Allow
            }
        }
    }
    let dir = TempDir::new("call-grade-resolver");
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let holly = spawn_rhai_with_ns(
        r#"
        let r = "";
        try { r = app::read_page(#{}) }
        catch(e) { r = "denied:" + e }
        r
        "#,
        &dir.path,
        calls.clone(),
        super::allow_all_table(),
        Some(Arc::new(DenyTarget)),
        None,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    super::prompt(&holly, &sid, "general").await;
    let events = collect(sub, &sid).await;
    let out = rhai_output(&events).expect("rhai output");
    assert!(
        out.contains("denied:"),
        "the resolver denied read_page: {out}"
    );
    assert!(calls.lock().unwrap().is_empty(), "nothing ran");
}

/// An Always grant after approval is stored under the target name.
#[tokio::test]
async fn always_grant_stored_under_target_name() {
    let dir = TempDir::new("call-grade-grant");
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let holly = spawn_rhai_with_ns(
        r#"app::write_page(#{id: 1})"#,
        &dir.path,
        calls.clone(),
        ask_write_table(),
        None,
        None,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    super::prompt(&holly, &sid, "general").await;
    let events = collect_auto_approving(&holly, sub, &sid).await;
    let out = rhai_output(&events).unwrap_or_default();
    assert!(
        out.contains("invoked:write_page"),
        "the call ran after approval: {out}"
    );
}

/// Only resolver-listed names are generated; an unlisted name is unknown.
#[tokio::test]
async fn host_namespace_unlisted_name_is_unknown() {
    let dir = TempDir::new("host-ns-unlisted");
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let holly = spawn_rhai_with_ns(
        r#"app::nonexistent(#{x: 1})"#,
        &dir.path,
        calls,
        super::allow_all_table(),
        None,
        None,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    super::prompt(&holly, &sid, "general").await;
    let events = collect(sub, &sid).await;
    let out = rhai_output(&events).expect("rhai output");
    assert!(
        out.to_lowercase().contains("function"),
        "unlisted name is unknown: {out}"
    );
}

/// `app::read_page(#{id: 42})` dispatches as `{"tool":"read_page","args":{"id":42}}`.
#[tokio::test]
async fn host_namespace_dispatches_with_tool_and_args() {
    let dir = TempDir::new("host-ns-dispatch");
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let holly = spawn_rhai_with_ns(
        r#"app::read_page(#{id: 42})"#,
        &dir.path,
        calls.clone(),
        super::allow_all_table(),
        None,
        None,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    super::prompt(&holly, &sid, "general").await;
    let events = collect(sub, &sid).await;
    let out = rhai_output(&events).expect("rhai output");
    assert!(out.contains("invoked:read_page"), "tool ran: {out}");
    let c = calls.lock().unwrap();
    assert_eq!(c.len(), 1);
    let input: serde_json::Value = serde_json::from_str(&c[0]).unwrap();
    assert_eq!(input["tool"], "read_page");
    assert_eq!(input["args"]["id"], 42);
}

/// A write target in ask mode parks an approval card, not an unconditional allow.
#[tokio::test]
async fn host_namespace_write_target_in_ask_mode_asks() {
    let dir = TempDir::new("host-ns-ask");
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let holly = spawn_rhai_with_ns(
        r#"app::write_page(#{id: 1})"#,
        &dir.path,
        calls,
        ask_write_table(),
        None,
        None,
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    super::prompt(&holly, &sid, "general").await;
    let events = collect_auto_approving(&holly, sub, &sid).await;
    let out = rhai_output(&events).expect("rhai output");
    assert!(
        out.contains("invoked:write_page"),
        "ran after approval: {out}"
    );
    // The write target raised an approval card.
    let cards = events
        .iter()
        .filter(|e| matches!(e, entanglement_core::OutEvent::ToolRequest { .. }))
        .count();
    assert_eq!(cards, 1, "the write target asked");
}

/// A resolver returning None hides the namespace entirely.
#[tokio::test]
async fn host_namespace_none_resolver_hides_namespace() {
    let dir = TempDir::new("host-ns-none");
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let holly = spawn_rhai_with_ns(
        r#"app::read_page(#{id: 1})"#,
        &dir.path,
        calls,
        super::allow_all_table(),
        None,
        Some(Arc::new(|_: &SessionId| None)),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    super::prompt(&holly, &sid, "general").await;
    let events = collect(sub, &sid).await;
    let out = rhai_output(&events).expect("rhai output");
    assert!(
        out.to_lowercase().contains("function") || out.to_lowercase().contains("module"),
        "hidden namespace yields unknown error: {out}"
    );
}
