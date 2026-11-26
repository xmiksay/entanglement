use super::*;

fn call(event: &str, data: Option<Value>, current_tool: &mut Option<PendingTool>) -> Vec<LlmEvent> {
    handle_frame(
        event,
        data,
        current_tool,
        &mut None,
        &mut Usage::default(),
        &mut None,
    )
    .unwrap()
}

/// Like [`call`] but threads the extended-thinking accumulator, which spans a
/// `content_block_start` → `*_delta` → `content_block_stop` sequence.
fn call_thinking(
    event: &str,
    data: Option<Value>,
    current_thinking: &mut Option<PendingThinking>,
) -> Vec<LlmEvent> {
    handle_frame(
        event,
        data,
        &mut None,
        current_thinking,
        &mut Usage::default(),
        &mut None,
    )
    .unwrap()
}

#[test]
fn thinking_block_assembles_text_and_signature_for_replay() {
    let mut thinking = None;
    call_thinking(
        "content_block_start",
        Some(json!({ "content_block": { "type": "thinking", "thinking": "" } })),
        &mut thinking,
    );
    let d = call_thinking(
        "content_block_delta",
        Some(json!({ "delta": { "type": "thinking_delta", "thinking": "step " } })),
        &mut thinking,
    );
    // The live display channel is unchanged by capture.
    assert_eq!(d, vec![LlmEvent::Reasoning("step ".into())]);
    call_thinking(
        "content_block_delta",
        Some(json!({ "delta": { "type": "thinking_delta", "thinking": "one" } })),
        &mut thinking,
    );
    // The signature arrives on its own delta type and is load-bearing on replay.
    call_thinking(
        "content_block_delta",
        Some(json!({ "delta": { "type": "signature_delta", "signature": "sig123" } })),
        &mut thinking,
    );
    let evs = call_thinking("content_block_stop", None, &mut thinking);

    let expected = json!({ "type": "thinking", "thinking": "step one", "signature": "sig123" });
    assert_eq!(
        evs,
        vec![LlmEvent::ContentBlock(ContentPart::reasoning(
            "anthropic",
            "step one",
            expected,
        ))]
    );
}

#[test]
fn thinking_block_with_empty_text_still_replays() {
    // Current models omit thinking text by default but still sign the block;
    // dropping it would break the next tool round-trip.
    let mut thinking = None;
    call_thinking(
        "content_block_start",
        Some(json!({ "content_block": { "type": "thinking" } })),
        &mut thinking,
    );
    call_thinking(
        "content_block_delta",
        Some(json!({ "delta": { "type": "signature_delta", "signature": "sig" } })),
        &mut thinking,
    );
    let evs = call_thinking("content_block_stop", None, &mut thinking);
    assert!(matches!(
        evs.as_slice(),
        [LlmEvent::ContentBlock(ContentPart::Reasoning { text, .. })] if text.is_empty()
    ));
}

#[test]
fn unsigned_thinking_block_is_not_captured_for_replay() {
    // Anthropic rejects a thinking block whose signature is missing, so one that
    // never received a `signature_delta` is display-only.
    let mut thinking = None;
    call_thinking(
        "content_block_start",
        Some(json!({ "content_block": { "type": "thinking" } })),
        &mut thinking,
    );
    call_thinking(
        "content_block_delta",
        Some(json!({ "delta": { "type": "thinking_delta", "thinking": "hm" } })),
        &mut thinking,
    );
    let evs = call_thinking("content_block_stop", None, &mut thinking);
    assert!(evs.is_empty(), "no replayable block without a signature");
}

#[test]
fn redacted_thinking_block_is_captured_whole() {
    // Redacted blocks arrive complete, carry no readable text, and must still be
    // echoed back verbatim.
    let block = json!({ "type": "redacted_thinking", "data": "encrypted-payload" });
    let mut thinking = None;
    let evs = call_thinking(
        "content_block_start",
        Some(json!({ "content_block": block.clone() })),
        &mut thinking,
    );
    assert_eq!(
        evs,
        vec![LlmEvent::ContentBlock(ContentPart::reasoning(
            "anthropic",
            "",
            block,
        ))]
    );
}

#[test]
fn text_delta_yields_text() {
    let data = json!({ "delta": { "type": "text_delta", "text": "hel" } });
    let evs = call("content_block_delta", Some(data), &mut None);
    assert_eq!(evs, vec![LlmEvent::Text("hel".into())]);
}

#[test]
fn tool_block_assembles_across_deltas() {
    let start = json!({
        "content_block": { "type": "tool_use", "id": "t1", "name": "greet", "input": {} }
    });
    let d1 = json!({ "delta": { "type": "input_json_delta", "partial_json": "{\"nm\":" } });
    let d2 = json!({ "delta": { "type": "input_json_delta", "partial_json": "\"sam\"}" } });

    let mut tool = None;
    call("content_block_start", Some(start), &mut tool);
    call("content_block_delta", Some(d1), &mut tool);
    call("content_block_delta", Some(d2), &mut tool);
    let evs = call("content_block_stop", None, &mut tool);
    assert_eq!(
        evs,
        vec![LlmEvent::ToolCall(crate::ToolCall {
            id: "t1".into(),
            name: "greet".into(),
            input: r#"{"nm":"sam"}"#.into(),
            provider_meta: None,
        })]
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
    call("content_block_start", Some(start), &mut tool);
    let e1 = call("content_block_delta", Some(d1), &mut tool);
    assert_eq!(
        e1,
        vec![LlmEvent::ToolCallDelta {
            id: "t1".into(),
            name: "greet".into(),
            delta: "{\"nm\":".into(),
        }]
    );
    let e2 = call("content_block_delta", Some(d2), &mut tool);
    assert_eq!(
        e2,
        vec![LlmEvent::ToolCallDelta {
            id: "t1".into(),
            name: "greet".into(),
            delta: "\"sam\"}".into(),
        }]
    );
    let stop = call("content_block_stop", None, &mut tool);
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
    let _ = handle_frame(
        "message_start",
        Some(json!({ "message": { "usage": {
            "input_tokens": 42,
            "cache_read_input_tokens": 10,
            "cache_creation_input_tokens": 5
        } } })),
        &mut None,
        &mut None,
        &mut usage,
        &mut stop,
    )
    .unwrap();
    let _ = handle_frame(
        "message_delta",
        Some(json!({ "delta": { "stop_reason": "max_tokens" }, "usage": { "output_tokens": 7 } })),
        &mut None,
        &mut None,
        &mut usage,
        &mut stop,
    )
    .unwrap();
    assert_eq!(usage.input_tokens, Some(42));
    assert_eq!(usage.output_tokens, Some(7));
    assert_eq!(usage.cached_input_tokens, Some(10));
    assert_eq!(usage.cache_write_tokens, Some(5));
    assert_eq!(stop, Some(StopReason::MaxTokens));
}

#[test]
fn parse_frame_reads_event_and_data() {
    let frame = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n";
    let (event, data) = parse_frame(frame);
    assert_eq!(event, "content_block_delta");
    assert_eq!(data.unwrap()["delta"]["text"], "x");
}
