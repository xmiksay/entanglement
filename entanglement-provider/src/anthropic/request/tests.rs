use super::*;

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
fn body_omits_tools_when_empty() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        None,
        None,
        None,
    );
    assert!(body.get("tools").is_none());
    assert_eq!(body["stream"], true);
    // No request params ⇒ the client's fallback cap, no temperature/thinking.
    assert_eq!(body["max_tokens"], 1024);
    assert!(body.get("temperature").is_none());
    assert!(body.get("thinking").is_none());
}

#[test]
fn body_includes_input_schema_when_tools_present() {
    let spec = ToolSpec::new("greet", "say hi");
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[spec],
        1024,
        None,
        None,
        None,
    );
    assert_eq!(body["tools"][0]["name"], "greet");
    assert!(body["tools"][0]["input_schema"].is_object());
}

#[test]
fn generation_max_output_tokens_overrides_fallback() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: Some(0.3),
            max_output_tokens: Some(8000),
            thinking_budget_tokens: None,
            reasoning_effort: None,
        }),
        None,
        None,
    );
    assert_eq!(body["max_tokens"], 8000);
    assert!((body["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
    assert!(body.get("thinking").is_none());
}

#[test]
fn thinking_budget_enables_thinking_and_drops_temperature() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: Some(0.7),
            max_output_tokens: Some(20_000),
            thinking_budget_tokens: Some(10_000),
            reasoning_effort: None,
        }),
        None,
        None,
    );
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 10_000);
    assert_eq!(body["max_tokens"], 20_000);
    // With thinking on, temperature must be its default — omitted, not sent.
    assert!(body.get("temperature").is_none());
}

#[test]
fn thinking_budget_bumps_max_tokens_when_it_would_swallow_the_cap() {
    // Anthropic requires budget_tokens < max_tokens; a budget at/over the cap
    // must lift the cap rather than send an invalid request.
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: None,
            max_output_tokens: Some(4000),
            thinking_budget_tokens: Some(4000),
            reasoning_effort: None,
        }),
        None,
        None,
    );
    let max = body["max_tokens"].as_u64().unwrap();
    let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
    assert!(max > budget, "max_tokens {max} must exceed budget {budget}");
}

#[test]
fn high_reasoning_effort_enables_thinking_at_the_tier_default_budget() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: Some(0.7),
            max_output_tokens: None,
            thinking_budget_tokens: None,
            reasoning_effort: Some(ReasoningEffort::High),
        }),
        None,
        None,
    );
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(
        body["thinking"]["budget_tokens"],
        HIGH_EFFORT_THINKING_BUDGET
    );
    // Thinking on ⇒ temperature omitted, same as an explicit budget.
    assert!(body.get("temperature").is_none());
}

#[test]
fn medium_reasoning_effort_uses_a_smaller_tier_budget() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: None,
            max_output_tokens: None,
            thinking_budget_tokens: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
        }),
        None,
        None,
    );
    assert_eq!(
        body["thinking"]["budget_tokens"],
        MEDIUM_EFFORT_THINKING_BUDGET
    );
}

#[test]
fn low_reasoning_effort_leaves_thinking_off() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: Some(0.4),
            max_output_tokens: None,
            thinking_budget_tokens: None,
            reasoning_effort: Some(ReasoningEffort::Low),
        }),
        None,
        None,
    );
    assert!(body.get("thinking").is_none());
    assert!((body["temperature"].as_f64().unwrap() - 0.4).abs() < 1e-6);
}

#[test]
fn explicit_thinking_budget_wins_over_reasoning_effort() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: None,
            max_output_tokens: Some(50_000),
            thinking_budget_tokens: Some(1234),
            reasoning_effort: Some(ReasoningEffort::High),
        }),
        None,
        None,
    );
    assert_eq!(body["thinking"]["budget_tokens"], 1234);
}

#[test]
fn consecutive_tool_results_merge_into_one_user_turn() {
    let msgs = vec![
        Message::assistant("", vec![]),
        Message::tool("a", "r1"),
        Message::tool("b", "r2"),
    ];
    let out = convert_messages(&msgs);
    // assistant (empty text, no calls) is dropped; both results land in one user msg.
    assert_eq!(out.len(), 1);
    let blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["tool_use_id"], "a");
    assert_eq!(blocks[1]["tool_use_id"], "b");
}

#[test]
fn adjacent_user_turns_coalesce_into_one() {
    // The ambiguous-stop retry shape (ADR-0118): an empty assistant round is
    // dropped, leaving the original prompt adjacent to the injected nudge.
    // Anthropic rejects non-alternating roles, so they must merge.
    let msgs = vec![
        msg(MessageRole::User, "do it"),
        msg(MessageRole::Assistant, ""), // empty ambiguous round → dropped
        msg(MessageRole::User, "[system] nudge"),
    ];
    let out = convert_messages(&msgs);
    assert_eq!(out.len(), 1, "the two user turns must merge; got {out:?}");
    assert_eq!(out[0]["role"], "user");
    let blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["text"], "do it");
    assert_eq!(blocks[1]["text"], "[system] nudge");
}

#[test]
fn alternating_roles_are_left_untouched() {
    // A well-formed history (the non-empty ambiguous case) must not merge.
    let msgs = vec![
        msg(MessageRole::User, "do it"),
        msg(MessageRole::Assistant, "partial"),
        msg(MessageRole::User, "[system] nudge"),
    ];
    let out = convert_messages(&msgs);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0]["role"], "user");
    assert_eq!(out[1]["role"], "assistant");
    assert_eq!(out[2]["role"], "user");
}

#[test]
fn user_image_renders_image_block() {
    let user = Message::user_content(vec![
        ContentPart::text("look"),
        ContentPart::image("image/png", "AAAA"),
    ]);
    let out = convert_messages(&[user]);
    let blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(blocks[0], json!({ "type": "text", "text": "look" }));
    assert_eq!(
        blocks[1],
        json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" },
        })
    );
}

#[test]
fn tool_result_with_image_renders_block_array() {
    // #221: `read` on an image emits an image tool result; text-only results
    // stay plain strings (asserted by `consecutive_tool_results_…`).
    let tool = Message::tool_content("a", vec![ContentPart::image("image/png", "AAAA")]);
    let out = convert_messages(&[tool]);
    let result = &out[0]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert_eq!(result["tool_use_id"], "a");
    assert_eq!(
        result["content"][0],
        json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" },
        })
    );
}

// ── provider-side web search (#305, version flag #481) ─────────────────

#[test]
fn body_omits_web_search_server_tool_without_config() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        None,
        None,
        None,
    );
    assert!(body.get("tools").is_none());
}

#[test]
fn body_pushes_web_search_server_tool_when_configured() {
    let ws = WebSearchConfig {
        enabled: true,
        max_uses: Some(4),
        allowed_domains: vec!["docs.rs".into()],
    };
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        None,
        Some(&ws),
        None,
    );
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "web_search_20250305");
    assert_eq!(tools[0]["name"], "web_search");
    assert_eq!(tools[0]["max_uses"], 4);
    assert_eq!(tools[0]["allowed_domains"][0], "docs.rs");
}

#[test]
fn web_search_server_tool_omits_unset_knobs() {
    let ws = WebSearchConfig {
        enabled: true,
        max_uses: None,
        allowed_domains: vec![],
    };
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        None,
        Some(&ws),
        None,
    );
    let tool = &body["tools"][0];
    assert_eq!(tool["type"], "web_search_20250305");
    assert!(tool.get("max_uses").is_none());
    assert!(tool.get("allowed_domains").is_none());
}

#[test]
fn web_search_tool_version_overrides_the_hardcoded_default() {
    // #481: a `ModelEntry::web_search_tool_version` capability flag selects
    // the newer server-tool type with no code change.
    let ws = WebSearchConfig {
        enabled: true,
        max_uses: None,
        allowed_domains: vec![],
    };
    let body = build_body(
        "claude-sonnet-4-6",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        None,
        Some(&ws),
        Some("web_search_20260209"),
    );
    assert_eq!(body["tools"][0]["type"], "web_search_20260209");
}

#[test]
fn provider_search_block_from_anthropic_replays_verbatim() {
    // A search block minted by *this* provider round-trips as its raw
    // stored `data` (#481) — the cache-benefit / continuity path.
    let raw = json!({ "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": { "query": "rust" } });
    let assistant = Message::assistant_content(
        vec![
            ContentPart::text("searching"),
            ContentPart::provider_search("anthropic", "[web_search] rust", raw.clone()),
        ],
        vec![],
    );
    let out = convert_messages(&[assistant]);
    let blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0], json!({ "type": "text", "text": "searching" }));
    assert_eq!(blocks[1], raw);
}

#[test]
fn provider_search_block_from_another_provider_is_dropped() {
    // A block minted by z.ai (crossed over via a live provider switch) has
    // no Anthropic-native wire shape — it must not leak `data` verbatim.
    let assistant = Message::assistant_content(
        vec![
            ContentPart::text("searching"),
            ContentPart::provider_search("zai", "[web_search] rust", json!(["anything"])),
        ],
        vec![],
    );
    let out = convert_messages(&[assistant]);
    let blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(
        blocks,
        &vec![json!({ "type": "text", "text": "searching" })]
    );
}
