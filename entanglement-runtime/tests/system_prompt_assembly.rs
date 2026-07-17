//! Integration test for deterministic system-prompt assembly (#113).
//!
//! Composition is baked into the registry by `load_registry` (runtime), then
//! shipped verbatim by core as `LlmRequest.system`. This drives two real spawned
//! sessions through `Holly` and captures the exact `system` string each one sends
//! to the LLM on its first turn, asserting:
//!
//! - a **primary** agent that flags `include_brief` gets
//!   `preamble + body + brief + env + skills`;
//! - a **subagent** gets `preamble + body` only — and *not* the project brief the
//!   sibling primary included (the child never inherits the parent's prompt).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId,
};
use entanglement_runtime::agents::load_registry;
use entanglement_runtime::system_prompt::{EnvBlock, PromptContext, SkillDisclosure};

const PREAMBLE: &str = "SHARED-PREAMBLE-RULES";
const BRIEF: &str = "PROJECT-BRIEF-TEXT";
const PARENT_BODY: &str = "PARENT-BODY-PROMPT";
const CHILD_BODY: &str = "CHILD-BODY-PROMPT";

fn write_agent(dir: &std::path::Path, file: &str, contents: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(file), contents).unwrap();
}

/// Records every `system` string it is asked to stream, then finishes the turn
/// immediately so both sessions reach `Done`.
struct RecordingLlm {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Llm for RecordingLlm {
    async fn stream(&mut self, req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        self.seen.lock().unwrap().push(req.system.to_string());
        Ok(stream_from_response(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
        }))
    }
}

#[tokio::test]
async fn spawned_child_system_has_preamble_and_body_but_not_the_parent_brief() {
    // A file-defined primary that opts into the brief, and a subagent that does not.
    let project = tempfile::tempdir().unwrap();
    let agents_dir = project.path().join(".entanglement").join("agents");
    write_agent(
        &agents_dir,
        "parent.md",
        &format!(
            "---\nname: parent\ndescription: primary\nmode: primary\ninclude_brief: true\n---\n{PARENT_BODY}"
        ),
    );
    write_agent(
        &agents_dir,
        "child.md",
        &format!("---\nname: child\ndescription: leaf\nmode: subagent\n---\n{CHILD_BODY}"),
    );

    // Explicit composition inputs: real preamble, brief, env, and one skill.
    let ctx = PromptContext {
        preamble: Some(PREAMBLE.into()),
        brief: Some(BRIEF.into()),
        env: Some(EnvBlock {
            root: PathBuf::from("/work/root"),
            platform: "test-os".into(),
            date: "2026-07-10".into(),
        }),
        skills: vec![SkillDisclosure {
            name: "git".into(),
            description: "commit helpers".into(),
        }],
        ..Default::default()
    };

    // Isolate from any host user-agents dir; project agents come from the temp dir.
    std::env::set_var("ENTANGLEMENT_AGENTS_DIR", "/nonexistent-user-agents-dir");
    let profiles = load_registry(
        project.path(),
        &ctx,
        &entanglement_runtime::skills::SkillRegistry::default(),
    )
    .expect("load_registry");
    std::env::remove_var("ENTANGLEMENT_AGENTS_DIR");

    // Sanity: the registry itself carries the composed prompts.
    let parent_prompt = &profiles.get("parent").unwrap().system_prompt;
    assert!(parent_prompt.contains(PREAMBLE) && parent_prompt.contains(BRIEF));
    let child_prompt = &profiles.get("child").unwrap().system_prompt;
    assert!(child_prompt.contains(CHILD_BODY) && !child_prompt.contains(BRIEF));

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_factory = seen.clone();
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(RecordingLlm {
                seen: seen_factory.clone(),
            }) as Box<dyn Llm>
        }),
        profiles,
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let mut sub = holly.subscribe();

    // Spawn both agents as sub-sessions of a (never-created) root so each runs
    // under its own profile and streams its own first request.
    let root = SessionId::new("root");
    for (child, agent) in [("p1", "parent"), ("c1", "child")] {
        holly
            .send(InMsg::Spawn {
                session: SessionId::new(child),
                parent: Some(root.clone()),
                predecessor: None,
                agent: agent.into(),
                prompt: "task".into(),
            })
            .await
            .unwrap();
    }

    // Wait until both sessions have finished a turn.
    let mut done = 0;
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
        if matches!(ev, OutEvent::Done { .. }) {
            done += 1;
            if done == 2 {
                break;
            }
        }
    }
    assert_eq!(done, 2, "both spawned sessions should finish");

    let systems = seen.lock().unwrap().clone();
    let parent_sys = systems
        .iter()
        .find(|s| s.contains(PARENT_BODY))
        .expect("parent session streamed a request");
    let child_sys = systems
        .iter()
        .find(|s| s.contains(CHILD_BODY))
        .expect("child session streamed a request");

    // Primary: the full five-part assembly.
    assert!(parent_sys.contains(PREAMBLE), "primary keeps the preamble");
    assert!(parent_sys.contains(BRIEF), "primary flagged the brief in");
    assert!(parent_sys.contains("<env>"), "primary gets the env block");
    assert!(
        parent_sys.contains("git: commit helpers"),
        "primary gets the skill index"
    );

    // Subagent: preamble + body only — never the parent's brief, env, or skills.
    assert!(child_sys.contains(PREAMBLE), "subagent keeps the preamble");
    assert!(
        child_sys.contains(CHILD_BODY),
        "subagent keeps its own body"
    );
    assert!(
        !child_sys.contains(BRIEF),
        "unflagged subagent must NOT carry the brief: {child_sys:?}"
    );
    assert!(!child_sys.contains("<env>"), "subagent gets no env block");
    assert!(
        !child_sys.contains("git: commit helpers"),
        "subagent gets no skill index"
    );
    // And it is composed from its own body, not the parent's.
    assert!(
        !child_sys.contains(PARENT_BODY),
        "no parent-prompt inheritance"
    );
}
