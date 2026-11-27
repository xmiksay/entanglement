//! Integration test: lifecycle hooks (#199, ADR-0066) wired through the real
//! engine the way `skutter` wires them (#964): `pre_tool_use` as a
//! [`HookResolver`] around the executor's resolver, the other points from
//! [`skutter::hooks::spawn`]'s watcher. A scripted LLM asks for a `read`.
//!
//! - a non-zero `pre_tool_use` hook must **deny** the tool (the file is
//!   never read) — a `rhai` launch or a script's nested call included
//!   ([`rhai`]);
//! - a `post_tool_use` hook runs as a side-effect after a cleared tool;
//! - a `user_prompt_submit` hook fires off the inbound `Prompt`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmRequest, LlmResponse, LlmStream,
    OutEvent, SessionId, ToolCall,
};
use entanglement_runtime::agents::built_in_registry;
use entanglement_runtime::extra_roots::ExtraRootStore;
use entanglement_runtime::mode::ModeTable;
use entanglement_runtime::policy::{ModeResolver, PermissionResolver};
use entanglement_runtime::tool_runner::{EscapeRoot, ToolExecutor};
use skutter::hooks::{CallPaths, HookResolver, HookSpec, Hooks};
use skutter::host::host_tools;
use tokio::task::JoinHandle;

mod rhai;

/// Replays scripted responses, then a plain text reply so the turn loop ends.
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
                ..Default::default()
            })
        };
        Ok(stream_from_response(resp))
    }
}

fn spec(command: &str) -> HookSpec {
    HookSpec {
        command: command.to_string(),
        tools: Vec::new(),
        timeout_secs: 30,
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

/// Scratch dir with a scratch file, self-cleaning on drop.
struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn scratch(tag: &str) -> Scratch {
    let root =
        std::env::temp_dir().join(format!("entanglement-hooks-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("hello.txt"), "greetings\n").unwrap();
    Scratch(root)
}

fn read_then_finish() -> Arc<Vec<LlmResponse>> {
    Arc::new(vec![
        LlmResponse {
            text: "".into(),
            tool_calls: vec![ToolCall {
                id: "r1".into(),
                name: "read".into(),
                input: r#"{"path":"hello.txt"}"#.into(),
                provider_meta: None,
            }],
            ..Default::default()
        },
        LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
            ..Default::default()
        },
    ])
}

fn engine(
    root: &std::path::Path,
    scripted: Arc<Vec<LlmResponse>>,
) -> (Holly, entanglement_runtime::ToolRegistry) {
    let tools = host_tools(root.to_path_buf());
    let cfg = EngineConfig {
        llm_factory: Arc::new(move || {
            Box::new(ScriptedLlm::new((*scripted).clone())) as Box<dyn Llm>
        }),
        tool_specs: tools.specs(),
        agents: built_in_registry().expect("built-in agents must parse"),
        ..EngineConfig::default()
    };
    (Holly::spawn(cfg), tools)
}

/// Poll for `path` to appear (a detached hook writes it), up to `secs`.
async fn wait_for(path: &std::path::Path, secs: u64) -> bool {
    for _ in 0..(secs * 20) {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    path.exists()
}

/// Poll until `path` holds parseable JSON. A hook like `cat > file` truncates the
/// file into existence *before* it finishes writing stdin, so a bare `exists()`
/// check races the content: wait for non-empty, valid JSON rather than mere
/// presence (fixes a flaky empty-file read on slow CI).
async fn read_json_when_ready(path: &std::path::Path, secs: u64) -> serde_json::Value {
    for _ in 0..(secs * 20) {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                return v;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} never became readable JSON", path.display());
}

#[tokio::test]
async fn pre_tool_use_hook_blocks_the_tool() {
    let s = scratch("pre-block");
    let root = s.0.clone();
    let (holly, tools) = engine(&root, read_then_finish());

    // The hook exits non-zero, so the `read` must be vetoed and never run.
    let hooks = Hooks {
        pre_tool_use: vec![spec("echo policy-veto >&2; exit 1")],
        ..Default::default()
    };
    let _exec = wire(&holly, &root, tools, hooks);

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "read it"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    let output = tool_output(&events).expect("a ToolOutput");
    // The veto's reason rides the resolver's `Deny` (#964): the model gets
    // the hook's own output, not the mode-deny text and its `/mode` advice.
    assert_eq!(
        output, "tool `read` blocked by pre_tool_use hook: policy-veto",
        "got: {output}"
    );
    // The blocked tool never read the file.
    assert!(
        !output.contains("greetings"),
        "tool ran despite the veto: {output}"
    );
}

#[tokio::test]
async fn pre_tool_use_hook_exit_zero_lets_the_tool_run() {
    let s = scratch("pre-allow");
    let root = s.0.clone();
    let (holly, tools) = engine(&root, read_then_finish());

    let hooks = Hooks {
        pre_tool_use: vec![spec("exit 0")],
        ..Default::default()
    };
    let _exec = wire(&holly, &root, tools, hooks);

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "read it"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    let output = tool_output(&events).expect("a ToolOutput");
    assert!(
        output.contains("greetings"),
        "cleared hook should let read run: {output}"
    );
}

#[tokio::test]
async fn post_tool_use_hook_runs_after_a_cleared_tool() {
    let s = scratch("post");
    let root = s.0.clone();
    let marker = root.join("post-ran");
    let (holly, tools) = engine(&root, read_then_finish());

    let hooks = Hooks {
        post_tool_use: vec![spec(&format!("touch {}", marker.display()))],
        ..Default::default()
    };
    let _exec = wire(&holly, &root, tools, hooks);

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "read it"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    let output = tool_output(&events).expect("a ToolOutput");
    assert!(
        output.contains("greetings"),
        "tool should have run: {output}"
    );
    assert!(
        wait_for(&marker, 3).await,
        "post_tool_use side-effect never ran"
    );
}

/// Observe-only (#964): a call refused at the gate never ran, so it fires
/// no `post_tool_use`.
#[tokio::test]
async fn post_tool_use_skips_a_refused_call() {
    let s = scratch("post-refused");
    let root = s.0.clone();
    let marker = root.join("post-ran");
    let (holly, tools) = engine(&root, read_then_finish());
    let hooks = Hooks {
        pre_tool_use: vec![spec("exit 1")],
        post_tool_use: vec![spec(&format!("touch {}", marker.display()))],
        ..Default::default()
    };
    let _exec = wire(&holly, &root, tools, hooks);

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "read"))
        .await
        .unwrap();
    let events = collect(sub, &sid).await;

    let output = tool_output(&events).expect("a ToolOutput");
    assert!(output.contains("blocked by pre_tool_use hook"), "{output}");
    assert!(
        !wait_for(&marker, 1).await,
        "post_tool_use fired for a refused call"
    );
}

#[tokio::test]
async fn user_prompt_submit_hook_fires_on_prompt() {
    let s = scratch("prompt");
    let root = s.0.clone();
    let out = root.join("prompt.json");
    // A tool-free turn: the point is the prompt hook, not any tool.
    let (holly, tools) = engine(
        &root,
        Arc::new(vec![LlmResponse {
            text: "hi".into(),
            tool_calls: vec![],
            ..Default::default()
        }]),
    );

    let hooks = Hooks {
        user_prompt_submit: vec![spec(&format!("cat > {}", out.display()))],
        ..Default::default()
    };
    let _exec = wire(&holly, &root, tools, hooks);

    let sid = SessionId::new("s1");
    let sub = holly.subscribe();
    holly
        .send(InMsg::prompt(sid.clone(), "please help"))
        .await
        .unwrap();
    let _ = collect(sub, &sid).await;

    let v = read_json_when_ready(&out, 3).await;
    assert_eq!(v["event"], "user_prompt_submit");
    assert_eq!(v["prompt"], "please help");
}

type Wired = (JoinHandle<()>, Option<JoinHandle<()>>);

/// The executor plus `skutter`'s two hook seams, as `services.rs` wires them,
/// rooted at `root`.
fn wire(
    holly: &Holly,
    root: &std::path::Path,
    tools: entanglement_runtime::ToolRegistry,
    hooks: Hooks,
) -> Wired {
    wire_trusted(holly, root, tools, hooks, Vec::new())
}

/// [`wire`] with `rhai.trusted` folders (project layer).
fn wire_trusted(
    holly: &Holly,
    root: &std::path::Path,
    tools: entanglement_runtime::ToolRegistry,
    hooks: Hooks,
    trusted: Vec<std::path::PathBuf>,
) -> Wired {
    let root = root.canonicalize().expect("scratch root resolves");
    let hooks = Arc::new(hooks);
    let tools = tools.shared();
    let modes = Arc::new(Mutex::new(HashMap::new()));
    let table = Arc::new(ModeTable::builtin().expect("built-in modes parse"));
    let inner: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        modes.clone(),
        table.clone(),
        tools.clone(),
        allow_all(),
        Some(root.clone()),
    ));
    let paths = CallPaths {
        tools: tools.clone(),
        root: root.clone(),
    };
    let watcher = skutter::hooks::spawn(holly, hooks.clone(), paths);
    let exec = ToolExecutor::builder(holly, tools)
        .agents(built_in_registry().expect("built-in agents must parse"))
        .base(allow_all())
        .perm_modes(modes)
        .mode_table(table)
        .escape_root(EscapeRoot {
            root,
            store: Arc::new(ExtraRootStore::ephemeral()),
        })
        .rhai_trusted(trusted)
        .resolver(Arc::new(HookResolver::new(inner, hooks)))
        .spawn();
    (exec, watcher)
}

fn allow_all() -> entanglement_core::PermissionProfile {
    entanglement_core::PermissionProfile::new(entanglement_core::Permission::Allow)
}

fn tool_output(events: &[OutEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        OutEvent::ToolOutput { output, .. } => Some(output.clone()),
        _ => None,
    })
}
