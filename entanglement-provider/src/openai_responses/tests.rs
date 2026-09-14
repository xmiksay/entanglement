//! Request-construction goldens for the Responses API client (mirrors
//! `openai/tests.rs`'s style). SSE parsing is tested in `sse.rs` directly.

use serde_json::json;

use super::request::{build_body, convert_messages};
use super::TOOL_SEARCH_CALL_TOOL;
use crate::{
    ContentPart, GenerationParams, Message, MessageRole, ReasoningEffort, ToolCall, ToolSpec,
};

fn msg(role: MessageRole, text: &str) -> Message {
    Message {
        role,
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(text)]
        },
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

#[test]
fn body_carries_instructions_input_and_omits_tools_when_empty() {
    let body = build_body(
        "gpt-5.4",
        "be helpful",
        &[msg(MessageRole::User, "hi")],
        &[],
        None,
    );
    assert_eq!(body["model"], "gpt-5.4");
    assert_eq!(body["instructions"], "be helpful");
    assert_eq!(body["stream"], true);
    assert_eq!(body["input"], json!([{ "role": "user", "content": "hi" }]));
    assert!(body.get("tools").is_none());
}

#[test]
fn empty_system_omits_instructions() {
    let body = build_body("gpt-5.4", "", &[msg(MessageRole::User, "hi")], &[], None);
    assert!(body.get("instructions").is_none());
}

#[test]
fn generation_params_map_to_responses_field_names() {
    let body = build_body(
        "gpt-5.4",
        "",
        &[],
        &[],
        Some(GenerationParams {
            temperature: Some(0.4),
            max_output_tokens: Some(2048),
            thinking_budget_tokens: None,
            reasoning_effort: Some(ReasoningEffort::High),
        }),
    );
    assert_eq!(body["temperature"].as_f64().unwrap(), 0.4_f32 as f64);
    // Responses' field is `max_output_tokens`, never Chat Completions' `max_tokens`.
    assert_eq!(body["max_output_tokens"], 2048);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["reasoning"], json!({ "effort": "high" }));
}

#[test]
fn tools_are_flat_function_entries_with_defer_loading_passthrough() {
    let mut deferred = ToolSpec::new("search_files", "search the workspace");
    deferred.defer_loading = true;
    let kernel = ToolSpec::new("read", "read a file");
    let body = build_body("gpt-5.4", "", &[], &[kernel, deferred], None);
    let tools = body["tools"].as_array().unwrap();
    // Flat shape: type/name/description/parameters directly on the entry,
    // never nested under a `function` key (unlike Chat Completions).
    let read_entry = tools.iter().find(|t| t["name"] == "read").unwrap();
    assert_eq!(read_entry["type"], "function");
    assert_eq!(read_entry["description"], "read a file");
    assert!(read_entry.get("defer_loading").is_none());
    let deferred_entry = tools.iter().find(|t| t["name"] == "search_files").unwrap();
    assert_eq!(deferred_entry["defer_loading"], true);
}

#[test]
fn tool_search_entry_is_declared_only_when_a_tool_is_deferred() {
    let plain = ToolSpec::new("read", "read a file");
    let body = build_body("gpt-5.4", "", &[], &[plain], None);
    let tools = body["tools"].as_array().unwrap();
    assert!(!tools.iter().any(|t| t["type"] == "tool_search"));

    let mut deferred = ToolSpec::new("search_files", "search the workspace");
    deferred.defer_loading = true;
    let body = build_body("gpt-5.4", "", &[], &[deferred], None);
    let tools = body["tools"].as_array().unwrap();
    let search = tools
        .iter()
        .find(|t| t["type"] == "tool_search")
        .expect("tool_search entry present once a tool is deferred");
    assert_eq!(search["execution"], "client");
    assert_eq!(search["parameters"]["required"][0], "query");
}

#[test]
fn function_call_and_output_pair_by_call_id() {
    let history = vec![
        msg(MessageRole::User, "what's the weather in Paris?"),
        Message::assistant_content(
            vec![],
            vec![ToolCall::new(
                "call_1",
                "get_weather",
                r#"{"city":"Paris"}"#,
            )],
        ),
        Message::tool("call_1", "18C, clear"),
    ];
    let items = convert_messages(&history);
    assert_eq!(
        items[0],
        json!({ "role": "user", "content": "what's the weather in Paris?" })
    );
    assert_eq!(
        items[1],
        json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "get_weather",
            "arguments": r#"{"city":"Paris"}"#,
        })
    );
    assert_eq!(
        items[2],
        json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "18C, clear",
        })
    );
}

#[test]
fn tool_search_call_and_output_pair_uses_the_native_item_types() {
    // The assistant's call: a ToolCall named the reserved sentinel, as the
    // SSE parser would have produced from a `tool_search_call` output item.
    let call = ToolCall::new(
        "call_9",
        TOOL_SEARCH_CALL_TOOL,
        r#"{"query":"weather tools"}"#,
    );
    let reply = ContentPart::tool_search_output(
        "openai_responses",
        "discovered: get_weather",
        json!([{ "type": "function", "name": "get_weather", "defer_loading": true }]),
    );
    let history = vec![
        Message::assistant_content(vec![], vec![call]),
        Message::tool_content("call_9", vec![reply]),
    ];
    let items = convert_messages(&history);
    assert_eq!(
        items[0],
        json!({
            "type": "tool_search_call",
            "execution": "client",
            "call_id": "call_9",
            "status": "completed",
            "arguments": { "query": "weather tools" },
        })
    );
    assert_eq!(
        items[1],
        json!({
            "type": "tool_search_output",
            "execution": "client",
            "call_id": "call_9",
            "status": "completed",
            "tools": [{ "type": "function", "name": "get_weather", "defer_loading": true }],
        })
    );
}

#[test]
fn a_tool_search_output_minted_by_a_foreign_wire_degrades_to_text() {
    // History replayed after a live `/model` switch onto this wire from a
    // different `responses_native`-tagged provider (or, in practice today, a
    // constructed test double) — the `provider` tag doesn't match this
    // wire's `WIRE_NAME`, so it must fold to its portable `summary` text
    // inside a plain `function_call_output`, never a native
    // `tool_search_output` (which would echo a foreign, meaningless
    // `call_id`/`tools` shape).
    let reply = ContentPart::tool_search_output(
        "some_other_provider",
        "discovered: get_weather",
        json!([{ "type": "function", "name": "get_weather" }]),
    );
    let history = vec![Message::tool_content("call_5", vec![reply])];
    let items = convert_messages(&history);
    assert_eq!(
        items[0],
        json!({
            "type": "function_call_output",
            "call_id": "call_5",
            "output": "discovered: get_weather",
        })
    );
}

#[test]
fn a_tool_reference_block_degrades_to_text_in_function_call_output() {
    let history = vec![Message::tool_content(
        "call_3",
        vec![ContentPart::tool_reference("search_files")],
    )];
    let items = convert_messages(&history);
    assert_eq!(
        items[0]["output"],
        crate::tool_reference_fallback_text("search_files")
    );
}

#[test]
fn image_tool_result_follows_as_a_separate_user_message() {
    let history = vec![Message::tool_content(
        "call_7",
        vec![ContentPart::image("image/png", "AAAA")],
    )];
    let items = convert_messages(&history);
    assert_eq!(items[0]["type"], "function_call_output");
    assert_eq!(items[0]["output"], "");
    assert_eq!(items[1]["role"], "user");
    let parts = items[1]["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], "input_image");
    assert!(parts[0]["image_url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
}

#[test]
fn assistant_text_only_round_trips_without_a_tool_call_item() {
    let history = vec![Message::assistant("all done", vec![])];
    let items = convert_messages(&history);
    assert_eq!(
        items,
        vec![json!({ "role": "assistant", "content": "all done" })]
    );
}

#[test]
fn a_provider_search_block_appends_to_assistant_text() {
    let history = vec![Message::assistant_content(
        vec![
            ContentPart::text("here's what I found"),
            ContentPart::provider_search("openai_responses", "[web] rust async", json!({})),
        ],
        vec![],
    )];
    let items = convert_messages(&history);
    let content = items[0]["content"].as_str().unwrap();
    assert!(content.starts_with("here's what I found"));
    assert!(content.contains("[web] rust async"));
}
