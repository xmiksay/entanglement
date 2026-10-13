//! Session working directories end to end (#764, ADR-0224): the runtime's
//! resolver validates `/cd`, the executor runs `bash`/`call`/`rhai` there and
//! resolves relative path arguments against it, while grading, containment
//! and the escape-root gate keep seeing root-relative paths.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, ApprovalScope, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, SessionId, ToolCall,
};
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::host::{host_tools_with, BashTool, CallTool, HostToolOptions};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::tool_runner::{EscapeRoot, ToolExecutor};
use entanglement_runtime::workdir::{self, SessionWorkdirs};
use tokio::sync::broadcast::Receiver;

use crate::mode_support::{allow_all_table, perm_modes};

/// Sends every scripted call as one batch, then ends the turn.
struct Batch {
    calls: Vec<ToolCall>,
    sent: bool,
}

#[async_trait]
impl Llm for Batch {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let tool_calls = if self.sent {
            Vec::new()
        } else {
            self.sent = true;
            self.calls.clone()
        };
        Ok(stream_from_response(LlmResponse {
            text: if tool_calls.is_empty() { "done" } else { "" }.into(),
            tool_calls,
            ..Default::default()
        }))
    }
}

fn call(id: &str, name: &str, input: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        input: input.to_string(),
        provider_meta: None,
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    holly: Holly,
    sid: SessionId,
}

/// A canonical project root with `top.txt` and `sub/{a,e,p}.txt`, a sibling
/// `outside/` directory, and the real host tools + executor wired the way
/// `engine_setup`/`services` wire them.
fn harness(calls: Vec<ToolCall>, mode_table: Arc<ModeTable>) -> Harness {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = dir.path().canonicalize().expect("canonical tempdir");
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("top.txt"), "top\n").unwrap();
    std::fs::write(root.join("sub/a.txt"), "alpha\n").unwrap();
    std::fs::write(root.join("sub/e.txt"), "one\n").unwrap();
    std::fs::write(root.join("sub/p.txt"), "x\ny\n").unwrap();
    std::fs::write(outside.join("o.txt"), "outer\n").unwrap();

    let store = Arc::new(ExtraRootStore::ephemeral());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(Batch {
                calls: calls.clone(),
                sent: false,
            }) as Box<dyn Llm>
        }),
        workdir_resolver: Some(workdir::resolver(root.clone(), Some(store.clone()))),
        ..EngineConfig::default()
    };
    let holly = Holly::spawn(cfg);
    let workdirs = SessionWorkdirs::default();
    let mut reg = host_tools_with(
        root.clone(),
        HostToolOptions {
            extra_roots: Some(store.clone()),
            backups: None,
            workdirs: Some(workdirs.clone()),
        },
    );
    reg.register(BashTool::new(root.clone()).with_extra_roots(store.clone()));
    reg.register(CallTool::new(root.clone()).with_extra_roots(store.clone()));
    let _executor = ToolExecutor::builder(&holly, reg.shared())
        .perm_modes(perm_modes())
        .mode_table(mode_table)
        .escape_root(EscapeRoot {
            root: root.clone(),
            store,
        })
        .workdirs(workdirs)
        .spawn();
    Harness {
        _dir: dir,
        root,
        outside,
        holly,
        sid: SessionId::new("s1"),
    }
}

async fn next(sub: &mut Receiver<OutEvent>) -> Option<OutEvent> {
    tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .ok()?
        .ok()
}

impl Harness {
    /// `/cd` and wait for the outcome: `Ok(dir)` or the refusal message.
    async fn cd(&self, dir: &Path, approve: Option<ApprovalScope>) -> Result<String, String> {
        let mut sub = self.holly.subscribe();
        self.holly
            .send(InMsg::SetWorkdir {
                session: self.sid.clone(),
                dir: dir.to_string_lossy().into_owned(),
                approve,
            })
            .await
            .unwrap();
        while let Some(ev) = next(&mut sub).await {
            match ev {
                OutEvent::WorkdirChanged { dir, .. } => return Ok(dir),
                OutEvent::Error { message, .. } => return Err(message),
                _ => {}
            }
        }
        panic!("no answer to SetWorkdir");
    }

    /// Run one turn, rejecting any approval prompt; returns every event.
    async fn turn(&self) -> Vec<OutEvent> {
        let mut sub = self.holly.subscribe();
        self.holly
            .send(InMsg::prompt(self.sid.clone(), "go"))
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Some(ev) = next(&mut sub).await {
            if let OutEvent::ToolRequest {
                request_id, tool, ..
            } = &ev
            {
                // A `rhai` launch card (ADR-0228) is answered Ask per
                // command, so the script's nested calls meet the gates
                // under test; every other card is rejected.
                let msg = if tool == "rhai" {
                    InMsg::Approve {
                        session: self.sid.clone(),
                        request_id: request_id.clone(),
                        scope: Default::default(),
                        mode: None,
                        run_all: false,
                    }
                } else {
                    InMsg::Reject {
                        session: self.sid.clone(),
                        request_id: request_id.clone(),
                        reason: None,
                    }
                };
                self.holly.send(msg).await.unwrap();
            }
            let done = matches!(ev, OutEvent::Done { .. });
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }
}

/// `(output, is_error)` of call `id`.
fn output(events: &[OutEvent], id: &str) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                request_id,
                output,
                is_error,
                ..
            } if request_id == id => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no output for {id}: {events:?}"))
}

fn requests(events: &[OutEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolRequest { input, .. } => Some(input.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn bash_and_call_run_in_the_session_workdir() {
    let h = harness(
        vec![
            call("b", "bash", serde_json::json!({"command": "pwd"})),
            call("c", "call", serde_json::json!({"command": "pwd"})),
        ],
        allow_all_table(),
    );
    let sub = h.root.join("sub");
    assert_eq!(
        h.cd(Path::new("sub"), None).await,
        Ok(sub.display().to_string())
    );
    let events = h.turn().await;
    for id in ["b", "c"] {
        let (out, is_error) = output(&events, id);
        assert!(
            !is_error && out.contains(&sub.display().to_string()),
            "{id}: {out}"
        );
    }
}

#[tokio::test]
async fn path_tools_resolve_relative_paths_against_the_workdir() {
    let h = harness(
        vec![
            call("r", "read", serde_json::json!({"path": "a.txt"})),
            call(
                "w",
                "write",
                serde_json::json!({"path": "new.txt", "content": "n\n"}),
            ),
            call(
                "e",
                "edit",
                serde_json::json!({"path": "e.txt", "oldString": "one", "newString": "two"}),
            ),
            call(
                "p",
                "apply_patch",
                serde_json::json!({"path": "p.txt", "patch": "@@ -1,2 +1,2 @@\n x\n-y\n+z\n"}),
            ),
            call("g", "glob", serde_json::json!({"pattern": "a.*"})),
            call("s", "grep", serde_json::json!({"pattern": "alpha"})),
        ],
        allow_all_table(),
    );
    h.cd(Path::new("sub"), None).await.expect("cd sub");
    let events = h.turn().await;
    assert!(output(&events, "r").0.contains("alpha"));
    let sub = h.root.join("sub");
    assert_eq!(std::fs::read_to_string(sub.join("new.txt")).unwrap(), "n\n");
    assert_eq!(std::fs::read_to_string(sub.join("e.txt")).unwrap(), "two\n");
    assert_eq!(
        std::fs::read_to_string(sub.join("p.txt")).unwrap(),
        "x\nz\n"
    );
    assert!(
        !h.root.join("new.txt").exists(),
        "nothing lands at the root"
    );
    // Listed relative to the workdir, so a path passes straight back.
    assert_eq!(output(&events, "g").0.trim(), "a.txt");
    assert!(
        output(&events, "s").0.starts_with("a.txt:1:alpha"),
        "{}",
        output(&events, "s").0
    );
}

/// Grading still sees the root-relative path a call really names (ADR-0125):
/// `read(sub/*)` admits `a.txt` from `sub`, but not `../top.txt`.
#[tokio::test]
async fn permission_arguments_stay_root_relative() {
    let mode = Mode {
        name: "build".into(),
        default: Permission::Deny,
        rules: Rules::from_lists(&[], &["read(sub/*)".to_string()], &[]),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    let h = harness(
        vec![
            call("in", "read", serde_json::json!({"path": "a.txt"})),
            call("up", "read", serde_json::json!({"path": "../top.txt"})),
        ],
        Arc::new(ModeTable::new(vec![mode]).unwrap()),
    );
    h.cd(Path::new("sub"), None).await.expect("cd sub");
    let events = h.turn().await;
    let (inside, inside_err) = output(&events, "in");
    assert!(!inside_err && inside.contains("alpha"), "{inside}");
    let (up, up_err) = output(&events, "up");
    assert!(up_err && up.contains("denied"), "{up}");
}

#[tokio::test]
async fn a_workdir_outside_the_root_needs_an_approval_once() {
    let h = harness(
        vec![call("b", "bash", serde_json::json!({"command": "pwd"}))],
        allow_all_table(),
    );
    let refused = h.cd(&h.outside, None).await.unwrap_err();
    assert!(refused.contains("outside the project root"), "{refused}");
    let missing = h.cd(Path::new("no-such-dir"), None).await.unwrap_err();
    assert!(missing.contains("no-such-dir"), "{missing}");

    let approved = h.cd(&h.outside, Some(ApprovalScope::Session)).await;
    assert_eq!(approved, Ok(h.outside.display().to_string()));
    let events = h.turn().await;
    assert!(
        requests(&events).is_empty(),
        "the grant covers the exec cwd"
    );
    let (out, _) = output(&events, "b");
    assert!(out.contains(&h.outside.display().to_string()), "{out}");
    // Asked once: back to the root and out again needs no new approval.
    h.cd(&h.root, None).await.expect("cd root");
    assert!(h.cd(&h.outside, None).await.is_ok());
}

/// Containment is re-checked at every use (ADR-0054): a workdir swapped for a
/// symlink out of the root after `/cd` trips the escape-root gate.
#[cfg(unix)]
#[tokio::test]
async fn a_workdir_swapped_for_a_symlink_is_caught_at_use() {
    let h = harness(
        vec![call("b", "bash", serde_json::json!({"command": "pwd"}))],
        allow_all_table(),
    );
    h.cd(Path::new("sub"), None).await.expect("cd sub");
    let sub = h.root.join("sub");
    std::fs::rename(&sub, h.root.join("sub-old")).unwrap();
    std::os::unix::fs::symlink(&h.outside, &sub).unwrap();

    let events = h.turn().await;
    let asked = requests(&events);
    assert!(
        asked.len() == 1 && asked[0].contains("OUTSIDE the project root"),
        "{asked:?}"
    );
    let (out, is_error) = output(&events, "b");
    assert!(is_error, "rejected, never run: {out}");
    assert!(!out.contains(&h.outside.display().to_string()), "{out}");
}

#[cfg(feature = "rhai")]
#[tokio::test]
async fn rhai_exec_bindings_default_to_the_workdir() {
    let h = harness(
        vec![call(
            "x",
            "rhai",
            serde_json::json!({"script": "exec(\"pwd\") + read(\"a.txt\")"}),
        )],
        allow_all_table(),
    );
    h.cd(Path::new("sub"), None).await.expect("cd sub");
    let events = h.turn().await;
    let (out, is_error) = output(&events, "x");
    assert!(!is_error, "{out}");
    assert!(
        out.contains(&h.root.join("sub").display().to_string()) && out.contains("alpha"),
        "{out}"
    );
}

/// The `/cd` grant covers the exec cwd only: a file tool reaching into the
/// out-of-root workdir still asks for its own escape-root approval.
#[tokio::test]
async fn a_file_tool_in_an_approved_outside_workdir_still_asks() {
    let h = harness(
        vec![call("r", "read", serde_json::json!({"path": "o.txt"}))],
        allow_all_table(),
    );
    h.cd(&h.outside, Some(ApprovalScope::Session))
        .await
        .expect("approved cd");
    let events = h.turn().await;
    let asked = requests(&events);
    assert!(
        asked.len() == 1 && asked[0].contains("OUTSIDE the project root"),
        "{asked:?}"
    );
    let (out, is_error) = output(&events, "r");
    assert!(
        is_error && !out.contains("outer"),
        "rejected, never read: {out}"
    );
}

/// `call`'s `input_file`/`output_file` in an out-of-root workdir ride the same
/// durable `call` grant its cwd does.
#[tokio::test]
async fn call_files_in_an_approved_outside_workdir_resolve() {
    let h = harness(
        vec![call(
            "c",
            "call",
            serde_json::json!({"command": "cat", "input_file": "o.txt", "output_file": "copy.txt"}),
        )],
        allow_all_table(),
    );
    h.cd(&h.outside, Some(ApprovalScope::Session))
        .await
        .expect("approved cd");
    let events = h.turn().await;
    let (out, is_error) = output(&events, "c");
    assert!(!is_error, "{out}");
    assert_eq!(
        std::fs::read_to_string(h.outside.join("copy.txt")).unwrap(),
        "outer\n"
    );
}
