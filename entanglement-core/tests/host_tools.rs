//! Integration test: the host-tool quintet (`read`/`glob`/`grep`/`edit`/`bash`)
//! wired through the real engine — a scripted LLM asks for a tool call, the
//! engine dispatches it under the `build` profile (Allow), and the result comes
//! back as a `ToolOutput`. Validates the registry wiring from ADR-0008 + ADR-0009.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    host_tools, stream_from_response, BashTool, EngineConfig, Holly, InMsg, Llm, LlmRequest,
    LlmResponse, LlmStream, OutEvent, SessionId, ToolCall,
};

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
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tools: host_tools(root.clone()),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
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
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tools: host_tools(root.clone()),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
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
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tools: {
            let mut reg = host_tools(root.clone());
            reg.register(BashTool::new(root.clone()));
            reg
        },
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
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
