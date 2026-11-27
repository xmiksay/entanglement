//! #933: the library's `rhai` engine binds no file or process function of
//! its own — a script reaches a registered tool only when that tool declares
//! a `Tool::script_binding`, and then through the same dispatch seam.

use std::borrow::Cow;

use async_trait::async_trait;
use entanglement_core::{InMsg, LlmResponse, OutEvent, Permission, SessionId, ToolCall};
use entanglement_runtime::attendance::Attendance;
use entanglement_runtime::capability::Capability;
use entanglement_runtime::host_ns::{ScriptArg, ScriptBinding};
use entanglement_runtime::mode::Limits;
use entanglement_runtime::tool_names::RHAI_TOOL;
use entanglement_runtime::{Tool, ToolRegistry};

use crate::unattended_mode::{collect, done_text, one_mode_table, spawn_attended, EchoBash};

/// A tool that binds itself as `fetch_doc(path)`.
struct FetchDoc;

#[async_trait]
impl Tool for FetchDoc {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("fetch_doc")
    }
    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Read]
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("doc for {input}"))
    }
    fn script_binding(&self) -> Option<ScriptBinding> {
        Some(ScriptBinding {
            overloads: &[&[("path", ScriptArg::Str)]],
            json: false,
            budget_key: None,
            doc: "fetch_doc(path) -> the doc",
        })
    }
}

/// Run `script` as one allowed `rhai` call against a library-only executor
/// — a registered `bash` with no binding, and `fetch_doc` — and return the
/// call's output.
async fn run_script(script: &str) -> String {
    let mut reg = ToolRegistry::new();
    reg.register(EchoBash);
    reg.register(FetchDoc);
    let call = LlmResponse {
        tool_calls: vec![ToolCall {
            id: "r1".into(),
            name: RHAI_TOOL.into(),
            input: serde_json::json!({ "script": script }).to_string(),
            provider_meta: None,
        }],
        ..Default::default()
    };
    let holly = spawn_attended(
        reg,
        one_mode_table(Permission::Allow, Limits::default()),
        vec![call, done_text()],
        Attendance::new(),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;
    events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no rhai output for {script}: {events:?}"))
}

#[tokio::test]
async fn a_library_only_script_cannot_touch_files_or_spawn_processes() {
    for script in [
        r#"read("Cargo.toml")"#,
        r#"read_raw("Cargo.toml")"#,
        r#"write("x.txt", "y")"#,
        r#"edit("x.txt", "a", "b")"#,
        r#"glob("*")"#,
        r#"grep("x")"#,
        r#"bash("echo hi")"#,
    ] {
        let out = run_script(script).await;
        assert!(out.contains("Function not found"), "{script}: {out}");
        assert!(!out.contains("ran: "), "{script} reached bash: {out}");
    }
}

#[tokio::test]
async fn a_tool_that_declares_a_binding_is_callable_by_its_name() {
    let out = run_script(r#"fetch_doc("a.md")"#).await;
    assert!(out.contains("doc for") && out.contains("a.md"), "{out}");
}
