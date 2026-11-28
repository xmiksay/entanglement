//! #934: the epic's core promise as a guard — hundreds of registered tools,
//! reached through a handful of skills, cost the first request nothing; a
//! turn still reaches a deep tool, graded under its own name; a script calls
//! several in one tool call, each graded; the same scripted transcript gives
//! the same events. Offline (a scripted LLM); the live variant is
//! `tests/live_tool_surface.rs` (`make test-live`).

pub mod fixture;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use entanglement_core::{
    stream_from_response, Llm, LlmFactory, LlmRequest, LlmResponse, LlmStream, OutEvent, SessionId,
    ToolCall,
};
use entanglement_runtime::tool_names::{LIST_SKILLS_TOOL, LOAD_SKILL_TOOL, RHAI_TOOL};

use fixture::{deep_tool_names, mode_table, spawn, Scenario};

/// Hard cap on the first request's serialized tools array (names,
/// descriptions, schemas). It holds the one advertised surface only, so it
/// must not move when tools are registered; raise it only for a deliberate
/// change to that surface (about 14.7 KB when #934 landed).
const SURFACE_BUDGET_BYTES: usize = 16 * 1024;

const SYSTEM: &str = "You are the general agent.";

/// Replays `responses` in order, then answers with plain text.
struct Scripted(Arc<Mutex<Vec<LlmResponse>>>);

#[async_trait]
impl Llm for Scripted {
    async fn stream(&mut self, _req: LlmRequest<'_>) -> anyhow::Result<LlmStream> {
        let next = self.0.lock().unwrap().pop();
        Ok(stream_from_response(next.unwrap_or_else(|| LlmResponse {
            text: "done".into(),
            ..Default::default()
        })))
    }
}

fn scripted(mut responses: Vec<LlmResponse>) -> LlmFactory {
    responses.reverse();
    let script = Arc::new(Mutex::new(responses));
    Arc::new(move || Box::new(Scripted(script.clone())) as Box<dyn Llm>)
}

fn call(id: &str, name: &str, input: serde_json::Value) -> LlmResponse {
    LlmResponse {
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.to_string(),
            provider_meta: None,
        }],
        ..Default::default()
    }
}

fn rhai(id: &str, script: &str) -> LlmResponse {
    call(id, RHAI_TOOL, serde_json::json!({ "script": script }))
}

/// Discover the inventory skill, load it, then run `script`.
fn through_the_skill(script: &str) -> Vec<LlmResponse> {
    vec![
        call("c1", LIST_SKILLS_TOOL, serde_json::json!({})),
        call(
            "c2",
            LOAD_SKILL_TOOL,
            serde_json::json!({ "skill_name": "inventory" }),
        ),
        rhai("c3", script),
    ]
}

/// One `ToolOutput` per tool call: `(tool, output, is_error)`.
fn outputs(events: &[OutEvent]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolOutput {
                tool,
                output,
                is_error,
                ..
            } => Some((tool.clone(), output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn output_of(events: &[OutEvent], tool: &str) -> String {
    outputs(events)
        .into_iter()
        .find(|(t, ..)| t == tool)
        .map(|(_, out, _)| out)
        .unwrap_or_else(|| panic!("no {tool} output: {events:#?}"))
}

fn sid() -> SessionId {
    SessionId::new("s1")
}

async fn first_request(with_deep_tools: bool) -> fixture::Seen {
    let s = spawn(
        scripted(vec![]),
        mode_table(&[], &[]),
        with_deep_tools,
        SYSTEM,
    );
    s.turn(&sid(), "hello", false).await;
    let seen = s.seen.lock().unwrap().clone();
    seen
}

#[tokio::test]
async fn three_hundred_registered_tools_leave_the_first_request_unchanged() {
    let deep = deep_tool_names();
    assert!(deep.len() >= 300, "{} deep tools", deep.len());
    let bare = first_request(false).await;
    let loaded = first_request(true).await;

    assert_eq!(
        loaded.first_names, bare.first_names,
        "advertised names moved"
    );
    assert_eq!(
        loaded.first_tool_bytes, bare.first_tool_bytes,
        "the tools array grew with the registry"
    );
    assert_eq!(
        loaded.first_system, bare.first_system,
        "system prompt moved"
    );
    assert!(
        loaded.first_tool_bytes <= SURFACE_BUDGET_BYTES,
        "first request carries {} tool-schema bytes, budget {SURFACE_BUDGET_BYTES}",
        loaded.first_tool_bytes
    );
    for name in &deep {
        assert!(!loaded.first_names.contains(name), "{name} advertised");
        assert!(
            !loaded.first_system.contains(name.as_str()),
            "{name} in prompt"
        );
    }
    // The way in is advertised: the skill roster, the skill loader, the script.
    for door in [LIST_SKILLS_TOOL, LOAD_SKILL_TOOL, RHAI_TOOL] {
        assert!(
            loaded.first_names.iter().any(|n| n == door),
            "{door} missing"
        );
    }
}

async fn reach_stock_level(s: &Scenario) -> Vec<OutEvent> {
    s.turn(&sid(), "stock of A-17 in brno?", true).await
}

#[tokio::test]
async fn a_turn_reaches_a_deep_tool_through_its_skill_graded_under_its_name() {
    let script = r#"inventory::get_stock_level(#{sku: "A-17", warehouse: "brno"})"#;
    let s = spawn(
        scripted(through_the_skill(script)),
        mode_table(&[], &["inventory_get_stock_level"]),
        true,
        SYSTEM,
    );
    let events = reach_stock_level(&s).await;

    let roster = output_of(&events, LIST_SKILLS_TOOL);
    assert!(roster.contains("inventory"), "{roster}");
    let body = output_of(&events, LOAD_SKILL_TOOL);
    assert!(body.contains("inventory::get_stock_level"), "{body}");
    // The rule naming the deep tool parked a card under that very name, not
    // the namespace's backing tool.
    let cards: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::ToolRequest { tool, .. } => Some(tool.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(cards, ["inventory_get_stock_level (rhai)"], "{events:#?}");
    let out = output_of(&events, RHAI_TOOL);
    assert!(out.contains("on_hand") && out.contains("brno"), "{out}");
    assert_eq!(*s.calls.lock().unwrap(), ["inventory_get_stock_level"]);
}

#[tokio::test]
async fn a_denied_deep_tool_never_runs() {
    let script = r#"inventory::get_stock_level(#{sku: "A-17"})"#;
    let s = spawn(
        scripted(through_the_skill(script)),
        mode_table(&["inventory_get_stock_level"], &[]),
        true,
        SYSTEM,
    );
    let events = reach_stock_level(&s).await;
    let out = output_of(&events, RHAI_TOOL);
    assert!(!out.contains("on_hand"), "{out}");
    assert!(s.calls.lock().unwrap().is_empty());
}

/// Three deep tools over two namespaces in one `rhai` call; the mode denies
/// the third by name, so it throws inside the script and the rest ran.
const MULTI: &str = r#"
let a = inventory::get_stock_level(#{sku: "A-17"}).parse_json();
let b = crm::get_contact(#{id: "c1"}).parse_json();
let c = "ran";
try { crm::delete_contact(#{id: "c1"}); } catch (e) { c = "refused"; }
`${a.tool}|${b.tool}|${c}`
"#;

fn multi_call_scenario() -> Scenario {
    spawn(
        scripted(through_the_skill(MULTI)),
        mode_table(&["crm_delete_contact"], &[]),
        true,
        SYSTEM,
    )
}

#[tokio::test]
async fn one_script_calls_several_deep_tools_each_graded() {
    let s = multi_call_scenario();
    let events = reach_stock_level(&s).await;
    let out = output_of(&events, RHAI_TOOL);
    assert!(
        out.contains("inventory_get_stock_level|crm_get_contact|refused"),
        "{out}"
    );
    assert_eq!(
        *s.calls.lock().unwrap(),
        ["inventory_get_stock_level", "crm_get_contact"]
    );
    let nested: Vec<(&str, bool)> = events
        .iter()
        .filter_map(|e| match e {
            OutEvent::NestedCall { tool, is_error, .. } => Some((tool.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(nested.len(), 3, "{nested:?}");
    assert_eq!(
        nested.iter().map(|(_, err)| *err).collect::<Vec<_>>(),
        [false, false, true],
        "{nested:?}"
    );
    let rhai_calls = outputs(&events)
        .iter()
        .filter(|(t, ..)| t == RHAI_TOOL)
        .count();
    assert_eq!(rhai_calls, 1, "one tool call carried all three");
}

#[tokio::test]
async fn the_same_transcript_yields_the_same_events() {
    let first = normalized(&reach_stock_level(&multi_call_scenario()).await);
    let second = normalized(&reach_stock_level(&multi_call_scenario()).await);
    assert!(first.len() > 10, "{first:#?}");
    assert_eq!(first, second);
}

/// The events as JSON with what legitimately differs between runs masked:
/// wall-clock readings (`duration_ms`, `SessionStarted.ts`) and
/// engine-minted ids (unique per process by design), which become `<id N>`
/// in first-seen order.
fn normalized(events: &[OutEvent]) -> Vec<serde_json::Value> {
    let mut ids = Vec::new();
    events
        .iter()
        .map(|e| {
            let mut v = serde_json::to_value(e).expect("an OutEvent serializes");
            mask(&mut v, &mut ids);
            v
        })
        .collect()
}

fn mask(v: &mut serde_json::Value, ids: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key == "duration_ms" || key == "ts" {
                    *value = serde_json::Value::Null;
                } else {
                    mask(value, ids);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|i| mask(i, ids)),
        serde_json::Value::String(s) if is_minted(s) => {
            let n = ids.iter().position(|i| i == s).unwrap_or_else(|| {
                ids.push(s.clone());
                ids.len() - 1
            });
            *s = format!("<id {n}>");
        }
        _ => {}
    }
}

/// `entanglement_core::mint_id`'s shape: `[tenant:]<kind>-<13 hex>`.
fn is_minted(s: &str) -> bool {
    let bare = s.rsplit(':').next().unwrap_or(s);
    let mut parts = bare.splitn(2, '-');
    matches!(
        (parts.next(), parts.next()),
        (Some(kind), Some(hex)) if kind.len() == 1
            && hex.len() == 13
            && hex.chars().all(|c| c.is_ascii_hexdigit())
    )
}
