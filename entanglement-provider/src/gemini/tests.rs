//! Response/stream-side unit tests for the Gemini client (request-side tests live
//! in `request.rs`). Covers SSE frame parsing, chunk → event mapping, the
//! thought-signature stash, and usage folding.

use super::*;

#[test]
fn parse_frame_extracts_data_json() {
    let frame = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n";
    let v = parse_frame(frame).unwrap();
    assert_eq!(v["candidates"][0]["content"]["parts"][0]["text"], "hi");
}

#[test]
fn parse_frame_ignores_non_data_frames() {
    assert!(parse_frame(": keep-alive\n\n").is_none());
    assert!(parse_frame("\n\n").is_none());
}

#[test]
fn text_part_yields_text() {
    let data = json!({ "candidates": [{ "content": { "parts": [{ "text": "hello" }] } }] });
    let mut usage = Usage::default();
    let mut fr = None;
    let evs = handle_chunk(&data, &mut usage, &mut fr).unwrap();
    assert_eq!(evs, vec![LlmEvent::Text("hello".into())]);
}

#[test]
fn thought_part_yields_reasoning() {
    let data = json!({
        "candidates": [{ "content": { "parts": [{ "text": "pondering", "thought": true }] } }]
    });
    let mut usage = Usage::default();
    let mut fr = None;
    let evs = handle_chunk(&data, &mut usage, &mut fr).unwrap();
    assert_eq!(evs, vec![LlmEvent::Reasoning("pondering".into())]);
}

#[test]
fn function_call_assembles_tool_call_with_signature() {
    let data = json!({
        "candidates": [{ "content": { "parts": [{
            "functionCall": { "name": "search", "args": { "q": "rust" } },
            "thoughtSignature": "SIG-xyz"
        }] } }]
    });
    let mut usage = Usage::default();
    let mut fr = None;
    let evs = handle_chunk(&data, &mut usage, &mut fr).unwrap();
    let LlmEvent::ToolCall(tc) = &evs[0] else {
        panic!("expected ToolCall, got {evs:?}");
    };
    assert_eq!(tc.name, "search");
    assert_eq!(tc.id, "search"); // Gemini matches responses by name.
    assert_eq!(tc.input, r#"{"q":"rust"}"#);
    // The opaque signature is stashed for verbatim round-trip (#309).
    assert_eq!(
        tc.provider_meta.as_ref().unwrap()[THOUGHT_SIGNATURE_KEY],
        "SIG-xyz"
    );
}

#[test]
fn function_call_without_signature_has_no_meta() {
    let data = json!({
        "candidates": [{ "content": { "parts": [{
            "functionCall": { "name": "noop", "args": {} }
        }] } }]
    });
    let mut usage = Usage::default();
    let mut fr = None;
    let evs = handle_chunk(&data, &mut usage, &mut fr).unwrap();
    let LlmEvent::ToolCall(tc) = &evs[0] else {
        panic!("expected ToolCall");
    };
    assert!(tc.provider_meta.is_none());
    assert_eq!(tc.input, "{}");
}

#[test]
fn usage_metadata_splits_cached_from_input() {
    // promptTokenCount is the whole prompt incl. cache reads; input_tokens must be
    // the uncached remainder so catalog pricing doesn't double-count (#192).
    let data = json!({
        "candidates": [{ "content": { "parts": [] }, "finishReason": "STOP" }],
        "usageMetadata": {
            "promptTokenCount": 100,
            "cachedContentTokenCount": 30,
            "candidatesTokenCount": 12
        }
    });
    let mut usage = Usage::default();
    let mut fr = None;
    handle_chunk(&data, &mut usage, &mut fr).unwrap();
    assert_eq!(usage.input_tokens, Some(70));
    assert_eq!(usage.cached_input_tokens, Some(30));
    assert_eq!(usage.output_tokens, Some(12));
    assert_eq!(fr.as_deref(), Some("STOP"));
}

#[test]
fn stop_reason_mapping() {
    assert_eq!(StopReason::from_gemini("STOP"), StopReason::EndTurn);
    assert_eq!(StopReason::from_gemini("MAX_TOKENS"), StopReason::MaxTokens);
    assert_eq!(StopReason::from_gemini("SAFETY"), StopReason::Other);
}
