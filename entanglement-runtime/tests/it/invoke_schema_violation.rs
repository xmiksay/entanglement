//! End-to-end coverage for ADR-0204's `invoke` fallback landing in
//! pre-dispatch argument validation (#560 remainder): a real engine that
//! actually **unwraps** a well-formed `invoke {name, args}` call, so
//! `arg_validate` grades the *inner* call and its decline threads back out
//! through a real `ToolOutput`. `decline_text_for_invoke_wraps_only_the_
//! example_call` (`entanglement-runtime/src/arg_validate/tests.rs`) only unit
//! tests the formatter — several ADR-0204 bugs were "the function is correct
//! in isolation, the path to it at runtime is not", so this module wires the
//! production resolvers (`tool_advertising::surface`, mirroring
//! `advertising_pin::harness`) rather than hand-building specs, so core's
//! `unwrap_batch` sees the same `invoke` spec the binary would advertise.
//!
//! The task fixture names its tool `write` — but the real `write` tool is a
//! member of `TOOL_SEARCH_KERNEL` (`tool_names.rs`), so `client_side_surface`
//! advertises it directly in `tools` under *every* discovery strategy,
//! `invoke` included (ADR-0204 §2: only a *non*-kernel tool is invoke-only).
//! Wrapping a kernel tool's example, or exempting its repeat violation from
//! suppression, would therefore be wrong — its schema genuinely is in
//! `tools` every round. `save_file` below is a schema-identical stand-in
//! (same `path`/`content` shape as `write`) that is *not* kernel, so it
//! actually exercises the invoke-only path the task describes; the real
//! `write` tool is used instead for the native-asymmetry test (6), where its
//! kernel status only reinforces the point.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, Catalog, EngineConfig, Holly, InMsg, Llm, LlmFactory, LlmRequest,
    LlmResponse, LlmStream, OutEvent, Permission, PermissionProfile, ResolvedModel, SessionId,
    ToolCall,
};
use entanglement_runtime::config::Config;
use entanglement_runtime::host::WriteTool;
use entanglement_runtime::mcp::AvailableMcp;
use entanglement_runtime::plan_files::PlanFileRegistry;
use entanglement_runtime::policy::{
    DefaultGrantStore, GrantStore, ModeResolver, PermissionResolver,
};
use entanglement_runtime::skills::SkillRegistry;
use entanglement_runtime::tool_advertising::surface::{tool_spec_resolver, SurfaceSources};
use entanglement_runtime::tool_advertising::{AdvertisingInputs, AdvertisingState};
use entanglement_runtime::tool_runner::{spawn_tool_executor_with_policy, DiscoverySurface};
use entanglement_runtime::{system_prompt_mode, Tool, ToolRegistry};

/// One provider `inv`/model `m` pinned to `discovery: invoke` — the array
/// never carries `save_file`/`label`, only the kernel + `invoke` (ADR-0204 §2).
const INVOKE_CATALOG: &str = "providers:\n\
  \x20 - name: inv\n\
  \x20   default_model: m\n\
  \x20   discovery: invoke\n\
  \x20   models:\n\
  \x20     - id: m\n";

/// A `discovery: append` counterpart — direct (non-`invoke`) native calls,
/// used to pin the asymmetry: repeating a *native* violation still gets the
/// "already provided above" short form.
const APPEND_CATALOG: &str = "providers:\n\
  \x20 - name: nat\n\
  \x20   default_model: m\n\
  \x20   discovery: append\n\
  \x20   models:\n\
  \x20     - id: m\n";

/// A non-kernel stand-in for `write` (module doc): same `path`/`content`
/// required shape, but not in `TOOL_SEARCH_KERNEL`, so under `discovery:
/// invoke` its schema is genuinely reachable only through a decline — the
/// path items 1/3/4/5 exercise.
struct SaveFile;
#[async_trait]
impl Tool for SaveFile {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("save_file")
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

/// A second schema'd tool (requires `text`) for the batch test — two
/// different tools reached through `invoke` in the same round.
struct Label;
#[async_trait]
impl Tool for Label {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("label")
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
        });
        Ok(stream_from_response(resp))
    }
}

fn load_config() -> Config {
    let _env = crate::env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("ENTANGLEMENT_CONFIG_FILE", dir.path().join("absent.yml"));
    let config = Config::load(dir.path()).expect("config loads");
    std::env::remove_var("ENTANGLEMENT_CONFIG_FILE");
    config
}

struct Harness {
    holly: Holly,
    sid: SessionId,
    advertising: Arc<AdvertisingState>,
    _executor: tokio::task::JoinHandle<()>,
}

/// Wired exactly like `main.rs`/`advertising_pin::harness`: production
/// `tool_spec_resolver` + `system_prompt_resolver` over one `AdvertisingState`,
/// so a session's pinned `discovery` — and therefore whether `invoke` is
/// advertised and core unwraps it — comes from the catalog, not a hand-set
/// test flag. Registers `save_file`/`label` (this module's fixtures) plus the
/// real `write` (for the item-6 native-kernel test).
fn harness(
    provider: &str,
    model: &str,
    catalog_yaml: &str,
    write_root: PathBuf,
    script: Vec<LlmResponse>,
) -> Harness {
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

    let mut profiles =
        entanglement_runtime::agents::built_in_registry().expect("built-in agents must parse");
    let mut general = profiles.get("general").cloned().expect("general profile");
    general.provider = Some(provider.into());
    general.model = Some(model.into());
    profiles.insert(general);

    let advertising = Arc::new(AdvertisingState::new());
    let mut reg = ToolRegistry::new();
    reg.register(WriteTool::new(write_root));
    reg.register(SaveFile);
    reg.register(Label);
    let tools = reg.shared();
    let resolved_factory = factory.clone();
    let catalog: Catalog = serde_yaml::from_str(catalog_yaml).expect("catalog yaml");
    let inputs = Arc::new(AdvertisingInputs::new(
        Arc::new(load_config()),
        Some(Arc::new(catalog)),
    ));
    let holly = Holly::spawn(EngineConfig {
        llm_factory: factory,
        agents: profiles.clone(),
        model_resolver: Some(Arc::new(move |_user, provider: &str, model: &str| {
            Ok(ResolvedModel {
                provider: provider.into(),
                model: model.into(),
                llm_factory: resolved_factory.clone(),
                generation: None,
                context_window: None,
            })
        })),
        tool_spec_resolver: Some(tool_spec_resolver(SurfaceSources {
            tools: tools.clone(),
            avail: Arc::new(AvailableMcp::default()),
            advertising: advertising.clone(),
            inputs: inputs.clone(),
            agent_specs: entanglement_runtime::subagent::agent_specs(&profiles),
        })),
        system_prompt_resolver: Some(system_prompt_mode::resolver(advertising.clone())),
        ..EngineConfig::default()
    });

    let base = PermissionProfile::new(Permission::Allow);
    let active = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let perm_modes = crate::mode_support::perm_modes();
    let resolver: Arc<dyn PermissionResolver> = Arc::new(ModeResolver::new(
        perm_modes.clone(),
        crate::mode_support::allow_all_table(),
        tools.clone(),
        base.clone(),
        None,
    ));
    let grants: Arc<dyn GrantStore> = Arc::new(DefaultGrantStore::load());
    let executor = spawn_tool_executor_with_policy(
        &holly,
        tools.clone(),
        entanglement_runtime::host::jobs::JobRegistry::new(),
        entanglement_runtime::retained_output::RetainedOutputRegistry::new(),
        entanglement_runtime::script_ops::ScriptRegistry::new(),
        Arc::new(RwLock::new(profiles)),
        Arc::new(RwLock::new(Arc::new(SkillRegistry::default()))),
        base,
        active,
        perm_modes,
        resolver,
        grants,
        Default::default(),
        None,
        Arc::new(
            entanglement_runtime::mode::ModeTable::builtin()
                .expect("built-in permission modes must parse"),
        ),
        Arc::new(PlanFileRegistry::new()),
        None,
        Some(inputs),
        Some(DiscoverySurface {
            advertising: advertising.clone(),
            ..Default::default()
        }),
    );
    Harness {
        holly,
        sid: SessionId::new("s1"),
        advertising,
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
    }
}

fn text() -> LlmResponse {
    LlmResponse {
        text: "done".into(),
        tool_calls: vec![],
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

/// The `{"name": ..., "args": {...}}` example section of a decline message —
/// only the leading JSON value, since the loop-breaker note (`LOOP_BREAKER_
/// NOTE`) can follow it in the same string on a repeat violation.
fn invoke_example(output: &str) -> serde_json::Value {
    let raw = output
        .split("example call:\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no example section: {output}"));
    serde_json::Deserializer::from_str(raw.trim())
        .into_iter::<serde_json::Value>()
        .next()
        .unwrap_or_else(|| panic!("no JSON value: {raw}"))
        .unwrap_or_else(|e| panic!("example not JSON ({e}): {raw}"))
}

/// The task's fixture, retargeted at `save_file` (module doc): `invoke {
/// "name": "save_file", "args": { "path": "file.md", "path": "data" } }` —
/// `serde_json::Value` silently keeps the *later* `path`, dropping `content`
/// entirely, so validation reports a missing-`content` violation. The
/// duplicate key is the actual mistake.
const DUP_KEY_SAVE_FILE: &str = r#"{"name":"save_file","args":{"path":"file.md","path":"data"}}"#;

#[tokio::test]
async fn invoke_violation_names_missing_field_and_duplicate_key() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(
        "inv",
        "m",
        INVOKE_CATALOG,
        dir.path().to_path_buf(),
        vec![call("w1", "invoke", DUP_KEY_SAVE_FILE), text()],
    );
    let events = h.turn("go").await;
    assert_eq!(
        h.advertising.discovery(&h.sid),
        entanglement_core::Discovery::Invoke
    );
    let (output, is_error) = output_for(&events, "w1");

    // 1. names the missing `content` and the duplicate `path`.
    assert!(output.contains("missing required: content"), "{output}");
    assert!(
        output.contains(
            r#"duplicate key "path" (the later value silently replaced the earlier one)"#
        ),
        "{output}"
    );
    assert!(is_error);

    // 2. the reply carries `save_file`'s JSON schema.
    assert!(output.contains("correct usage"), "{output}");
    assert!(output.contains("\"name\": \"save_file\""), "{output}");
    assert!(output.contains("\"content\""), "{output}");

    // 3. the example is in `invoke` form.
    let example = invoke_example(output);
    assert_eq!(example["name"], "save_file");
    assert_eq!(example["args"]["path"], "example");
    assert_eq!(example["args"]["content"], "example");
}

#[tokio::test]
async fn repeated_invoke_violation_still_returns_the_full_schema() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(
        "inv",
        "m",
        INVOKE_CATALOG,
        dir.path().to_path_buf(),
        vec![
            call("w1", "invoke", DUP_KEY_SAVE_FILE),
            call("w2", "invoke", DUP_KEY_SAVE_FILE),
            text(),
        ],
    );
    let events = h.turn("go").await;

    // 4. the *second* identical invoke violation is not suppressed: the
    // schema is not in `tools`, so "already provided above" would point the
    // model at nothing.
    for id in ["w1", "w2"] {
        let (output, is_error) = output_for(&events, id);
        assert!(is_error);
        assert!(output.contains("correct usage"), "{id}: {output}");
        assert!(!output.contains("already provided above"), "{id}: {output}");
        let example = invoke_example(output);
        assert_eq!(example["name"], "save_file");
    }
}

#[tokio::test]
async fn a_batch_of_two_bad_invoke_calls_each_get_a_complete_reply() {
    let dir = tempfile::tempdir().unwrap();
    let h = harness(
        "inv",
        "m",
        INVOKE_CATALOG,
        dir.path().to_path_buf(),
        vec![
            batch(&[
                ("w1", "invoke", DUP_KEY_SAVE_FILE),
                ("l1", "invoke", r#"{"name":"label","args":{}}"#),
            ]),
            text(),
        ],
    );
    let events = h.turn("go").await;

    // 5. two different tools, one round, both bad: neither suppresses the
    // other's schema.
    let (save_out, save_err) = output_for(&events, "w1");
    assert!(save_err);
    assert!(save_out.contains("correct usage"), "{save_out}");
    assert_eq!(invoke_example(save_out)["name"], "save_file");

    let (label_out, label_err) = output_for(&events, "l1");
    assert!(label_err);
    assert!(label_out.contains("missing required: text"), "{label_out}");
    assert!(label_out.contains("correct usage"), "{label_out}");
    assert_eq!(invoke_example(label_out)["name"], "label");
}

#[tokio::test]
async fn native_repeat_violation_still_says_already_provided_above() {
    // 6. pin the asymmetry: under `discovery: append` a call reaches the real
    // `write` tool by its real name (no `invoke` wrapper) — its schema *is*
    // in `tools` every round (doubly so: it's both a native call and a
    // kernel tool), so the second identical violation stays suppressed. This
    // must never be unified with the `invoke` behavior above.
    let dir = tempfile::tempdir().unwrap();
    let h = harness(
        "nat",
        "m",
        APPEND_CATALOG,
        dir.path().to_path_buf(),
        vec![
            call("w1", "write", r#"{"path":"file.md"}"#),
            call("w2", "write", r#"{"path":"file.md"}"#),
            text(),
        ],
    );
    let events = h.turn("go").await;
    assert_eq!(
        h.advertising.discovery(&h.sid),
        entanglement_core::Discovery::Append
    );

    let (first, is_error1) = output_for(&events, "w1");
    assert!(is_error1);
    assert!(first.contains("correct usage"), "{first}");

    let (second, is_error2) = output_for(&events, "w2");
    assert!(is_error2);
    assert!(
        second.contains("already provided above"),
        "second native violation should point back: {second}"
    );
    assert!(
        !second.contains("correct usage"),
        "second native violation must not resend the schema: {second}"
    );
}
