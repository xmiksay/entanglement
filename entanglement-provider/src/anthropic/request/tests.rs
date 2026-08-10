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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
    );
    assert_eq!(body["thinking"]["budget_tokens"], 1234);
}

#[test]
fn adaptive_style_emits_adaptive_thinking_and_effort_not_a_budget() {
    // The newer Anthropic models reject `budget_tokens` with a 400; the adaptive
    // shape carries depth as `output_config.effort` instead.
    let body = build_body(
        "claude-opus-4-8",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: Some(0.7),
            max_output_tokens: Some(20_000),
            thinking_budget_tokens: None,
            reasoning_effort: Some(ReasoningEffort::High),
        }),
        None,
        None,
        ThinkingStyle::Adaptive,
        false,
    );
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert!(
        body["thinking"].get("budget_tokens").is_none(),
        "budget_tokens is rejected on the adaptive shape"
    );
    assert_eq!(body["output_config"]["effort"], "high");
    // Thinking is on, so temperature must still be omitted.
    assert!(body.get("temperature").is_none());
}

#[test]
fn adaptive_style_maps_every_effort_tier() {
    for (effort, expected) in [
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::Low, "low"),
    ] {
        let body = build_body(
            "claude-opus-4-8",
            "sys",
            &[msg(MessageRole::User, "hi")],
            &[],
            1024,
            Some(GenerationParams {
                temperature: None,
                max_output_tokens: None,
                thinking_budget_tokens: None,
                reasoning_effort: Some(effort),
            }),
            None,
            None,
            ThinkingStyle::Adaptive,
            false,
        );
        assert_eq!(body["output_config"]["effort"], expected);
    }
    // Unlike the budget shape, `Low` is a real tier here rather than "off" —
    // there is no budget to be too small, so the model still thinks adaptively.
}

#[test]
fn adaptive_style_without_effort_leaves_thinking_off() {
    // No effort ⇒ no thinking, and temperature passes through as it does with
    // thinking off on the budget shape.
    let body = build_body(
        "claude-opus-4-8",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            // Exactly representable in f32, so the widening to JSON's f64 is
            // lossless and the assertion below can compare equal.
            temperature: Some(0.5),
            max_output_tokens: None,
            thinking_budget_tokens: None,
            reasoning_effort: None,
        }),
        None,
        None,
        ThinkingStyle::Adaptive,
        false,
    );
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
    assert_eq!(body["temperature"], 0.5);
}

#[test]
fn adaptive_style_ignores_a_thinking_budget_and_never_bumps_max_tokens() {
    // A budget carried over from a profile persisted against a budget-shape model
    // must not leak onto the wire (it would 400), and must not trigger the
    // budget shape's `max_tokens` headroom bump either.
    let body = build_body(
        "claude-opus-4-8",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        Some(GenerationParams {
            temperature: None,
            max_output_tokens: Some(4000),
            thinking_budget_tokens: Some(64_000),
            reasoning_effort: None,
        }),
        None,
        None,
        ThinkingStyle::Adaptive,
        false,
    );
    assert!(body.get("thinking").is_none());
    assert_eq!(body["max_tokens"], 4000);
}

/// An assistant turn carrying one captured thinking block plus some text.
fn assistant_with_reasoning(text: &str, provider: &str) -> Message {
    let block = json!({ "type": "thinking", "thinking": "why", "signature": "sig" });
    Message::assistant_content(
        vec![
            ContentPart::text(text),
            ContentPart::reasoning(provider, "why", block),
        ],
        Vec::new(),
    )
}

#[test]
fn reasoning_block_replays_first_on_the_last_assistant_turn() {
    // Anthropic requires the thinking block to lead the assistant message, but
    // the turn loop appends content blocks after the round's text — the
    // converter restores the order.
    let msgs = vec![
        msg(MessageRole::User, "hi"),
        assistant_with_reasoning("answer", "anthropic"),
    ];
    let out = convert_messages(&msgs, true);
    let blocks = out[1]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["signature"], "sig");
    assert_eq!(blocks[1]["type"], "text");
}

#[test]
fn reasoning_block_is_dropped_when_replay_is_off() {
    // The catalog flag gates the wire, not history: the block is still in the
    // message, it just isn't sent.
    let msgs = vec![
        msg(MessageRole::User, "hi"),
        assistant_with_reasoning("answer", "anthropic"),
    ];
    let out = convert_messages(&msgs, false);
    let blocks = out[1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "text");
}

#[test]
fn reasoning_block_from_another_provider_is_dropped() {
    // An opaque signature is meaningless to anyone but its author, and reasoning
    // is not answer content — so it vanishes rather than degrading to text.
    let msgs = vec![
        msg(MessageRole::User, "hi"),
        assistant_with_reasoning("answer", "gemini"),
    ];
    let out = convert_messages(&msgs, true);
    let blocks = out[1]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "text");
}

#[test]
fn only_the_last_assistant_turn_replays_its_reasoning() {
    // Anthropic strips earlier turns' thinking server-side, so resending it
    // would just burn input tokens on blocks discarded on arrival.
    let msgs = vec![
        msg(MessageRole::User, "hi"),
        assistant_with_reasoning("first", "anthropic"),
        msg(MessageRole::User, "again"),
        assistant_with_reasoning("second", "anthropic"),
    ];
    let out = convert_messages(&msgs, true);
    let earlier = out[1]["content"].as_array().unwrap();
    assert!(
        earlier.iter().all(|b| b["type"] != "thinking"),
        "earlier assistant turn must not carry a thinking block"
    );
    let last = out[3]["content"].as_array().unwrap();
    assert_eq!(last[0]["type"], "thinking");
}

#[test]
fn consecutive_tool_results_merge_into_one_user_turn() {
    let msgs = vec![
        Message::assistant("", vec![]),
        Message::tool("a", "r1"),
        Message::tool("b", "r2"),
    ];
    let out = convert_messages(&msgs, false);
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
    let out = convert_messages(&msgs, false);
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
    let out = convert_messages(&msgs, false);
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
    let out = convert_messages(&[user], false);
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
    let out = convert_messages(&[tool], false);
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
        ThinkingStyle::Budget,
        false,
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
    let out = convert_messages(&[assistant], false);
    let blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0], json!({ "type": "text", "text": "searching" }));
    assert_eq!(blocks[1], raw);
}

// ── prompt caching breakpoints (#566) ───────────────────────────────────

#[test]
fn system_block_carries_a_cache_breakpoint() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        None,
        None,
        None,
        ThinkingStyle::Budget,
        false,
    );
    let system = body["system"].as_array().unwrap();
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["text"], "sys");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn last_tool_entry_carries_a_cache_breakpoint() {
    let specs = vec![ToolSpec::new("a", "a"), ToolSpec::new("b", "b")];
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &specs,
        1024,
        None,
        None,
        None,
        ThinkingStyle::Budget,
        false,
    );
    let tools = body["tools"].as_array().unwrap();
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn single_user_turn_carries_the_history_breakpoint() {
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &[msg(MessageRole::User, "hi")],
        &[],
        1024,
        None,
        None,
        None,
        ThinkingStyle::Budget,
        false,
    );
    let blocks = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral");
}

#[test]
fn second_to_last_user_turn_carries_the_history_breakpoint() {
    // The most recent user turn may still be edited/retried — anchor one turn
    // earlier so the stable bulk of history isn't re-marked every request.
    let msgs = vec![
        msg(MessageRole::User, "first"),
        msg(MessageRole::Assistant, "reply"),
        msg(MessageRole::User, "second"),
    ];
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &msgs,
        &[],
        1024,
        None,
        None,
        None,
        ThinkingStyle::Budget,
        false,
    );
    let out = body["messages"].as_array().unwrap();
    assert_eq!(out.len(), 3);
    let first_blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(
        first_blocks.last().unwrap()["cache_control"]["type"],
        "ephemeral"
    );
    // The latest turn is left unmarked.
    let last_blocks = out[2]["content"].as_array().unwrap();
    assert!(last_blocks.last().unwrap().get("cache_control").is_none());
}

#[test]
fn long_history_carries_two_anchors_and_at_most_four_markers_total() {
    // #673: near anchor on the 2nd-to-last user turn, deep anchor on the
    // 4th-to-last — so a round that appends more user-role messages than the
    // provider's ~20-block lookback can scan still finds a cached prefix.
    let mut msgs = Vec::new();
    for i in 0..6 {
        msgs.push(msg(MessageRole::User, &format!("u{i}")));
        msgs.push(msg(MessageRole::Assistant, &format!("a{i}")));
    }
    let specs = vec![ToolSpec::new("a", "a")];
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &msgs,
        &specs,
        1024,
        None,
        None,
        None,
        ThinkingStyle::Budget,
        false,
    );
    let out = body["messages"].as_array().unwrap();
    let marked: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m["content"]
                .as_array()
                .and_then(|b| b.last())
                .and_then(|b| b.get("cache_control"))
                .is_some()
        })
        .map(|(i, _)| i)
        .collect();
    // 12 messages, user turns at even indexes: 4th-to-last user = index 4,
    // 2nd-to-last user = index 8; the last user turn (index 10) stays clean.
    assert_eq!(marked, vec![4, 8]);
    // The API caps `cache_control` markers at 4 per request; system (1) +
    // tools (1) + history (2) sits exactly at it — a future 5th marker would
    // 400 every request, so lock the total in.
    let total = count_cache_controls(&body);
    assert_eq!(total, 4);
}

#[test]
fn short_history_dedupes_the_two_anchors() {
    // With only 1-3 user turns the near and deep anchors collapse to one
    // marked message — the same block is never marked twice.
    let msgs = vec![
        msg(MessageRole::User, "first"),
        msg(MessageRole::Assistant, "reply"),
        msg(MessageRole::User, "second"),
    ];
    let body = build_body(
        "claude-sonnet-4-5",
        "sys",
        &msgs,
        &[],
        1024,
        None,
        None,
        None,
        ThinkingStyle::Budget,
        false,
    );
    let out = body["messages"].as_array().unwrap();
    let marked = out
        .iter()
        .filter(|m| {
            m["content"]
                .as_array()
                .and_then(|b| b.last())
                .and_then(|b| b.get("cache_control"))
                .is_some()
        })
        .count();
    assert_eq!(marked, 1);
}

/// Count every `cache_control` occurrence anywhere in the request body.
fn count_cache_controls(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Object(map) => {
            let own = usize::from(map.contains_key("cache_control"));
            own + map.values().map(count_cache_controls).sum::<usize>()
        }
        serde_json::Value::Array(items) => items.iter().map(count_cache_controls).sum(),
        _ => 0,
    }
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
    let out = convert_messages(&[assistant], false);
    let blocks = out[0]["content"].as_array().unwrap();
    assert_eq!(
        blocks,
        &vec![json!({ "type": "text", "text": "searching" })]
    );
}
