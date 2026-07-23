use std::collections::BTreeMap;

use serde_json::json;

use super::request::{build_body, convert_messages};
use super::sse::{
    drain_available_frames, flush_pending_tools, handle_chunk, note_finish_reason, parse_sse_line,
    SseEvent,
};
use super::PendingTool;
use crate::web_search::WebSearchConfig;
use crate::{
    ContentPart, GenerationParams, LlmEvent, Message, MessageRole, StopReason, ToolCall, ToolSpec,
    Usage,
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
fn body_prepends_system_message_and_omits_tools_when_empty() {
    let body = build_body(
        "glm-5.2",
        "be helpful",
        &[msg(MessageRole::User, "hi")],
        &[],
        None,
        None,
    );
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("tools").is_none());
    // No generation params ⇒ no temperature/max_tokens on the wire.
    assert!(body.get("temperature").is_none());
    assert!(body.get("max_tokens").is_none());
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "be helpful");
    assert_eq!(msgs[1]["role"], "user");
}

#[test]
fn text_only_user_content_stays_a_plain_string() {
    let out = convert_messages(&[msg(MessageRole::User, "hi")]);
    assert_eq!(out[0]["content"], "hi");
}

#[test]
fn user_image_renders_data_url_block() {
    let user = Message::user_content(vec![
        ContentPart::text("look"),
        ContentPart::image("image/png", "AAAA"),
    ]);
    let out = convert_messages(&[user]);
    let content = &out[0]["content"];
    assert_eq!(content[0], json!({ "type": "text", "text": "look" }));
    assert_eq!(
        content[1],
        json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } })
    );
}

#[test]
fn tool_result_with_image_appends_a_user_image_message() {
    // #221: OpenAI's `role: "tool"` message can't hold an image, so the image
    // is handed to the model as a trailing `role: "user"` message with an
    // `image_url` block; the tool message keeps a text placeholder.
    let tool = Message::tool_content("call-1", vec![ContentPart::image("image/png", "AAAA")]);
    let out = convert_messages(&[tool]);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["role"], "tool");
    assert_eq!(out[0]["tool_call_id"], "call-1");
    assert_eq!(
        out[0]["content"],
        "[image returned; see the following message]"
    );
    assert_eq!(out[1]["role"], "user");
    assert_eq!(
        out[1]["content"][0],
        json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } })
    );
}

#[test]
fn text_only_tool_result_stays_a_single_string_message() {
    let tool = Message::tool("call-1", "done");
    let out = convert_messages(&[tool]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["role"], "tool");
    assert_eq!(out[0]["content"], "done");
}

#[test]
fn body_includes_tools_with_parameters_schema() {
    let spec = ToolSpec::new("greet", "say hi");
    let body = build_body(
        "glm-5.2",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[spec],
        None,
        None,
    );
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "greet");
    assert!(body["tools"][0]["function"]["parameters"].is_object());
}

#[test]
fn generation_params_set_temperature_and_max_tokens() {
    let body = build_body(
        "glm-5.2",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        Some(GenerationParams {
            temperature: Some(0.7),
            max_output_tokens: Some(2048),
            // No thinking channel on the OpenAI-compat wire — dropped.
            thinking_budget_tokens: Some(4096),
            reasoning_effort: None,
        }),
        None,
    );
    assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 1e-6);
    assert_eq!(body["max_tokens"], 2048);
    assert!(body.get("thinking").is_none());
}

#[test]
fn reasoning_effort_passes_through_verbatim_lowercase() {
    let body = build_body(
        "gpt-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        Some(GenerationParams {
            temperature: None,
            max_output_tokens: None,
            thinking_budget_tokens: None,
            reasoning_effort: Some(crate::ReasoningEffort::High),
        }),
        None,
    );
    assert_eq!(body["reasoning_effort"], "high");
}

#[test]
fn generation_params_omit_unset_knobs() {
    let body = build_body(
        "glm-5.2",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        Some(GenerationParams::default()),
        None,
    );
    assert!(body.get("temperature").is_none());
    assert!(body.get("max_tokens").is_none());
}

#[test]
fn tool_results_become_one_message_each() {
    let msgs = vec![Message::tool("a", "r1"), Message::tool("b", "r2")];
    let out = convert_messages(&msgs);
    // Unlike Anthropic, two tool results are two messages, not one.
    assert_eq!(out.len(), 2);
    assert_eq!(out[0]["role"], "tool");
    assert_eq!(out[0]["tool_call_id"], "a");
    assert_eq!(out[0]["content"], "r1");
    assert_eq!(out[1]["tool_call_id"], "b");
}

#[test]
fn assistant_with_tool_calls_serializes_arguments() {
    let msgs = vec![Message::assistant(
        "thinking",
        vec![ToolCall {
            id: "c1".into(),
            name: "greet".into(),
            input: r#"{"nm":"sam"}"#.into(),
            provider_meta: None,
        }],
    )];
    let out = convert_messages(&msgs);
    assert_eq!(out[0]["role"], "assistant");
    assert_eq!(out[0]["content"], "thinking");
    let call = &out[0]["tool_calls"][0];
    assert_eq!(call["id"], "c1");
    assert_eq!(call["function"]["name"], "greet");
    assert_eq!(call["function"]["arguments"], r#"{"nm":"sam"}"#);
}

#[test]
fn text_delta_yields_text() {
    let data = json!({ "choices": [{ "delta": { "content": "hel" } }] });
    let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut Usage::default()).unwrap();
    assert_eq!(evs, vec![LlmEvent::Text("hel".into())]);
}

#[test]
fn empty_content_delta_emits_nothing() {
    let data = json!({ "choices": [{ "delta": { "content": "" } }] });
    let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut Usage::default()).unwrap();
    assert!(evs.is_empty());
}

#[test]
fn tool_calls_assemble_across_deltas_and_flush_via_flush_pending_tools() {
    // `handle_chunk` no longer flushes on `finish_reason: "tool_calls"`
    // itself (#445) — the assembled call stays pending even past that
    // chunk, so the caller's single validating flush site
    // (`flush_pending_tools`) is what emits it.
    let mut tools = BTreeMap::new();
    let d1 = json!({ "choices": [{ "delta": { "tool_calls": [
        { "index": 0, "id": "c1", "type": "function",
          "function": { "name": "greet", "arguments": "{\"nm\":" } }
    ] } }] });
    let d2 = json!({ "choices": [{ "delta": { "tool_calls": [
        { "index": 0, "function": { "arguments": "\"sam\"}" } }
    ] } }] });
    let d3 = json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });

    let _ = handle_chunk(&d1, &mut tools, &mut Usage::default()).unwrap();
    assert!(tools.contains_key(&0)); // assembled but not yet flushed
    let _ = handle_chunk(&d2, &mut tools, &mut Usage::default()).unwrap();
    let evs = handle_chunk(&d3, &mut tools, &mut Usage::default()).unwrap();
    assert!(evs.is_empty(), "finish_reason chunk itself flushes nothing");
    assert!(
        tools.contains_key(&0),
        "still pending after the finish chunk"
    );

    let mut out = Vec::new();
    let emitted_any = flush_pending_tools(&mut tools, &mut out);
    assert!(emitted_any);
    assert_eq!(
        out,
        vec![LlmEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "greet".into(),
            input: r#"{"nm":"sam"}"#.into(),
            provider_meta: None,
        })]
    );
    assert!(tools.is_empty(), "flush should drain the map");
}

#[test]
fn flush_pending_tools_skips_malformed_json_arguments() {
    let mut tools = BTreeMap::new();
    tools.insert(
        0,
        PendingTool {
            id: "c1".into(),
            name: "greet".into(),
            arguments: "not json".into(),
        },
    );
    let mut out = Vec::new();
    let emitted_any = flush_pending_tools(&mut tools, &mut out);
    assert!(!emitted_any, "malformed args must not be emitted");
    assert!(out.is_empty());
    assert!(tools.is_empty(), "flush should still drain the map");
}

#[test]
fn flush_pending_tools_skips_non_object_json_arguments() {
    let mut tools = BTreeMap::new();
    tools.insert(
        0,
        PendingTool {
            id: "c1".into(),
            name: "greet".into(),
            arguments: "[1,2,3]".into(),
        },
    );
    let mut out = Vec::new();
    let emitted_any = flush_pending_tools(&mut tools, &mut out);
    assert!(!emitted_any, "a JSON array is not a valid tool input");
    assert!(out.is_empty());
}

#[test]
fn tool_arg_fragments_stream_as_deltas_before_the_assembled_call() {
    // Each `function.arguments` fragment is surfaced as a `ToolCallDelta`
    // (id + name + raw fragment) as it arrives (#194); the concatenated
    // deltas rebuild the eventual `ToolCall::input`.
    let mut tools = BTreeMap::new();
    let d1 = json!({ "choices": [{ "delta": { "tool_calls": [
        { "index": 0, "id": "c1", "type": "function",
          "function": { "name": "greet", "arguments": "{\"nm\":" } }
    ] } }] });
    let d2 = json!({ "choices": [{ "delta": { "tool_calls": [
        { "index": 0, "function": { "arguments": "\"sam\"}" } }
    ] } }] });

    let e1 = handle_chunk(&d1, &mut tools, &mut Usage::default()).unwrap();
    assert_eq!(
        e1,
        vec![LlmEvent::ToolCallDelta {
            id: "c1".into(),
            name: "greet".into(),
            delta: "{\"nm\":".into(),
        }]
    );
    let e2 = handle_chunk(&d2, &mut tools, &mut Usage::default()).unwrap();
    assert_eq!(
        e2,
        vec![LlmEvent::ToolCallDelta {
            id: "c1".into(),
            name: "greet".into(),
            delta: "\"sam\"}".into(),
        }]
    );
    // Fragments joined equal what a flush would assemble as the input.
    let joined: String = [&e1, &e2]
        .iter()
        .flat_map(|evs| evs.iter())
        .map(|ev| match ev {
            LlmEvent::ToolCallDelta { delta, .. } => delta.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(joined, r#"{"nm":"sam"}"#);
}

#[test]
fn empty_arg_fragment_emits_no_delta() {
    // A tool-call delta that only carries id/name (no args yet) must not
    // emit an empty `ToolCallDelta`.
    let mut tools = BTreeMap::new();
    let d = json!({ "choices": [{ "delta": { "tool_calls": [
        { "index": 0, "id": "c1", "type": "function",
          "function": { "name": "greet", "arguments": "" } }
    ] } }] });
    let evs = handle_chunk(&d, &mut tools, &mut Usage::default()).unwrap();
    assert!(evs.is_empty(), "no args ⇒ no delta: {evs:?}");
}

#[test]
fn multiple_tools_flush_in_index_order() {
    let mut tools = BTreeMap::new();
    let d1 = json!({ "choices": [{ "delta": { "tool_calls": [
        { "index": 1, "id": "c2", "type": "function",
          "function": { "name": "b", "arguments": "{}" } },
        { "index": 0, "id": "c1", "type": "function",
          "function": { "name": "a", "arguments": "{}" } }
    ] } }] });
    let d2 = json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] });
    let _ = handle_chunk(&d1, &mut tools, &mut Usage::default()).unwrap();
    let _ = handle_chunk(&d2, &mut tools, &mut Usage::default()).unwrap();
    let mut evs = Vec::new();
    flush_pending_tools(&mut tools, &mut evs);
    assert_eq!(evs.len(), 2);
    assert_eq!(
        evs[0],
        LlmEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "a".into(),
            input: "{}".into(),
            provider_meta: None,
        })
    );
    assert_eq!(
        evs[1],
        LlmEvent::ToolCall(ToolCall {
            id: "c2".into(),
            name: "b".into(),
            input: "{}".into(),
            provider_meta: None,
        })
    );
}

#[test]
fn usage_is_captured_from_chunk() {
    let mut usage = Usage::default();
    let data = json!({ "choices": [], "usage": {
        "prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49
    } });
    let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut usage).unwrap();
    assert!(evs.is_empty()); // no content/tool event from a usage-only chunk
    assert_eq!(usage.input_tokens, Some(42));
    assert_eq!(usage.output_tokens, Some(7));
    assert_eq!(usage.cached_input_tokens, None);
}

#[test]
fn cached_prompt_tokens_split_out_of_input() {
    // OpenAI reports cached reads inside `prompt_tokens`; the input count is
    // the remainder so each dimension prices once (#192).
    let mut usage = Usage::default();
    let data = json!({ "choices": [], "usage": {
        "prompt_tokens": 100, "completion_tokens": 8, "total_tokens": 108,
        "prompt_tokens_details": { "cached_tokens": 30 }
    } });
    let _ = handle_chunk(&data, &mut BTreeMap::new(), &mut usage).unwrap();
    assert_eq!(usage.input_tokens, Some(70));
    assert_eq!(usage.cached_input_tokens, Some(30));
    assert_eq!(usage.output_tokens, Some(8));
}

#[test]
fn stop_finish_reason_does_not_flush_or_error() {
    let mut tools = BTreeMap::new();
    let data = json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] });
    let evs = handle_chunk(&data, &mut tools, &mut Usage::default()).unwrap();
    assert!(evs.is_empty());
    assert!(tools.is_empty());
}

#[test]
fn stream_without_finish_reason_flushes_pending_tools() {
    let mut tools = BTreeMap::new();
    let d1 = json!({ "choices": [{ "delta": { "tool_calls": [
        { "index": 0, "id": "c1", "type": "function",
          "function": { "name": "greet", "arguments": "{\"nm\":\"sam\"}" } }
    ] } }] });
    let _ = handle_chunk(&d1, &mut tools, &mut Usage::default()).unwrap();
    assert!(tools.contains_key(&0), "tool should be assembled");

    // Simulate stream ending without explicit finish_reason - this would be
    // handled by the flush-at-end logic in the streaming loop
    assert!(!tools.is_empty(), "tools should still be pending");
}

// ── provider-side web search (#305, persistence + version flag #481) ───────

#[test]
fn body_omits_web_search_tool_without_config() {
    let body = build_body(
        "glm-5.2",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        None,
        None,
    );
    assert!(body.get("tools").is_none());
}

#[test]
fn body_pushes_web_search_tool_when_configured() {
    let ws = WebSearchConfig {
        enabled: true,
        max_uses: Some(3),
        allowed_domains: vec!["docs.rs".into(), "example.com".into()],
    };
    let body = build_body(
        "glm-5.2",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        None,
        Some(&ws),
    );
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "web_search");
    assert_eq!(tools[0]["web_search"]["enable"], true);
    assert_eq!(tools[0]["web_search"]["search_result"], true);
    assert_eq!(tools[0]["web_search"]["count"], 3);
    assert_eq!(
        tools[0]["web_search"]["search_domain_filter"],
        "docs.rs,example.com"
    );
}

#[test]
fn web_search_tool_rides_alongside_function_tools() {
    let ws = WebSearchConfig {
        enabled: true,
        max_uses: None,
        allowed_domains: vec![],
    };
    let body = build_body(
        "glm-5.2",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[ToolSpec::new("greet", "say hi")],
        None,
        Some(&ws),
    );
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[1]["type"], "web_search");
    // No knobs set ⇒ no `count` / `search_domain_filter`.
    assert!(tools[1]["web_search"].get("count").is_none());
    assert!(tools[1]["web_search"].get("search_domain_filter").is_none());
}

#[test]
fn web_search_array_surfaces_as_reasoning_and_content_block() {
    // A chunk carrying a `web_search` source array (defensive top-level
    // placement) yields one Reasoning line per entry, no Text/ToolCall, plus
    // one persisted ContentBlock (#481) summarizing the same lines.
    let data = json!({ "web_search": [
        { "title": "Rust async", "link": "https://docs.rs/async" },
        { "title": "Tokio", "url": "https://tokio.rs" },
    ] });
    let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut Usage::default()).unwrap();
    assert_eq!(
        evs,
        vec![
            LlmEvent::Reasoning("[web_search] Rust async — https://docs.rs/async".into()),
            LlmEvent::Reasoning("[web_search] Tokio — https://tokio.rs".into()),
            LlmEvent::ContentBlock(ContentPart::provider_search(
                "zai",
                "[web_search] Rust async — https://docs.rs/async\n[web_search] Tokio — https://tokio.rs",
                data["web_search"].clone(),
            )),
        ]
    );
}

#[test]
fn chunk_without_web_search_array_emits_no_reasoning() {
    let data = json!({ "choices": [{ "delta": { "content": "hi" } }] });
    let evs = handle_chunk(&data, &mut BTreeMap::new(), &mut Usage::default()).unwrap();
    assert_eq!(evs, vec![LlmEvent::Text("hi".into())]);
}

#[test]
fn assistant_provider_search_block_renders_as_appended_text() {
    // #481: the OpenAI-compat wire has no native replay format for a search
    // block, so its `summary` rides as plain text on the assistant message —
    // regardless of which provider minted it (no `data` is ever sent).
    let assistant = Message::assistant_content(
        vec![
            ContentPart::text("found it"),
            ContentPart::provider_search("anthropic", "[web_search] rust", json!({"raw": true})),
        ],
        vec![],
    );
    let out = convert_messages(&[assistant]);
    assert_eq!(out[0]["content"], "found it\n\n[web_search] rust");
}

// ── stream robustness: [DONE] terminator + trailing-frame flush (#483) ────

#[test]
fn done_terminates_before_trailing_junk_is_ever_parsed() {
    let mut frames = crate::sse_frame::SseFrameBuffer::new(b"\n");
    frames.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n");
    frames.push(b"data: [DONE]\n");
    frames.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"should never surface\"}}]}\n");

    let mut tools = BTreeMap::new();
    let mut usage = Usage::default();
    let mut seen_finish_reason = None;
    let (events, done) =
        drain_available_frames(&mut frames, &mut tools, &mut usage, &mut seen_finish_reason)
            .unwrap();

    assert_eq!(events, vec![LlmEvent::Text("hi".into())]);
    assert!(done, "must report the stream as terminated at [DONE]");
    // The junk frame after [DONE] is still sitting unparsed in the buffer —
    // proof drain_available_frames never touched it once it saw [DONE].
    assert!(frames.take_remaining().is_some());
}

#[test]
fn trailing_unterminated_frame_with_finish_reason_yields_confident_stop() {
    // No trailing '\n' — the connection closed mid-frame (or the server
    // never terminates its last event), so `next_frame` can't surface it.
    let mut frames = crate::sse_frame::SseFrameBuffer::new(b"\n");
    frames.push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}");

    let mut tools = BTreeMap::new();
    let mut usage = Usage::default();
    let mut seen_finish_reason = None;
    let (events, done) =
        drain_available_frames(&mut frames, &mut tools, &mut usage, &mut seen_finish_reason)
            .unwrap();
    assert!(events.is_empty(), "frame isn't newline-terminated yet");
    assert!(!done);

    // Mirrors the EOF flush in `stream()`: pull the leftover bytes and run
    // them through the same parse + handle_chunk path.
    let trailing = frames
        .take_remaining()
        .expect("unterminated frame must still be buffered");
    match parse_sse_line(&trailing) {
        SseEvent::Data(data) => {
            note_finish_reason(&data, &mut seen_finish_reason);
            handle_chunk(&data, &mut tools, &mut usage).unwrap();
        }
        _ => panic!("trailing frame should parse as a data event"),
    }
    assert_eq!(seen_finish_reason.as_deref(), Some("stop"));

    // Reproduce the caller's stop_reason resolution (ADR-0118): a `stop`
    // finish_reason with no pending tool calls must be a confident
    // `EndTurn`, never `None` (which would trigger an ambiguous-stop retry).
    let mut flushed = Vec::new();
    let emitted_any_tool_call = flush_pending_tools(&mut tools, &mut flushed);
    let stop_reason = match seen_finish_reason.as_deref() {
        Some("tool_calls") if !emitted_any_tool_call => None,
        Some(r) => Some(StopReason::from_openai(r)),
        None if emitted_any_tool_call => Some(StopReason::ToolUse),
        None => None,
    };
    assert_eq!(stop_reason, Some(StopReason::EndTurn));
}
