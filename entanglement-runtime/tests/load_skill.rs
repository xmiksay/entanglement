//! Integration test for the `load_skill` tier-2 tool (#115, ADR-0037).
//!
//! Drives the real engine with a scripted LLM: turn 1 calls `load_skill` for a
//! project skill; the handler substitutes the body's relative `references/…`
//! ref to an absolute path. Turn 2 feeds that exact absolute path back into the
//! `read` tool, proving the model can open a substituted ref without guessing a
//! base directory (the bug class ADR-0037 closes). A third case checks that a
//! read-only `explore` profile denies `load_skill` like any other host tool —
//! no special exemption.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmSession,
    LlmStream, OutEvent, ProfileRegistry, SessionId, ToolCall,
};
use entanglement_runtime::host::host_tools;
use entanglement_runtime::skills::{load_registry, LoadSkillTool};
use entanglement_runtime::tool_runner::spawn_tool_executor;

/// Replays scripted responses in order, then a plain text reply so the turn loop
/// terminates. The `read` call in turn 2 references a path captured *after* the
/// scripts are built, so it is injected via a shared slot the LLM reads live.
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

/// Write a project skill under `<root>/.entanglement/skills/<name>/` with a
/// `references/guide.md` payload, and return the root the tools/registry use.
fn write_project_skill(root: &std::path::Path) {
    let skill_dir = root.join(".entanglement/skills/demo");
    std::fs::create_dir_all(skill_dir.join("references")).unwrap();
    std::fs::write(skill_dir.join("references/guide.md"), "the guide body\n").unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: a demo skill\n---\n\
         Follow references/guide.md to proceed.\n",
    )
    .unwrap();
}

#[tokio::test]
async fn load_skill_then_read_a_substituted_ref() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-loadskill-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());
    write_project_skill(&root);

    // The absolute path the handler substitutes for `references/guide.md`.
    let abs_ref = root.join(".entanglement/skills/demo/references/guide.md");

    let load_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "l1".into(),
            name: "load_skill".into(),
            input: r#"{"skill_name":"demo"}"#.into(),
        }],
    };
    // Turn 2 reads the substituted absolute path directly — no base-guessing.
    let read_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "r1".into(),
            name: "read".into(),
            input: serde_json::json!({ "path": abs_ref.to_string_lossy() }).to_string(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };

    let scripted = Arc::new(vec![load_call, read_call, finish]);
    // Point the user layer at a non-existent dir so a real ~/.config skill can't
    // leak in; the project layer under `root` supplies `demo`.
    std::env::set_var("ENTANGLEMENT_SKILLS_DIR", root.join("no-such-user-dir"));
    let registry = Arc::new(load_registry(&root).unwrap());
    std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");

    let mut tools = host_tools(root.clone());
    tools.register(LoadSkillTool::new(registry));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(ScriptedLlm::new((*scripted).clone())))
        }),
        tool_specs: tools.specs(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let _executor = spawn_tool_executor(&holly, tools, ProfileRegistry::new());
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::Prompt {
            session: sid.clone(),
            text: "use the demo skill".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    let outputs: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();

    // The load_skill result carries the skill_id and the absolute-substituted ref.
    assert!(
        outputs.iter().any(|o| o.contains("skill_id: demo")),
        "expected a load_skill result; got {outputs:?}"
    );
    assert!(
        outputs
            .iter()
            .any(|o| o.contains(&abs_ref.display().to_string())),
        "relative ref must be substituted to absolute; got {outputs:?}"
    );
    // The subsequent read of that absolute path returned the guide body.
    assert!(
        outputs.iter().any(|o| o.contains("the guide body")),
        "read of the substituted ref must return its contents; got {outputs:?}"
    );
    assert!(events.iter().any(|e| matches!(e, OutEvent::Done { .. })));
}

#[tokio::test]
async fn load_skill_denied_under_explore_profile() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-loadskill-deny-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    struct Drop_(std::path::PathBuf);
    impl Drop for Drop_ {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Drop_(root.clone());
    write_project_skill(&root);

    let load_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "l1".into(),
            name: "load_skill".into(),
            input: r#"{"skill_name":"demo"}"#.into(),
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let scripted = Arc::new(vec![load_call, finish]);
    std::env::set_var("ENTANGLEMENT_SKILLS_DIR", root.join("no-such-user-dir"));
    let registry = Arc::new(load_registry(&root).unwrap());
    std::env::remove_var("ENTANGLEMENT_SKILLS_DIR");

    let mut tools = host_tools(root.clone());
    tools.register(LoadSkillTool::new(registry));
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            LlmSession::new(Box::new(ScriptedLlm::new((*scripted).clone())))
        }),
        tool_specs: tools.specs(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    // `explore` is the built-in read-only profile: default Deny — `load_skill`
    // is gated exactly like `read`, no exemption (ADR-0037).
    let _executor = spawn_tool_executor(&holly, tools, ProfileRegistry::new());
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
            text: "try the demo skill".into(),
        })
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, .. } if output.contains("denied")
        )),
        "explore should deny load_skill; got {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, .. } if output.contains("skill_id")
        )),
        "denied load_skill must not return a body; got {events:?}"
    );
}
