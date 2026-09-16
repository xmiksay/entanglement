//! Replay fidelity as byte equality: for each scripted session shape, the
//! history a resumed session sends on its next request must equal the live
//! session's, message for message (ADR-0202 — any drift is a prompt-cache miss
//! and, on thinking models, a history the provider never signed).

mod harness;

use entanglement_core::{AgentState, ContentPart, SessionId, StopReason, ToolSpec, INVOKE_TOOL};
use harness::{
    assert_resume_from, assert_resume_is_byte_identical, call, replayed_messages, shape,
    text_round, tool_round, Run, Step,
};

fn no_specs() -> Vec<ToolSpec> {
    Vec::new()
}

#[tokio::test]
async fn text_only_turn() {
    let mut live = Run::start(vec![text_round("hello there")], no_specs());
    live.prompt("hi").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(
        shape(&history),
        ["user:hi", "assistant:hello there[]", "user:probe"]
    );
}

#[tokio::test]
async fn tool_round_then_text_round() {
    let mut script = tool_round(vec![call("c1", "read", r#"{"path":"a"}"#)]);
    script.insert(0, Step::Text("let me look"));
    let mut live = Run::start(vec![script, text_round("it says hi")], no_specs());
    live.prompt("read a").await;
    live.until_exec("c1").await;
    live.result("c1", "hi").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(
        shape(&history),
        [
            "user:read a",
            "assistant:let me look[\"c1\"]",
            "tool:c1",
            "assistant:it says hi[]",
            "user:probe"
        ]
    );
}

#[tokio::test]
async fn parallel_calls_resolved_in_reverse_order() {
    let batch = tool_round(vec![call("c1", "read", "{}"), call("c2", "grep", "{}")]);
    let mut live = Run::start(vec![batch, text_round("both read")], no_specs());
    live.prompt("go").await;
    live.until_exec("c2").await;
    live.result("c2", "second").await;
    live.result("c1", "first").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(
        shape(&history),
        [
            "user:go",
            "assistant:[\"c1\", \"c2\"]",
            "tool:c2",
            "tool:c1",
            "assistant:both read[]",
            "user:probe"
        ]
    );
}

/// Three rounds whose results are a text + tool reference, an image, and
/// empty — none of which a plain `output` string can rebuild.
#[tokio::test]
async fn three_consecutive_tool_rounds() {
    let mut first = tool_round(vec![call("a", "describe", "{}")]);
    first.insert(0, Step::Text("step one"));
    let scripts = vec![
        first,
        tool_round(vec![call("b", "read", r#"{"path":"x.png"}"#)]),
        tool_round(vec![call("c", "bash", "{}")]),
        text_round("all done"),
    ];
    let mut live = Run::start(scripts, no_specs());
    live.prompt("go").await;
    live.until_exec("a").await;
    let reference = ContentPart::ToolReference {
        tool_name: "grep".into(),
    };
    live.result_content("a", vec![ContentPart::text("schema"), reference])
        .await;
    live.until_exec("b").await;
    live.result_content("b", vec![ContentPart::image("image/png", "AAAA")])
        .await;
    live.until_exec("c").await;
    live.result("c", "").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(history.len(), 9, "{:#?}", shape(&history));
}

#[tokio::test]
async fn reasoning_block_and_tool_round() {
    let mut signed = call("c1", "read", "{}");
    signed.provider_meta = Some(serde_json::json!({ "thought_signature": "SIG" }));
    let block = ContentPart::Reasoning {
        provider: "anthropic".into(),
        text: "think".into(),
        data: serde_json::json!({ "signature": "abc" }),
    };
    let search = ContentPart::provider_search(
        "anthropic",
        "[web_search] q",
        serde_json::json!({ "type": "server_tool_use" }),
    );
    let mut script = tool_round(vec![signed]);
    script.splice(
        0..0,
        [
            Step::Reasoning("think"),
            Step::Block(block),
            Step::Block(search),
            Step::Text("reading"),
        ],
    );
    let mut live = Run::start(vec![script, text_round("done")], no_specs());
    live.prompt("go").await;
    live.until_exec("c1").await;
    live.result("c1", "out").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(history[1].content.len(), 3, "text, reasoning, search");
    assert!(history[1].tool_calls[0].provider_meta.is_some());
}

#[tokio::test]
async fn ambiguous_stop_retry_nudges() {
    let scripts = vec![
        vec![Step::Finish(None)],
        vec![Step::Text("half"), Step::Finish(Some(StopReason::Other))],
        text_round("full"),
    ];
    let mut live = Run::start(scripts, no_specs());
    live.prompt("go").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    let shape = shape(&history);
    assert_eq!(shape.len(), 6, "{shape:#?}");
    assert_eq!(
        shape[2], "assistant:half[]",
        "the empty round commits nothing"
    );
}

#[tokio::test]
async fn invoke_envelope_then_text_round() {
    let specs = vec![
        ToolSpec::new("read", "read a file"),
        ToolSpec::new(INVOKE_TOOL, "call a discovered tool"),
    ];
    let raw = r#"{"name":"read","args":{"path":"x"}}"#;
    let scripts = vec![
        tool_round(vec![call("c1", INVOKE_TOOL, raw)]),
        text_round("done"),
    ];
    let mut live = Run::start(scripts, specs);
    live.prompt("go").await;
    live.until_exec("c1").await;
    live.result("c1", "contents").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(history[1].tool_calls[0].name, INVOKE_TOOL);
    assert_eq!(history[1].tool_calls[0].input, raw);
}

#[tokio::test]
async fn mid_stream_failure_commits_the_interrupted_partial() {
    let scripts = vec![vec![Step::Text("partial"), Step::Fail("connection reset")]];
    let mut live = Run::start(scripts, no_specs());
    live.prompt("go").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(history[1].text(), "partial\n\n[interrupted]");
}

#[tokio::test]
async fn stop_mid_stream_discards_the_partial() {
    let scripts = vec![vec![Step::Text("partial"), Step::Hang]];
    let mut live = Run::start(scripts, no_specs());
    live.prompt("go").await;
    live.until(|ev| matches!(ev, entanglement_core::OutEvent::TextDelta { .. }))
        .await;
    live.stop().await;
    live.until(|ev| {
        matches!(
            ev,
            entanglement_core::OutEvent::Status {
                state: entanglement_core::AgentState::Done,
                ..
            }
        )
    })
    .await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(shape(&history), ["user:go", "user:probe"]);
}

#[tokio::test]
async fn two_full_user_turns() {
    let scripts = vec![
        tool_round(vec![call("c1", "read", "{}")]),
        text_round("first answer"),
        text_round("second answer"),
    ];
    let mut live = Run::start(scripts, no_specs());
    live.prompt("one").await;
    live.until_exec("c1").await;
    live.result("c1", "out").await;
    live.until_done().await;
    live.prompt("two").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(history.len(), 7, "{:#?}", shape(&history));
}

/// ADR-0058: a prompt sent while the batch is parked folds in after the
/// results, at the next round.
#[tokio::test]
async fn prompt_sent_while_parked_folds_at_the_next_round() {
    let scripts = vec![
        tool_round(vec![call("c1", "read", "{}")]),
        text_round("done"),
    ];
    let mut live = Run::start(scripts, no_specs());
    live.prompt("go").await;
    live.until_exec("c1").await;
    live.prompt("also y").await;
    live.result("c1", "out").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(
        shape(&history),
        [
            "user:go",
            "assistant:[\"c1\"]",
            "tool:c1",
            "user:also y",
            "assistant:done[]",
            "user:probe"
        ]
    );
}

/// A confident stop with no content commits nothing (no empty assistant turn).
#[tokio::test]
async fn confident_empty_reply_after_a_tool_round() {
    let scripts = vec![
        tool_round(vec![call("c1", "read", "{}")]),
        vec![Step::Finish(Some(StopReason::EndTurn))],
    ];
    let mut live = Run::start(scripts, no_specs());
    live.prompt("go").await;
    live.until_exec("c1").await;
    live.result("c1", "out").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(
        shape(&history),
        ["user:go", "assistant:[\"c1\"]", "tool:c1", "user:probe"]
    );
}

/// With retries disabled an empty ambiguous reply ends the turn uncommitted —
/// indistinguishable in the log from an empty confident one, hence one rule.
#[tokio::test]
async fn empty_ambiguous_reply_with_retries_disabled() {
    let scripts = vec![
        tool_round(vec![call("c1", "read", "{}")]),
        vec![Step::Finish(None)],
    ];
    let mut live = Run::with_config(scripts, no_specs(), |cfg| {
        cfg.max_ambiguous_stop_retries = 0
    });
    live.prompt("go").await;
    live.until_exec("c1").await;
    live.result("c1", "out").await;
    live.until_done().await;
    let history = assert_resume_is_byte_identical(live).await;
    assert_eq!(
        shape(&history),
        ["user:go", "assistant:[\"c1\"]", "tool:c1", "user:probe"]
    );
}

/// A `Stop` before the round streamed anything ends the turn, so the next
/// prompt starts a turn and is pushed at once — even when the log then stops
/// mid-stream (a crash snapshot), before that round's first event.
#[tokio::test]
async fn stop_before_any_stream_event_ends_the_turn() {
    let mut live = Run::start(vec![vec![Step::Hang], vec![Step::Hang]], no_specs());
    live.prompt("go").await;
    live.until_status(AgentState::Thinking).await;
    live.stop().await;
    live.until_status(AgentState::Done).await;
    live.prompt("two").await;
    live.until_status(AgentState::Thinking).await;
    let log = live.take_log();
    live.stop().await;
    live.until_status(AgentState::Done).await;
    let history = assert_resume_from(live, log).await;
    assert_eq!(shape(&history), ["user:go", "user:two", "user:probe"]);
}

/// A `max_turns` trip ends the turn with `Done`, so a later crash snapshot
/// neither stashes the next prompt nor resumes the tripped turn.
#[tokio::test]
async fn max_turns_trip_ends_the_turn() {
    let scripts = vec![tool_round(vec![call("c1", "read", "{}")]), vec![Step::Hang]];
    let mut live = Run::with_config(scripts, no_specs(), |cfg| cfg.max_turns = 1);
    live.prompt("go").await;
    live.until_exec("c1").await;
    live.result("c1", "out").await;
    live.until_done().await;
    live.prompt("two").await;
    live.until_status(AgentState::Thinking).await;
    let log = live.take_log();
    live.stop().await;
    live.until_status(AgentState::Done).await;
    let history = assert_resume_from(live, log).await;
    assert_eq!(
        shape(&history),
        [
            "user:go",
            "assistant:[\"c1\"]",
            "tool:c1",
            "user:two",
            "user:probe"
        ]
    );
}

// --- Compaction successors (ADR-0205) -------------------------------------
//
// Every compaction forks: the source is retired unchanged and a successor
// carries the turn on. Both halves must hold up under replay — the successor
// reads back byte-identically from its own log, and the retired predecessor
// still replays to exactly what it held at the fork point.

/// 4k window ⇒ a ~3400-token input budget, small enough to overflow on demand.
fn small_window(cfg: &mut entanglement_core::EngineConfig) {
    cfg.context_window = Some(4_000);
}

/// Same, with auto-summarize off so an overflow goes straight to the
/// prune-only fallback.
fn small_window_no_summary(cfg: &mut entanglement_core::EngineConfig) {
    cfg.context_window = Some(4_000);
    cfg.auto_compact = false;
}

#[tokio::test]
async fn a_summary_fork_successor_replays_byte_identically() {
    let scripts = vec![
        text_round("first reply"),
        text_round("second reply"),
        // the compaction summary request
        text_round("SUMMARY of the earlier work"),
        text_round("continuing"),
    ];
    let mut live = Run::with_config(scripts, no_specs(), small_window);
    // Two full turns, so the history has a real head to summarize: the
    // keep-tail clamp (`safe_kept`) walks forward to a `User` boundary, and on
    // a one-turn history that swallows everything and leaves nothing to
    // summarize.
    live.prompt(&"a".repeat(3_000)).await;
    live.until_done().await;
    live.prompt(&"b".repeat(3_000)).await;
    live.until_done().await;
    // Only this third prompt tips the history over the budget, so this is the
    // round that summarizes and forks.
    live.prompt(&"c".repeat(6_200)).await;
    let predecessor_log = live.until_forked().await;

    // The retired predecessor still replays to what it held at the fork.
    let before = replayed_messages(small_window, &SessionId::new("eq"), &predecessor_log);
    assert_eq!(
        shape(&before)[1],
        "assistant:first reply[]",
        "the predecessor keeps its pre-compaction history: {:#?}",
        shape(&before)
    );

    // And the successor's live history equals its resumed history byte for byte.
    // The successor's own log replays to the history it started live with:
    // the summary plus the ADR-0102 verbatim kept tail, seeded through its
    // `Spawn` prompt and recorded as a prompt the way persistence does
    // (ADR-0113). Folded directly rather than probed — probing would run a
    // fresh turn, which is a different question from what the log holds.
    let after = replayed_messages(small_window, &live.sid, &live.log);
    assert!(
        after[0].text().contains("SUMMARY of the earlier work"),
        "the successor starts from the summary: {:#?}",
        shape(&after)
    );
    assert!(
        after[0].text().contains("preserved verbatim"),
        "the kept tail rides into the successor: {:#?}",
        shape(&after)
    );
}

#[tokio::test]
async fn a_prune_fork_successor_replays_byte_identically() {
    // With auto-summarize off, an overflow takes the prune fallback — which
    // forks and announces itself too (ADR-0205 retires ADR-0121's silence).
    let scripts = vec![
        tool_round(vec![call("c1", "read", "{}")]),
        text_round("continuing"),
    ];
    let mut live = Run::with_config(scripts, no_specs(), small_window_no_summary);
    live.prompt("go").await;
    live.until_exec("c1").await;
    // A tool output far over the budget, and prunable — exactly what the
    // placeholder prune reclaims.
    live.result("c1", &"x".repeat(14_000)).await;
    let predecessor_log = live.until_forked().await;

    let before = replayed_messages(
        small_window_no_summary,
        &SessionId::new("eq"),
        &predecessor_log,
    );
    assert_eq!(
        shape(&before)[..3],
        [
            "user:go".to_string(),
            "assistant:[\"c1\"]".to_string(),
            "tool:c1".to_string()
        ],
        "the predecessor keeps its un-pruned history: {:#?}",
        shape(&before)
    );

    assert_eq!(
        before[2].text().len(),
        14_000,
        "the source's bulky tool output was never pruned in place"
    );

    // The successor's own log replays to the pruned transcript it started
    // from — the prune ran on a copy and seeded this session with the result.
    let after = replayed_messages(small_window_no_summary, &live.sid, &live.log);
    assert!(
        after[0].text().contains("pruned to fit the context window"),
        "the successor starts from the pruned transcript: {:#?}",
        shape(&after)
    );
}
