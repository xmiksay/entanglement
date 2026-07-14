//! Integration test: the host tools wired through the real engine — a scripted
//! LLM asks for a tool call, the engine dispatches it under the `build` profile
//! (Allow), and the result comes back as a `ToolOutput`. Validates the registry
//! wiring from ADR-0008 + ADR-0009; `bash` is registered explicitly here to
//! mirror a head's opt-in path (ADR-0010).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::protocol::FileChangeKind;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::host::{host_tools, BashTool, CallTool};
use entanglement_runtime::tool_runner::spawn_tool_executor;
use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256, matching the `FileChange.hash` the executor emits.
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// An LLM that replays a scripted list of responses in order, then a plain
/// text reply (so a turn loop that re-prompts after a tool call terminates).
struct ScriptedLlm {
    responses: Mutex<Vec<LlmResponse>>,
}
impl ScriptedLlm {
    fn new(mut responses: Vec<LlmResponse>) -> Self {
        // Pop from the back; reverse so the first scripted reply is popped first.
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

/// Collect events for `sid` until `Done`, with a safety timeout.
async fn collect(
    mut sub: tokio::sync::broadcast::Receiver<OutEvent>,
    sid: &SessionId,
) -> Vec<OutEvent> {
    let mut out = Vec::new();
    loop {
        let Ok(recv) = tokio::time::timeout(Duration::from_secs(3), sub.recv()).await else {
            break;
        };
        match recv {
            Ok(ev) if ev.session() == sid => {
                let done = matches!(ev, OutEvent::Done { .. });
                out.push(ev);
                if done {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

#[tokio::test]
async fn read_tool_runs_through_engine_under_build_profile() {
    // Scratch file under a temp dir.
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-host-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("hello.txt"), "greetings\nfrom holly\n").unwrap();
    // Best-effort cleanup.
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());

    let read_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "r1".into(),
            name: "read".into(),
            input: r#"{"path":"hello.txt"}"#.into(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    // ScriptedLlm::new reverses internally and pops from the back, so passing
    // [read_call, finish] yields read_call first, then the tool-free turn that
    // ends the loop.
    let scripted = Arc::new(vec![read_call, finish]);
    let tools = host_tools(root.clone());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve a `SetAgent`/`Spawn` to `plan`/`explore`.
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    // Core relocated execution to the runtime (#58): the executor answers the
    // ToolExec round-trip against the real host-tool registry.
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "read it".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    // No ToolRequest under Allow — it ran directly.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "read should auto-run under build"
    );
    let output = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("expected a ToolOutput");
    assert!(output.contains("greetings"), "got: {output}");
    assert!(output.contains("from holly"), "got: {output}");
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn edit_tool_creates_file_through_engine_under_build_profile() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-edit-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());

    let edit_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "e1".into(),
            name: "edit".into(),
            input: r#"{"path":"out.txt","oldString":"","newString":"created\n"}"#.into(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![edit_call, finish]);
    let tools = host_tools(root.clone());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve a `SetAgent`/`Spawn` to `plan`/`explore`.
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    // Core relocated execution to the runtime (#58): the executor answers the
    // ToolExec round-trip against the real host-tool registry.
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "make the file".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "edit should auto-run under build"
    );
    let output = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("expected a ToolOutput");
    assert!(output.contains("created"), "got: {output}");
    // The create actually landed on disk.
    let on_disk = std::fs::read_to_string(root.join("out.txt")).unwrap();
    assert_eq!(on_disk, "created\n");
    // The executor emits the FileChange audit for the create (#202): path,
    // `Create`, and the SHA-256 of the after-content — no file bytes on the wire.
    let (path, kind, hash) = events
        .iter()
        .find_map(|e| match e {
            OutEvent::FileChange {
                path,
                change_kind,
                hash,
                ..
            } => Some((path.clone(), *change_kind, hash.clone())),
            _ => None,
        })
        .expect("expected a FileChange");
    assert_eq!(path, "out.txt");
    assert_eq!(kind, FileChangeKind::Create);
    assert_eq!(hash, sha256_hex(b"created\n"));
}

#[tokio::test]
async fn write_tool_creates_and_overwrites_through_engine_under_build_profile() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-write-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());

    // Two write calls in one turn: create a nested file, then overwrite it.
    let create_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "w1".into(),
            name: "write".into(),
            input: r#"{"path":"pkg/out.txt","content":"a\nb\n"}"#.into(),
        }],
    };
    let overwrite_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "w2".into(),
            name: "write".into(),
            input: r#"{"path":"pkg/out.txt","content":"only\n"}"#.into(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![create_call, overwrite_call, finish]);
    let tools = host_tools(root.clone());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve a `SetAgent`/`Spawn` to `plan`/`explore`.
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "write the file".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "write should auto-run under build"
    );
    let outputs: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();
    assert!(
        outputs
            .iter()
            .any(|o| o.contains("created") && o.contains("2 lines")),
        "expected a create confirmation; got {outputs:?}"
    );
    assert!(
        outputs
            .iter()
            .any(|o| o.contains("overwrote") && o.contains("1 lines, was 2")),
        "expected an overwrite confirmation; got {outputs:?}"
    );
    // Confirmations must not echo file content.
    assert!(
        !outputs.iter().any(|o| o.contains("only")),
        "write output must not echo content; got {outputs:?}"
    );
    let on_disk = std::fs::read_to_string(root.join("pkg/out.txt")).unwrap();
    assert_eq!(on_disk, "only\n");
    // Both writes emit a FileChange audit (#202): the first `Create`, the second
    // `Edit`, each carrying the after-content hash in ToolExec order.
    let changes: Vec<(String, FileChangeKind, String)> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::FileChange {
                path,
                change_kind,
                hash,
                ..
            } => Some((path.clone(), *change_kind, hash.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes,
        vec![
            (
                "pkg/out.txt".into(),
                FileChangeKind::Create,
                sha256_hex(b"a\nb\n")
            ),
            (
                "pkg/out.txt".into(),
                FileChangeKind::Edit,
                sha256_hex(b"only\n")
            ),
        ],
    );
}

#[tokio::test]
async fn write_tool_denied_under_explore_profile() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-write-deny-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());

    let write_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "w1".into(),
            name: "write".into(),
            input: r#"{"path":"blocked.txt","content":"nope\n"}"#.into(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![write_call, finish]);
    let tools = host_tools(root.clone());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve a `SetAgent`/`Spawn` to `plan`/`explore`.
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "explore".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "try to write".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    // `write` is now *masked* out of `explore`'s tool set (#116, ADR-0038): the
    // executor refuses it as "not available" before permission even resolves —
    // a strictly stronger block than the earlier permission `Deny`.
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, .. } if output.contains("not available")
        )),
        "explore should refuse write as unavailable; got {events:?}"
    );
    assert!(!root.join("blocked.txt").exists(), "write must not land");
}

#[tokio::test]
async fn write_tool_masked_under_plan_profile() {
    // The built-in `plan` is now physically read-only (#140, ADR-0041): its tool
    // mask carries only the read trio + delegation/skill tools, so `write` is
    // masked out and refused as "not available" before permission resolves — the
    // plan agent authors the plan, it never mutates the tree.
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-write-plan-mask-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());

    let write_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "w1".into(),
            name: "write".into(),
            input: r#"{"path":"blocked.txt","content":"nope\n"}"#.into(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![write_call, finish]);
    let tools = host_tools(root.clone());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve a `SetAgent`/`Spawn` to `plan`/`explore`.
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let sid = SessionId::new("s1");
    holly
        .send(InMsg::SetAgent {
            session: sid.clone(),
            agent: "plan".into(),
        })
        .await
        .unwrap();
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "try to write".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, .. } if output.contains("not available")
        )),
        "plan should refuse write as unavailable; got {events:?}"
    );
    assert!(!root.join("blocked.txt").exists(), "write must not land");
}

#[tokio::test]
async fn bash_tool_runs_through_engine_under_build_profile() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-bash-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());

    let bash_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "b1".into(),
            name: "bash".into(),
            input: r#"{"command":"printf 'shell-ok\\n'"}"#.into(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![bash_call, finish]);
    // bash is opt-in (ADR-0010); mirror what `skutter` does when
    // ENTANGLEMENT_ENABLE_BASH=1 by registering BashTool explicitly.
    let mut tools = host_tools(root.clone());
    tools.register(BashTool::new(root.clone()));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve a `SetAgent`/`Spawn` to `plan`/`explore`.
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    // Core relocated execution to the runtime (#58): the executor answers the
    // ToolExec round-trip against the real host-tool registry.
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "run it".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "bash should auto-run under build"
    );
    let output = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("expected a ToolOutput");
    assert!(output.contains("[exit 0]"), "got: {output}");
    assert!(output.contains("shell-ok"), "got: {output}");
}

#[tokio::test]
async fn call_tool_runs_argv_verbatim_through_engine_under_build_profile() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-call-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());

    // A payload full of shell metacharacters: passed as argv it must reach
    // `printf` verbatim, never expanded or split by a shell.
    let payload = "$HOME && rm -rf / | cat *.rs";
    let call_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "call".into(),
            input: serde_json::json!({ "command": "printf", "args": ["%s", payload] }).to_string(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![call_call, finish]);
    // `call` is opt-in (ADR-0010/ADR-0045); mirror the head registering the exec
    // pair under ENTANGLEMENT_ENABLE_BASH=1.
    let mut tools = host_tools(root.clone());
    tools.register(BashTool::new(root.clone()));
    tools.register(CallTool::new(root.clone()));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        // Core carries only `build` now (#201); the engine needs the full trio to
        // resolve a `SetAgent`/`Spawn` to `plan`/`explore`.
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let _executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "call it".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "call should auto-run under build"
    );
    let output = events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("expected a ToolOutput");
    assert!(output.contains("[exit 0]"), "got: {output}");
    // The metacharacters survived as literal argv — no shell touched them.
    assert!(
        output.contains(payload),
        "argv must be verbatim, got: {output}"
    );
}
