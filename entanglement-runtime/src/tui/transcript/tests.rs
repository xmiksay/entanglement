use super::*;
use crate::tui::app::App;
use crate::tui::theme::hash_profile_color;
use entanglement_core::{OutEvent, SessionId};

fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn feed_reasoning(app: &mut App, sid: &SessionId, body: &str) {
    for (i, chunk) in body.split_inclusive('\n').enumerate() {
        app.handle_out_event(OutEvent::ReasoningDelta {
            session: sid.clone(),
            seq: i as u64 + 1,
            text: chunk.to_string(),
        });
    }
}

/// Index of the first rendered line whose text contains `needle`.
fn line_index_of(body: &RenderedBody, needle: &str) -> usize {
    body.lines
        .iter()
        .position(|l| line_text(l).contains(needle))
        .unwrap_or_else(|| panic!("no rendered line contains {needle:?}"))
}

#[test]
fn text_then_reasoning_renders_in_arrival_order() {
    // A turn that streams assistant text and *then* a thinking block must
    // render the thinking header after the text, not before it (#88).
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 1,
        text: "ANSWERTEXT\n".to_string(),
    });
    app.handle_out_event(OutEvent::ReasoningDelta {
        session: sid.clone(),
        seq: 2,
        text: "afterthought\n".to_string(),
    });

    let body = render_body_lines(&mut app, 80);
    let text_at = line_index_of(&body, "ANSWERTEXT");
    let thinking_at = line_index_of(&body, "▸ Thinking");
    assert!(
        text_at < thinking_at,
        "text (line {text_at}) must render before the trailing thinking block (line {thinking_at})"
    );
}

#[test]
fn reasoning_then_text_renders_thinking_first() {
    // The common case — model thinks, then answers. The thinking block must
    // render before the assistant text, not after it (the #88 regression).
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    app.handle_out_event(OutEvent::ReasoningDelta {
        session: sid.clone(),
        seq: 1,
        text: "forethought\n".to_string(),
    });
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 2,
        text: "ANSWERTEXT\n".to_string(),
    });

    let body = render_body_lines(&mut app, 80);
    let thinking_at = line_index_of(&body, "▸ Thinking");
    let text_at = line_index_of(&body, "ANSWERTEXT");
    assert!(
        thinking_at < text_at,
        "leading thinking block (line {thinking_at}) must render before the text (line {text_at})"
    );
}

#[test]
fn interleaved_runs_stay_ordered_and_distinct() {
    // text → reasoning → text produces two text runs bracketing one thinking
    // block, each a separately-toggleable run keyed by its own block id.
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 1,
        text: "FIRSTTEXT\n".to_string(),
    });
    app.handle_out_event(OutEvent::ReasoningDelta {
        session: sid.clone(),
        seq: 2,
        text: "mid think\n".to_string(),
    });
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 3,
        text: "SECONDTEXT\n".to_string(),
    });

    let body = render_body_lines(&mut app, 80);
    let first = line_index_of(&body, "FIRSTTEXT");
    let thinking = line_index_of(&body, "▸ Thinking");
    let second = line_index_of(&body, "SECONDTEXT");
    assert!(
        first < thinking && thinking < second,
        "expected FIRSTTEXT ({first}) < Thinking ({thinking}) < SECONDTEXT ({second})"
    );
    // The reasoning run's block id is the transcript index of its first
    // `ReasoningDelta` (entry 1), tagged on its rendered header.
    assert_eq!(body.line_blocks[thinking], Some(1));
}

#[test]
fn reasoning_collapsed_by_default_shows_only_header() {
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    feed_reasoning(&mut app, &sid, "alpha\nbeta\nSECRETBODY\n");

    let body = render_body_lines(&mut app, 80);
    assert!(
        body.lines
            .iter()
            .any(|l| line_text(l).contains("▸ Thinking")),
        "collapsed run should show a ▸ header"
    );
    assert!(
        !body
            .lines
            .iter()
            .any(|l| line_text(l).contains("SECRETBODY")),
        "collapsed run must hide the reasoning body"
    );
    // The header line is tagged with the run's block id (transcript index 0).
    let header_idx = body
        .lines
        .iter()
        .position(|l| line_text(l).contains("▸ Thinking"))
        .unwrap();
    assert_eq!(body.line_blocks[header_idx], Some(0));
}

#[test]
fn reasoning_expands_on_toggle() {
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    feed_reasoning(&mut app, &sid, "alpha\nbeta\nSECRETBODY\n");

    app.toggle_block(0);
    let body = render_body_lines(&mut app, 80);
    assert!(
        body.lines
            .iter()
            .any(|l| line_text(l).contains("▾ Thinking")),
        "expanded run should show a ▾ header"
    );
    assert!(
        body.lines
            .iter()
            .any(|l| line_text(l).contains("SECRETBODY")),
        "expanded run must show the reasoning body"
    );

    // Toggle round-trip collapses it again.
    app.toggle_block(0);
    let body = render_body_lines(&mut app, 80);
    assert!(!body
        .lines
        .iter()
        .any(|l| line_text(l).contains("SECRETBODY")));
}

fn feed_tool_call(app: &mut App, sid: &SessionId, seq: u64, tool: &str, input: &str) {
    app.handle_out_event(OutEvent::ToolCall {
        session: sid.clone(),
        seq,
        request_id: format!("c{seq}"),
        tool: tool.to_string(),
        input: input.to_string(),
    });
}

#[test]
fn tool_op_header_shows_primary_arg_and_collapses_by_default() {
    // One collapsible line per op: the `read` header carries the filename and
    // the call args (the body) stay hidden until expanded (#340).
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    feed_tool_call(&mut app, &sid, 1, "read", r#"{"path":"src/main.rs"}"#);

    let body = render_body_lines(&mut app, 80);
    let header_idx = line_index_of(&body, "▸ read");
    let header = line_text(&body.lines[header_idx]);
    assert!(
        header.contains("src/main.rs"),
        "collapsed read header must show the filename: {header:?}"
    );
    // Collapsed: the header line is tagged with the op's block id (index 0).
    assert_eq!(body.line_blocks[header_idx], Some(0));
}

#[test]
fn tool_op_expands_to_show_body_and_check_when_done() {
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    feed_tool_call(&mut app, &sid, 1, "bash", r#"{"command":"echo hi"}"#);
    app.handle_out_event(OutEvent::ToolOutput {
        session: sid.clone(),
        seq: 2,
        request_id: "c1".to_string(),
        tool: "bash".to_string(),
        output: "SECRETBODY".to_string(),
        content: vec![],
    });

    // Folded but collapsed: a ✓ header, no body.
    let body = render_body_lines(&mut app, 80);
    let header_idx = line_index_of(&body, "▸ bash");
    assert!(line_text(&body.lines[header_idx]).contains("✓"));
    assert!(
        !body
            .lines
            .iter()
            .any(|l| line_text(l).contains("SECRETBODY")),
        "collapsed op must hide the output body"
    );

    // Expanding the block (its transcript index is 0) reveals the body.
    app.toggle_block(0);
    let body = render_body_lines(&mut app, 80);
    assert!(body.lines.iter().any(|l| line_text(l).contains("▾ bash")));
    assert!(
        body.lines
            .iter()
            .any(|l| line_text(l).contains("SECRETBODY")),
        "expanded op must show the output body"
    );
}

#[test]
fn edit_op_expands_to_a_diff_not_raw_json() {
    // The epic's payoff (#341 wired through #340): an expanded `edit` shows a
    // real `+`/`-` diff of oldString→newString, never the raw JSON args.
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    feed_tool_call(
        &mut app,
        &sid,
        1,
        "edit",
        r#"{"path":"a.rs","oldString":"foo","newString":"bar"}"#,
    );

    app.toggle_block(0);
    let body = render_body_lines(&mut app, 80);
    let has_delete = body
        .lines
        .iter()
        .any(|l| l.spans.iter().any(|s| s.content == "- "));
    let has_insert = body
        .lines
        .iter()
        .any(|l| l.spans.iter().any(|s| s.content == "+ "));
    assert!(
        has_delete && has_insert,
        "expanded edit must render a `-`/`+` diff pair"
    );
    assert!(
        !body
            .lines
            .iter()
            .any(|l| line_text(l).contains("oldString")),
        "expanded edit must not dump the raw JSON args"
    );
}

#[test]
fn streamed_table_renders_as_grid_after_all_deltas() {
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    // A table streamed token-by-token, exactly as the engine emits it.
    let deltas = [
        "| name | role |\n",
        "| --- | --- |\n",
        "| holly | engine |\n",
        "| tui | head |\n",
    ];
    for (i, d) in deltas.iter().enumerate() {
        app.handle_out_event(OutEvent::TextDelta {
            session: sid.clone(),
            seq: i as u64 + 1,
            text: (*d).to_string(),
        });
    }

    let lines = render_body_lines(&mut app, 80).lines;
    let joined: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect::<String>()
        .replace('\n', "\\n");
    println!("STREAMED TABLE LINES:");
    for l in &lines {
        let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
        println!("  {s:?}");
    }
    let has_grid = lines.iter().any(|l| {
        let s: String = l.spans.iter().map(|sp| sp.content.as_ref()).collect();
        s.contains("---")
    });
    assert!(
        has_grid,
        "streamed table did not render as a grid: {joined}"
    );
}

#[test]
fn narrow_widths_do_not_panic() {
    // The padding rows compute `" ".repeat(width - 1)`; at width 0 or 1 a
    // raw `u16` subtraction underflows (panic in debug, 65535 in release).
    // Feed one of every padded entry kind and render at the degenerate
    // widths — this must not panic and must produce lines.
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    app.record_user_message("hello".to_string());
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 1,
        text: "assistant reply\n".to_string(),
    });
    app.handle_out_event(OutEvent::ToolCall {
        session: sid.clone(),
        seq: 2,
        request_id: "c1".to_string(),
        tool: "read".to_string(),
        input: "{\"path\":\"x\"}".to_string(),
    });
    app.handle_out_event(OutEvent::ToolOutput {
        session: sid.clone(),
        seq: 3,
        request_id: "c1".to_string(),
        tool: "read".to_string(),
        output: "file body".to_string(),
        content: vec![],
    });
    app.handle_out_event(OutEvent::Error {
        session: sid.clone(),
        seq: 4,
        message: "boom".to_string(),
    });
    feed_reasoning(&mut app, &sid, "thinking hard\n");

    for width in [0u16, 1, 2] {
        let body = render_body_lines(&mut app, width);
        assert!(
            !body.lines.is_empty(),
            "width {width} should still render lines"
        );
    }
}

#[test]
fn user_messages_use_profile_colors() {
    let sid = SessionId::new("test");
    let mut app = App::new_for_test(sid.clone());
    app.record_user_message("Hello world".to_string());

    let lines = render_body_lines(&mut app, 80).lines;
    let user_color = hash_profile_color("build");
    let theme = app.theme();
    let expected_user_bg = theme.user_colors(user_color).bg;

    let user_lines: Vec<_> = lines
        .iter()
        .filter(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("Hello") || s.content.contains("world"))
        })
        .collect();

    assert!(!user_lines.is_empty(), "Should have user message lines");
    for line in user_lines {
        if let Some(bg) = line.style.bg {
            assert_eq!(
                bg, expected_user_bg,
                "User message should have message background"
            );
        }
    }
}

#[test]
fn assistant_lines_use_theme_colors() {
    let sid = SessionId::new("test");
    let mut app = App::new_for_test(sid.clone());
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 1,
        text: "Response".to_string(),
    });

    let lines = render_body_lines(&mut app, 80).lines;
    let theme = app.theme();
    let expected_bg = theme.assistant_colors().bg;

    let assistant_lines: Vec<_> = lines
        .iter()
        .filter(|l| l.spans.iter().any(|s| s.content.contains("Response")))
        .collect();

    assert!(!assistant_lines.is_empty(), "Should have assistant lines");
    for line in assistant_lines {
        if let Some(bg) = line.style.bg {
            assert_eq!(
                bg, expected_bg,
                "Assistant lines should use theme message background"
            );
        }
    }
}

/// Compares two rendered bodies span-for-span (content + style), so cache reuse
/// can't silently swap in a different-but-equal-looking line.
fn assert_same_lines(a: &RenderedBody, b: &RenderedBody) {
    assert_eq!(a.lines.len(), b.lines.len(), "line count differs");
    for (i, (la, lb)) in a.lines.iter().zip(b.lines.iter()).enumerate() {
        assert_eq!(la.style, lb.style, "line {i} style differs");
        assert_eq!(
            la.spans.len(),
            lb.spans.len(),
            "line {i} span count differs"
        );
        for (sa, sb) in la.spans.iter().zip(lb.spans.iter()) {
            assert_eq!(sa.content, sb.content, "line {i} content differs");
            assert_eq!(sa.style, sb.style, "line {i} span style differs");
        }
    }
    assert_eq!(a.line_blocks, b.line_blocks, "line_blocks differ");
}

#[test]
fn identical_renders_produce_identical_lines_and_reuse_cache() {
    // #342: a redraw with no content change must reuse every cached block —
    // zero markdown re-parse — and yield byte-identical lines.
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    app.record_user_message("hello there".to_string());
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 1,
        text: "**bold** reply with `code`\n".to_string(),
    });
    feed_tool_call(&mut app, &sid, 2, "read", r#"{"path":"src/main.rs"}"#);
    feed_reasoning(&mut app, &sid, "thinking\nmore thinking\n");

    let first = render_body_lines(&mut app, 80);
    // First pass renders every block fresh.
    assert!(app.last_render_rebuilt() > 0);

    let second = render_body_lines(&mut app, 80);
    assert_eq!(
        app.last_render_rebuilt(),
        0,
        "an unchanged redraw must rebuild no blocks"
    );
    assert_same_lines(&first, &second);
}

#[test]
fn mutating_one_entry_rebuilds_only_its_block() {
    // #342: appending a new tool call must re-render only the trailing block,
    // reusing every earlier block's cached lines.
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    app.record_user_message("first".to_string());
    feed_tool_call(&mut app, &sid, 1, "read", r#"{"path":"a.rs"}"#);
    let _ = render_body_lines(&mut app, 80);

    // A brand-new tool call is one new block; the user + first tool block stay.
    feed_tool_call(&mut app, &sid, 2, "read", r#"{"path":"b.rs"}"#);
    let _ = render_body_lines(&mut app, 80);
    assert_eq!(
        app.last_render_rebuilt(),
        1,
        "only the newly-appended block should rebuild"
    );
}

#[test]
fn toggling_one_block_rebuilds_only_it() {
    // #342: expanding a reasoning block flips only that block's key, so only it
    // re-renders.
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    feed_tool_call(&mut app, &sid, 1, "read", r#"{"path":"a.rs"}"#);
    feed_reasoning(&mut app, &sid, "alpha\nbeta\n");
    let _ = render_body_lines(&mut app, 80);

    app.toggle_block(1); // reasoning run's block id is its first delta index (1)
    let _ = render_body_lines(&mut app, 80);
    assert_eq!(
        app.last_render_rebuilt(),
        1,
        "only the toggled block should rebuild"
    );
}

#[test]
fn width_change_rebuilds_every_block() {
    // #342: a resize invalidates the whole memo (wrap width baked into lines).
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    app.record_user_message("hello".to_string());
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 1,
        text: "some reply\n".to_string(),
    });
    let _ = render_body_lines(&mut app, 80);
    let _ = render_body_lines(&mut app, 80);
    assert_eq!(app.last_render_rebuilt(), 0, "warm cache at width 80");

    let _ = render_body_lines(&mut app, 40);
    assert!(
        app.last_render_rebuilt() >= 2,
        "a width change must rebuild every block"
    );
}

#[test]
fn prose_with_inline_code_still_wraps() {
    // Regression: the old code/non-code heuristic flagged ANY line containing
    // inline `code` as a code block (because that one span carried an fg color),
    // disabling wrapping and forcing a horizontal scroll for plain prose. A
    // prose paragraph must wrap to the panel width even when it mentions `code`.
    let sid = SessionId::new("s1");
    let mut app = App::new_for_test(sid.clone());
    let long = "This is a fairly long prose paragraph that mentions an `inline_code` \
                token somewhere in the middle and should still wrap to the panel \
                width rather than force a horizontal scroll.";
    app.handle_out_event(OutEvent::TextDelta {
        session: sid.clone(),
        seq: 1,
        text: format!("{long}\n"),
    });

    let body = render_body_lines(&mut app, 40);
    let content_lines = body.lines.iter().filter(|l| !l.spans.is_empty()).count();
    assert!(
        content_lines > 1,
        "prose with inline code must wrap across multiple lines, got {content_lines}"
    );
}
