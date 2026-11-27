//! `pre_tool_use` against `rhai` (#964): the veto is a resolver grade, so it
//! refuses a launch — a `rhai.trusted` one included — and a script's nested
//! call, under trust like any other deny. The launch's grade runs before the
//! script file is read.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use entanglement_core::{Holly, InMsg, LlmResponse, OutEvent, SessionId, ToolCall};

use super::{collect, engine, scratch, spec, tool_output, wire, wire_trusted};
use skutter::hooks::{HookSpec, Hooks};

/// Tries a nested `write`, returning `ran` or the caught refusal.
const TRY_WRITE: &str = r#"let r = "ran"; try { write("no.txt", "x"); } catch (e) { r = e; } r"#;

/// One scripted `rhai` call with `input`.
fn rhai_call(input: String) -> Arc<Vec<LlmResponse>> {
    Arc::new(vec![LlmResponse {
        tool_calls: vec![ToolCall {
            id: "s1".into(),
            name: "rhai".into(),
            input,
            provider_meta: None,
        }],
        ..Default::default()
    }])
}

/// A `pre_tool_use` hook that runs `command` for `tool` only.
fn hook_for(tool: &str, command: &str) -> Hooks {
    Hooks {
        pre_tool_use: vec![HookSpec {
            tools: vec![tool.into()],
            ..spec(command)
        }],
        ..Default::default()
    }
}

/// `root/scripts` (trusted), holding `try.rhai` = [`TRY_WRITE`].
fn trusted_scripts(root: &Path) -> Vec<std::path::PathBuf> {
    let dir = root.join("scripts");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("try.rhai"), TRY_WRITE).unwrap();
    vec![dir.canonicalize().unwrap()]
}

/// Prompt `s1` and return the first `ToolOutput`'s text.
async fn run(holly: &Holly) -> String {
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;
    tool_output(&events).expect("a ToolOutput")
}

/// A `tools: [rhai]` veto refuses even a trusted `path` launch, which would
/// otherwise run with no card.
#[tokio::test]
async fn a_rhai_veto_refuses_a_trusted_launch() {
    let s = scratch("pre-rhai-trusted");
    let root = s.0.clone();
    let trusted = trusted_scripts(&root);
    let input = serde_json::json!({ "path": "scripts/try.rhai" }).to_string();
    let (holly, tools) = engine(&root, rhai_call(input));
    let _exec = wire_trusted(&holly, &root, tools, hook_for("rhai", "exit 1"), trusted);

    let output = run(&holly).await;
    assert_eq!(
        output, "tool `rhai` blocked by pre_tool_use hook: hook exited with code 1",
        "got: {output}"
    );
    assert!(!root.join("no.txt").exists(), "the vetoed script ran");
}

/// Under trust every nested call is force-allowed, yet a `write` veto still
/// refuses the nested `write` as a catchable error.
#[tokio::test]
async fn a_nested_veto_wins_under_a_trusted_script() {
    let s = scratch("pre-rhai-nested-trusted");
    let root = s.0.clone();
    let trusted = trusted_scripts(&root);
    let input = serde_json::json!({ "path": "scripts/try.rhai" }).to_string();
    let (holly, tools) = engine(&root, rhai_call(input));
    let _exec = wire_trusted(&holly, &root, tools, hook_for("write", "exit 1"), trusted);

    let output = run(&holly).await;
    assert!(
        output.contains("tool `write` blocked by pre_tool_use hook"),
        "got: {output}"
    );
    assert!(!root.join("no.txt").exists(), "the vetoed write ran");
}

/// The launch is graded on its input alone, so the hook runs before the file
/// is read: a hook can still create the very script that then runs.
#[tokio::test]
async fn the_script_file_is_read_after_the_hook() {
    let s = scratch("pre-rhai-late");
    let root = s.0.clone();
    let trusted = trusted_scripts(&root);
    let late = root.join("scripts/late.rhai");
    let input = serde_json::json!({ "path": "scripts/late.rhai" }).to_string();
    let (holly, tools) = engine(&root, rhai_call(input));
    let hooks = hook_for("rhai", &format!("printf 41+1 > {}", late.display()));
    let _exec = wire_trusted(&holly, &root, tools, hooks, trusted);

    let output = run(&holly).await;
    assert!(output.contains("42"), "got: {output}");
}

/// The veto reaches an inline script's nested call like any deny: the
/// binding throws and the write never happens.
#[tokio::test]
async fn a_nested_veto_refuses_an_inline_scripts_call() {
    let s = scratch("pre-nested");
    let root = s.0.clone();
    let input = serde_json::json!({ "script": TRY_WRITE }).to_string();
    let (holly, tools) = engine(&root, rhai_call(input));
    let _exec = wire(&holly, &root, tools, hook_for("write", "exit 1"));

    let sid = SessionId::new("s1");
    let mut sub = holly.subscribe();
    // `auto` runs an inline script with no launch card (`build` asks).
    holly
        .send(InMsg::SetMode {
            session: sid.clone(),
            mode: "auto".into(),
        })
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
            Ok(Ok(OutEvent::ModeChanged { mode, .. })) if mode == "auto" => break,
            Ok(Ok(_)) => {}
            other => panic!("no mode confirmation: {other:?}"),
        }
    }
    holly.send(InMsg::prompt(sid.clone(), "go")).await.unwrap();
    let events = collect(sub, &sid).await;

    let output = tool_output(&events).expect("a ToolOutput");
    assert!(
        output.contains("tool `write` blocked by pre_tool_use hook"),
        "got: {output}"
    );
    assert!(!root.join("no.txt").exists(), "the vetoed write ran");
}
