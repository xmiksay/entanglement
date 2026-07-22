use super::*;

#[allow(clippy::type_complexity)]
fn call(
    event: &str,
    data: Option<Value>,
    current_tool: &mut Option<PendingTool>,
    current_text: &mut Option<String>,
    assembled_blocks: &mut Vec<Value>,
) -> Vec<LlmEvent> {
    handle_frame(
        event,
        data,
        current_tool,
        current_text,
        assembled_blocks,
        &mut Usage::default(),
        &mut None,
        &mut false,
    )
    .unwrap()
}

#[test]
fn text_delta_yields_text() {
    let data = json!({ "delta": { "type": "text_delta", "text": "hel" } });
    let evs = call(
        "content_block_delta",
        Some(data),
        &mut None,
        &mut None,
        &mut Vec::new(),
    );
    assert_eq!(evs, vec![LlmEvent::Text("hel".into())]);
}

#[test]
fn text_block_accumulates_into_assembled_blocks() {
    let mut tool = None;
    let mut text = None;
    let mut blocks = Vec::new();
    call(
        "content_block_start",
        Some(json!({ "content_block": { "type": "text" } })),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    call(
        "content_block_delta",
        Some(json!({ "delta": { "type": "text_delta", "text": "hel" } })),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    call(
        "content_block_delta",
        Some(json!({ "delta": { "type": "text_delta", "text": "lo" } })),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    call(
        "content_block_stop",
        None,
        &mut tool,
        &mut text,
        &mut blocks,
    );
    assert_eq!(blocks, vec![json!({ "type": "text", "text": "hello" })]);
}

#[test]
fn tool_block_assembles_across_deltas() {
    let start = json!({
        "content_block": { "type": "tool_use", "id": "t1", "name": "greet", "input": {} }
    });
    let d1 = json!({ "delta": { "type": "input_json_delta", "partial_json": "{\"nm\":" } });
    let d2 = json!({ "delta": { "type": "input_json_delta", "partial_json": "\"sam\"}" } });

    let mut tool = None;
    let mut text = None;
    let mut blocks = Vec::new();
    call(
        "content_block_start",
        Some(start),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    call(
        "content_block_delta",
        Some(d1),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    call(
        "content_block_delta",
        Some(d2),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    let evs = call(
        "content_block_stop",
        None,
        &mut tool,
        &mut text,
        &mut blocks,
    );
    assert_eq!(
        evs,
        vec![LlmEvent::ToolCall(crate::ToolCall {
            id: "t1".into(),
            name: "greet".into(),
            input: r#"{"nm":"sam"}"#.into(),
            provider_meta: None,
        })]
    );
    assert_eq!(
        blocks,
        vec![json!({ "type": "tool_use", "id": "t1", "name": "greet", "input": { "nm": "sam" } })]
    );
}

#[test]
fn input_json_deltas_stream_as_tool_call_deltas() {
    // Each `input_json_delta` is surfaced as a `ToolCallDelta` (id + name +
    // raw fragment) as it arrives (#194), and the block still finalizes into
    // the assembled `ToolCall` on `content_block_stop`.
    let start = json!({
        "content_block": { "type": "tool_use", "id": "t1", "name": "greet", "input": {} }
    });
    let d1 = json!({ "delta": { "type": "input_json_delta", "partial_json": "{\"nm\":" } });
    let d2 = json!({ "delta": { "type": "input_json_delta", "partial_json": "\"sam\"}" } });

    let mut tool = None;
    let mut text = None;
    let mut blocks = Vec::new();
    call(
        "content_block_start",
        Some(start),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    let e1 = call(
        "content_block_delta",
        Some(d1),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    assert_eq!(
        e1,
        vec![LlmEvent::ToolCallDelta {
            id: "t1".into(),
            name: "greet".into(),
            delta: "{\"nm\":".into(),
        }]
    );
    let e2 = call(
        "content_block_delta",
        Some(d2),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    assert_eq!(
        e2,
        vec![LlmEvent::ToolCallDelta {
            id: "t1".into(),
            name: "greet".into(),
            delta: "\"sam\"}".into(),
        }]
    );
    let stop = call(
        "content_block_stop",
        None,
        &mut tool,
        &mut text,
        &mut blocks,
    );
    assert_eq!(
        stop,
        vec![LlmEvent::ToolCall(crate::ToolCall {
            id: "t1".into(),
            name: "greet".into(),
            input: r#"{"nm":"sam"}"#.into(),
            provider_meta: None,
        })]
    );
}

#[test]
fn usage_is_captured_from_frames() {
    let mut usage = Usage::default();
    let mut stop = None;
    let mut pause = false;
    let _ = handle_frame(
        "message_start",
        Some(json!({ "message": { "usage": {
            "input_tokens": 42,
            "cache_read_input_tokens": 10,
            "cache_creation_input_tokens": 5
        } } })),
        &mut None,
        &mut None,
        &mut Vec::new(),
        &mut usage,
        &mut stop,
        &mut pause,
    )
    .unwrap();
    let _ = handle_frame(
        "message_delta",
        Some(json!({ "delta": { "stop_reason": "max_tokens" }, "usage": { "output_tokens": 7 } })),
        &mut None,
        &mut None,
        &mut Vec::new(),
        &mut usage,
        &mut stop,
        &mut pause,
    )
    .unwrap();
    assert_eq!(usage.input_tokens, Some(42));
    assert_eq!(usage.output_tokens, Some(7));
    assert_eq!(usage.cached_input_tokens, Some(10));
    assert_eq!(usage.cache_write_tokens, Some(5));
    assert_eq!(stop, Some(StopReason::MaxTokens));
    assert!(!pause);
}

#[test]
fn pause_turn_stop_reason_sets_the_flag() {
    let mut usage = Usage::default();
    let mut stop = None;
    let mut pause = false;
    let _ = handle_frame(
        "message_delta",
        Some(json!({ "delta": { "stop_reason": "pause_turn" }, "usage": {} })),
        &mut None,
        &mut None,
        &mut Vec::new(),
        &mut usage,
        &mut stop,
        &mut pause,
    )
    .unwrap();
    assert!(pause, "pause_turn must set the continuation flag");
}

#[test]
fn parse_frame_reads_event_and_data() {
    let frame = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n";
    let (event, data) = parse_frame(frame);
    assert_eq!(event, "content_block_delta");
    assert_eq!(data.unwrap()["delta"]["text"], "x");
}

// ── provider-side web search (#305, persistence #481) ───────────────────

#[test]
fn server_tool_use_sequence_yields_reasoning_and_content_block_not_tool_call() {
    // server_tool_use start → input_json_delta (the query) → stop must surface
    // the query as Reasoning + a persisted ContentBlock, never a ToolCall, and
    // stream no ToolCallDelta.
    let start = json!({
        "content_block": { "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {} }
    });
    let d1 = json!({ "delta": { "type": "input_json_delta", "partial_json": "{\"query\":" } });
    let d2 = json!({ "delta": { "type": "input_json_delta", "partial_json": "\"rust async\"}" } });

    let mut tool = None;
    let mut text = None;
    let mut blocks = Vec::new();
    let e0 = call(
        "content_block_start",
        Some(start),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    assert!(e0.is_empty());
    let e1 = call(
        "content_block_delta",
        Some(d1),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    let e2 = call(
        "content_block_delta",
        Some(d2),
        &mut tool,
        &mut text,
        &mut blocks,
    );
    // A server tool streams no client ToolCallDelta.
    assert!(e1.is_empty() && e2.is_empty(), "no deltas for server tool");
    let stop = call(
        "content_block_stop",
        None,
        &mut tool,
        &mut text,
        &mut blocks,
    );
    assert_eq!(
        stop,
        vec![
            LlmEvent::Reasoning("[web_search] rust async".into()),
            LlmEvent::ContentBlock(ContentPart::provider_search(
                "anthropic",
                "[web_search] rust async",
                json!({
                    "type": "server_tool_use",
                    "id": "srvtoolu_1",
                    "name": "web_search",
                    "input": { "query": "rust async" },
                }),
            )),
        ]
    );
    assert!(
        !stop.iter().any(|e| matches!(e, LlmEvent::ToolCall(_))),
        "server tool must never yield a ToolCall"
    );
    assert_eq!(
        blocks.len(),
        1,
        "the server_tool_use block is captured for continuation"
    );
}

#[test]
fn web_search_tool_result_block_renders_sources_and_content_block() {
    let block = json!({
        "content_block": {
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_1",
            "content": [
                { "type": "web_search_result", "title": "Rust async", "url": "https://docs.rs/async" },
                { "type": "web_search_result", "title": "Tokio", "url": "https://tokio.rs" }
            ]
        }
    });
    let mut blocks = Vec::new();
    let evs = call(
        "content_block_start",
        Some(block.clone()),
        &mut None,
        &mut None,
        &mut blocks,
    );
    assert_eq!(
        evs,
        vec![
            LlmEvent::Reasoning("[web_search] Rust async — https://docs.rs/async".into()),
            LlmEvent::Reasoning("[web_search] Tokio — https://tokio.rs".into()),
            LlmEvent::ContentBlock(ContentPart::provider_search(
                "anthropic",
                "[web_search] Rust async — https://docs.rs/async\n[web_search] Tokio — https://tokio.rs",
                block["content_block"].clone(),
            )),
        ]
    );
    assert_eq!(blocks, vec![block["content_block"].clone()]);
}

#[test]
fn web_search_tool_result_error_renders_error_line() {
    let block = json!({
        "content_block": {
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_1",
            "content": { "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" }
        }
    });
    let evs = call(
        "content_block_start",
        Some(block.clone()),
        &mut None,
        &mut None,
        &mut Vec::new(),
    );
    assert_eq!(
        evs,
        vec![
            LlmEvent::Reasoning("[web_search] error: max_uses_exceeded".into()),
            LlmEvent::ContentBlock(ContentPart::provider_search(
                "anthropic",
                "[web_search] error: max_uses_exceeded",
                block["content_block"].clone(),
            )),
        ]
    );
}
