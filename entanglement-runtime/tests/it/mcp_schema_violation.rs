//! End-to-end coverage for ADR-0226's `mcp` envelope landing in pre-dispatch
//! argument validation (ADR-0196 §6): a real engine **unwraps** a well-formed
//! `mcp {server, tool, args}` call, so `arg_validate` grades the *inner*
//! `mcp__<server>__<tool>` call against the tool's real schema and its
//! decline threads back out through a real `ToolOutput` — the violation never
//! reaches the server. The unit tests in `src/arg_validate/tests.rs` only
//! cover the formatter; this pins the path to it.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest, LlmResponse,
    LlmStream, OutEvent, Permission, PermissionProfile, SessionId, ToolCall,
};
use entanglement_runtime::host::WriteTool;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::AdvertisingState;
use entanglement_runtime::tool_runner::{DiscoverySurface, ToolExecutor};
use entanglement_runtime::{system_prompt_mode, Tool, ToolRegistry};

/// A connected MCP server's `save_file` (registered as it would be after a
/// connect): `path`/`content` required. Reaching `run` would mean the
/// violation went to the server.
struct SaveFile;
#[async_trait]
impl Tool for SaveFile {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("mcp__docs__save_file")
    }
    fn description(&self) -> &str {
        "save a file"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("saved: {input}"))
    }
}

/// A second MCP tool (requires `text`) for the batch test.
struct Label;
#[async_trait]
impl Tool for Label {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("mcp__docs__label")
    }
    fn description(&self) -> &str {
        "attach a label"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        })
    }
    async fn run(&self, input: &str) -> anyhow::Result<String> {
        Ok(format!("labeled: {input}"))
    }
}

struct Scripted {
    responses: Arc<Mutex<Vec<LlmResponse>>>,
}
#[async_trait]
impl Llm for Scripted {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let resp = self.responses.lock().unwrap().pop().unwrap_or(LlmResponse {
            text: "done".into(),
            tool_calls: vec![],
            ..Default::default()
        });
        Ok(stream_from_response(resp))
    }
}

struct Harness {
    holly: Holly,
    sid: SessionId,
    _executor: tokio::task::JoinHandle<()>,
}

/// Wired exactly like `boot.rs`/`advertising_pin::harness`: the production
/// `tool_spec_resolver` advertises the `mcp` envelope, so core really unwraps
/// a well-formed call into `mcp__docs__<tool>`. `mcp__docs__save_file`/
/// `mcp__docs__label` stand in for a connected server's tools; the real
/// `write` covers the native path.
fn harness(write_root: PathBuf, script: Vec<LlmResponse>) -> Harness {
    let mut script = script;
    script.reverse();
    let responses = Arc::new(Mutex::new(script));
    let factory: LlmFactory = {
        let responses = responses.clone();
        Arc::new(move || {
            Box::new(Scripted {
                responses: responses.clone(),
            }) as Box<dyn Llm>
        })
    };
    let profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let advertising = Arc::new(AdvertisingState::new());
    let mut reg = ToolRegistry::new();
    reg.register(WriteTool::new(write_root));
    reg.register(SaveFile);
    reg.register(Label);
    let tools = reg.shared();
    let holly = Holly::spawn(EngineConfig {
        llm_factory: factory,
        agents: profiles.clone(),
        tool_spec_resolver: Some(tool_spec_resolver(SurfaceSources {
            tools: tools.clone(),
            advertising: advertising.clone(),
            agent_specs: entanglement_runtime::subagent::agent_specs(),
        })),
        system_prompt_resolver: Some(system_prompt_mode::resolver(advertising.clone())),
        ..EngineConfig::default()
    });
    let base = PermissionProfile::new(Permission::Allow);
    let perm_modes = crate::mode_support::perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        crate::mode_support::allow_all_table(),
        tools.clone(),
        base.clone(),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load_from(None));
    let executor = ToolExecutor::builder(&holly, tools.clone())
        .agents(profiles)
        .base(base)
        .perm_modes(perm_modes)
        .resolver(resolver)
        .grants(grants)
        .discovery(DiscoverySurface {
            advertising,
            ..Default::default()
        })
        .spawn();
    Harness {
        holly,
        sid: SessionId::new("s1"),
        _executor: executor,
    }
}

impl Harness {
    /// Send a prompt and collect this session's events up to `Done` — the
    /// scripted LLM keeps replying across as many rounds as `script` has
    /// entries, so one call drives an entire multi-round exchange.
    async fn turn(&self, text: &str) -> Vec<OutEvent> {
        let mut sub = self.holly.subscribe();
        self.holly
            .send(InMsg::prompt(self.sid.clone(), text))
            .await
            .unwrap();
        let mut events = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), sub.recv()).await {
                Ok(Ok(ev)) if ev.session() == Some(&self.sid) => {
                    let done = matches!(ev, OutEvent::Done { .. });
                    events.push(ev);
                    if done {
                        return events;
                    }
                }
                Ok(Ok(_)) => {}
                other => panic!("turn did not finish: {other:?}"),
            }
        }
    }
}

fn call(id: &str, name: &str, input: &str) -> LlmResponse {
    LlmResponse {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            provider_meta: None,
        }],
        ..Default::default()
    }
}

fn batch(calls: &[(&str, &str, &str)]) -> LlmResponse {
    LlmResponse {
        text: String::new(),
        tool_calls: calls
            .iter()
            .map(|(id, name, input)| ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                input: (*input).into(),
                provider_meta: None,
            })
            .collect(),
        ..Default::default()
    }
}

fn text() -> LlmResponse {
    LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
        ..Default::default()
    }
}

/// This `request_id`'s `(output, is_error)`.
fn output_for<'a>(events: &'a [OutEvent], request_id: &str) -> (&'a str, bool) {
    events
        .iter()
        .find_map(|e| match e {
            OutEvent::ToolOutput {
                request_id: rid,
                output,
                is_error,
                ..
            } if rid == request_id => Some((output.as_str(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no ToolOutput for {request_id}; got {events:?}"))
}

/// The example section of an `mcp` tool's decline — only the leading JSON
/// value, since the loop-breaker note can follow it on a repeat violation.
fn mcp_example(output: &str) -> serde_json::Value {
    let raw = output
        .split("example call (through mcp):\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no mcp example section: {output}"));
    serde_json::Deserializer::from_str(raw.trim())
        .into_iter::<serde_json::Value>()
        .next()
        .unwrap_or_else(|| panic!("no JSON value: {raw}"))
        .unwrap_or_else(|e| panic!("example not JSON ({e}): {raw}"))
}

/// `serde_json::Value` silently keeps the *later* `path`, dropping `content`,
/// so validation reports a missing `content` — the duplicate key is the real
/// mistake, recovered by re-scanning the emitted envelope.
const DUP_KEY_SAVE_FILE: &str =
    r#"{"server":"docs","tool":"save_file","args":{"path":"file.md","path":"data"}}"#;

#[tokio::test]
async fn an_mcp_violation_names_the_fault_shows_the_schema_and_never_reaches_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(
        dir.path().to_path_buf(),
        vec![call("w1", "mcp", DUP_KEY_SAVE_FILE), text()],
    );
    let events = h.turn("go").await;
    let (output, is_error) = output_for(&events, "w1");
    assert!(is_error);
    assert!(!output.contains("saved:"), "reached the server: {output}");
    assert!(output.contains("missing required: content"), "{output}");
    assert!(
        output.contains(
            r#"duplicate key "path" (the later value silently replaced the earlier one)"#
        ),
        "{output}"
    );
    assert!(output.contains("correct usage"), "{output}");
    assert!(
        output.contains("\"name\": \"mcp__docs__save_file\""),
        "{output}"
    );
    let example = mcp_example(output);
    assert_eq!(example["server"], "docs");
    assert_eq!(example["tool"], "save_file");
    assert_eq!(example["args"]["path"], "example");
    assert_eq!(example["args"]["content"], "example");
}

/// The first decline delivered the schema into this session's transcript, so
/// an identical second violation points back instead of resending it.
#[tokio::test]
async fn a_repeated_mcp_violation_points_back_to_the_delivered_schema() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(
        dir.path().to_path_buf(),
        vec![
            call("w1", "mcp", DUP_KEY_SAVE_FILE),
            call("w2", "mcp", DUP_KEY_SAVE_FILE),
            text(),
        ],
    );
    let events = h.turn("go").await;
    let (first, _) = output_for(&events, "w1");
    assert!(first.contains("correct usage"), "{first}");
    let (second, is_error) = output_for(&events, "w2");
    assert!(is_error);
    assert!(second.contains("already provided above"), "{second}");
    assert!(!second.contains("correct usage"), "{second}");
}

#[tokio::test]
async fn a_batch_of_two_bad_mcp_calls_each_get_a_complete_reply() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(
        dir.path().to_path_buf(),
        vec![
            batch(&[
                ("w1", "mcp", DUP_KEY_SAVE_FILE),
                ("l1", "mcp", r#"{"server":"docs","tool":"label","args":{}}"#),
            ]),
            text(),
        ],
    );
    let events = h.turn("go").await;
    let (save_out, save_err) = output_for(&events, "w1");
    assert!(save_err);
    assert!(save_out.contains("correct usage"), "{save_out}");
    assert_eq!(mcp_example(save_out)["tool"], "save_file");
    let (label_out, label_err) = output_for(&events, "l1");
    assert!(label_err);
    assert!(label_out.contains("missing required: text"), "{label_out}");
    assert!(label_out.contains("correct usage"), "{label_out}");
    assert_eq!(mcp_example(label_out)["tool"], "label");
}

#[tokio::test]
async fn a_native_violation_shows_a_native_example_and_points_back_on_repeat() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(
        dir.path().to_path_buf(),
        vec![
            call("w1", "write", r#"{"path":"file.md"}"#),
            call("w2", "write", r#"{"path":"file.md"}"#),
            text(),
        ],
    );
    let events = h.turn("go").await;
    let (first, is_error1) = output_for(&events, "w1");
    assert!(is_error1);
    assert!(first.contains("correct usage"), "{first}");
    assert!(first.contains("example call:\n"), "{first}");
    let (second, is_error2) = output_for(&events, "w2");
    assert!(is_error2);
    assert!(second.contains("already provided above"), "{second}");
}
