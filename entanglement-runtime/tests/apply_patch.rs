//! Integration test: `apply_patch` wired through the real engine (#455) —
//! mirrors `host_tools.rs`'s `write_tool_creates_and_overwrites_through_engine_under_build_profile`,
//! but for the multi-hunk unified-diff apply tool, and asserts the reserved
//! `FileChangeKind::ApplyDiff` variant it's the first producer of.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::protocol::FileChangeKind;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::host::host_tools;
use entanglement_runtime::tool_runner::spawn_tool_executor;
use sha2::{Digest, Sha256};

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

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
            Ok(ev) if ev.session() == Some(sid) => {
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

struct CleanupDir(std::path::PathBuf);
impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn build_holly_and_executor(
    root: std::path::PathBuf,
    scripted: Vec<LlmResponse>,
) -> (Holly, tokio::task::JoinHandle<()>) {
    let tools = host_tools(root);
    let scripted = Arc::new(scripted);
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        profiles: entanglement_runtime::agents::built_in_registry(),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let executor = spawn_tool_executor(
        &holly,
        tools,
        entanglement_runtime::agents::built_in_registry(),
        entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow),
    );
    (holly, executor)
}

#[tokio::test]
async fn apply_patch_multi_hunk_through_engine_emits_apply_diff() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-apply-patch-e2e-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    let _cleanup = CleanupDir(root.clone());
    std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();

    let patch = "@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n@@ -3,2 +3,2 @@\n three\n-four\n+FOUR\n";
    let patch_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "p1".into(),
            name: "apply_patch".into(),
            input: serde_json::json!({"path": "f.txt", "patch": patch}).to_string(),
            provider_meta: None,
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let (holly, _executor) = build_holly_and_executor(root.clone(), vec![patch_call, finish]);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "apply the patch"))
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::ToolRequest { .. })),
        "apply_patch should auto-run under build"
    );
    let on_disk = std::fs::read_to_string(root.join("f.txt")).unwrap();
    assert_eq!(on_disk, "ONE\ntwo\nthree\nFOUR\n");

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
        vec![(
            "f.txt".into(),
            FileChangeKind::ApplyDiff,
            sha256_hex(b"ONE\ntwo\nthree\nFOUR\n")
        )],
    );
}

#[tokio::test]
async fn apply_patch_context_mismatch_leaves_file_untouched_through_engine() {
    let id = std::process::id();
    let root = std::env::temp_dir().join(format!("entanglement-apply-patch-mismatch-{id}"));
    std::fs::create_dir_all(&root).unwrap();
    let _cleanup = CleanupDir(root.clone());
    std::fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();

    let patch = "@@ -1,2 +1,2 @@\n alpha\n-WRONG\n+BETA\n";
    let patch_call = LlmResponse {
        text: "".into(),
        tool_calls: vec![ToolCall {
            id: "p1".into(),
            name: "apply_patch".into(),
            input: serde_json::json!({"path": "f.txt", "patch": patch}).to_string(),
            provider_meta: None,
        }],
    };
    let finish = LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
    };
    let (holly, _executor) = build_holly_and_executor(root.clone(), vec![patch_call, finish]);
    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "apply the patch"))
        .await
        .unwrap();

    let events = collect(sub, &sid).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            OutEvent::ToolOutput { output, .. } if output.contains("context does not match")
        )),
        "expected a context-mismatch error; got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutEvent::FileChange { .. })),
        "a failed apply must not record a FileChange"
    );
    let on_disk = std::fs::read_to_string(root.join("f.txt")).unwrap();
    assert_eq!(on_disk, "alpha\nbeta\n", "file must be untouched on error");
}
