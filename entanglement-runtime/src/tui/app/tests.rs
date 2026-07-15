use super::{App, ProfileInfo};
use crate::tui::mention::{FileIndex, MentionPopup};
use crate::tui::session_view::TranscriptEntry;
use entanglement_core::{AgentMode, AgentState, OutEvent, SessionId};
use entanglement_provider::{Catalog, ModelInfo};
use ratatui::layout::Rect;

/// Build an `App` over a custom entry-agent roster so the Tab-cycle ring
/// filtering (#322) can be exercised: `build`/`plan` as `Primary`, `helper` as
/// `All`.
fn app_with_mixed_modes(sid: SessionId) -> App {
    App::new(
        sid,
        Catalog::builtin(),
        vec![
            ProfileInfo {
                name: "build".to_string(),
                description: "Coding agent".to_string(),
                mode: AgentMode::Primary,
            },
            ProfileInfo {
                name: "plan".to_string(),
                description: "Planning agent".to_string(),
                mode: AgentMode::Primary,
            },
            ProfileInfo {
                name: "helper".to_string(),
                description: "Cross-vendor helper".to_string(),
                mode: AgentMode::All,
            },
        ],
    )
}

/// Move the active session's agent off the cycle ring, as the Ctrl+A picker
/// would when landing on an `all`-mode agent.
fn set_agent(app: &mut App, sid: &SessionId, agent: &str) {
    app.handle_out_event(OutEvent::AgentChanged {
        session: sid.clone(),
        agent: agent.to_string(),
        profile_detail: None,
    });
}

#[test]
fn history_up_down_navigates_and_restores_draft() {
    let mut app = App::new_for_test(SessionId::new("test"));
    app.input.insert_str("first");
    assert_eq!(app.take_input_text(), "first");
    app.input.insert_str("second");
    assert_eq!(app.take_input_text(), "second");

    // A draft is preserved as the search term and restored on the way down.
    app.input.insert_str("draft");
    app.history_up();
    assert_eq!(app.input_text(), "second");
    app.history_up();
    assert_eq!(app.input_text(), "first");
    app.history_up(); // clamps at the oldest entry
    assert_eq!(app.input_text(), "first");
    app.history_down();
    assert_eq!(app.input_text(), "second");
    app.history_down(); // past the newest → restore the draft
    assert_eq!(app.input_text(), "draft");
}

#[test]
fn history_navigation_is_a_noop_with_empty_history() {
    let mut app = App::new_for_test(SessionId::new("test"));
    app.history_up();
    app.history_down();
    assert_eq!(app.input_text(), "");
}

#[test]
fn history_up_preserves_multibyte_entry() {
    let mut app = App::new_for_test(SessionId::new("test"));
    app.input.insert_str("héllo 🚀");
    assert_eq!(app.take_input_text(), "héllo 🚀");
    app.history_up();
    assert_eq!(app.input_text(), "héllo 🚀");
}

#[test]
fn test_profile_color_for_hash() {
    let sid = SessionId::new("test");
    let app = App::new_for_test(sid);
    let color1 = app.profile_color_for("build");
    let color2 = app.profile_color_for("plan");
    let color3 = app.profile_color_for("build");

    assert_eq!(color1, color3);
    assert_ne!(color1, color2);
}

#[test]
fn test_profile_color_for_override() {
    let sid = SessionId::new("test");
    let mut app = App::new_for_test(sid);
    let hash_color = app.profile_color_for("build");

    app.profile_colors
        .insert("build".to_string(), ratatui::style::Color::Magenta);
    let override_color = app.profile_color_for("build");

    assert_ne!(hash_color, override_color);
    assert_eq!(override_color, ratatui::style::Color::Magenta);
}

#[test]
fn reasoning_block_at_maps_row_plus_offset_to_block() {
    let sid = SessionId::new("test");
    let mut app = App::new_for_test(sid);
    // Chat area at (x=2, y=1), 10 wide, 4 tall, scrolled down by 3 lines.
    let area = Rect::new(2, 1, 10, 4);
    // Rendered lines: only indices 3 and 5 belong to reasoning block 7.
    let line_blocks = vec![None, None, None, Some(7), None, Some(7), None];
    app.set_chat_hit_test(area, 3, line_blocks);

    // Top row of the area (row 1) + offset 3 → line index 3 → block 7.
    assert_eq!(app.reasoning_block_at(3, 1), Some(7));
    // row 3 + offset 3 → line 5 → block 7.
    assert_eq!(app.reasoning_block_at(5, 3), Some(7));
    // row 2 + offset 3 → line 4 → padding line, no block.
    assert_eq!(app.reasoning_block_at(5, 2), None);
}

#[test]
fn reasoning_block_at_rejects_clicks_outside_chat_rect() {
    let sid = SessionId::new("test");
    let mut app = App::new_for_test(sid);
    let area = Rect::new(2, 1, 10, 4);
    app.set_chat_hit_test(area, 3, vec![None, None, None, Some(7)]);

    assert_eq!(app.reasoning_block_at(1, 1), None, "left of area");
    assert_eq!(app.reasoning_block_at(12, 1), None, "right of area");
    assert_eq!(app.reasoning_block_at(3, 0), None, "above area");
    assert_eq!(app.reasoning_block_at(3, 5), None, "below area");
}

#[test]
fn reasoning_block_at_is_empty_before_first_draw() {
    let sid = SessionId::new("test");
    let app = App::new_for_test(sid);
    assert_eq!(app.reasoning_block_at(0, 0), None);
}

#[test]
fn test_thinking_state_tracking() {
    let sid = SessionId::new("test");
    let mut app = App::new_for_test(sid.clone());

    app.handle_out_event(OutEvent::Status {
        session: sid.clone(),
        state: AgentState::Thinking,
    });
    app.tick_thinking();

    assert!(app.thinking_since().is_some());
    assert!(matches!(app.state(), AgentState::Thinking));

    app.handle_out_event(OutEvent::Status {
        session: sid.clone(),
        state: AgentState::Done,
    });
    app.tick_thinking();

    assert!(app.thinking_since().is_none());
}

#[test]
fn accept_mention_replaces_at_token_with_path() {
    let mut app = App::new_for_test(SessionId::new("test"));
    app.mention = MentionPopup::new(FileIndex::from_paths(vec!["src/tui/app.rs".to_string()]));
    app.input.insert_str("explain @app");

    app.update_mention();
    assert!(app.mention_visible());

    assert!(app.accept_mention());
    assert_eq!(app.input_text(), "explain @src/tui/app.rs ");
    assert!(!app.mention_visible());
}

#[test]
fn record_bash_passthrough_appends_tool_call_and_output() {
    let mut app = App::new_for_test(SessionId::new("test"));
    app.record_bash_passthrough("echo hi".to_string(), "[exit 0]\nhi\n".to_string());

    let entries = app.transcript();
    assert!(matches!(
        &entries[entries.len() - 2],
        TranscriptEntry::ToolCall { tool, input } if tool == "!bash" && input == "echo hi"
    ));
    assert!(matches!(
        &entries[entries.len() - 1],
        TranscriptEntry::ToolOutput { tool: Some(t), output } if t == "!bash" && output.contains("hi")
    ));
}

#[test]
fn select_model_picker_maps_flat_index_to_provider_and_model() {
    // The picker selection is a flat index across per-provider groups (#218);
    // `select_model_picker` must resolve it to the right `(provider, model)`.
    let mut app = App::new_for_test(SessionId::new("test"));
    let groups = app.available_models().to_vec();
    assert!(!groups.is_empty(), "builtin catalog has providers");

    // First row → first provider's first model.
    app.model_picker_state().select(Some(0));
    assert_eq!(
        app.select_model_picker(),
        Some((groups[0].0.clone(), groups[0].1[0].clone()))
    );

    // A flat index landing in the second group resolves to that provider.
    if groups.len() > 1 {
        let idx = groups[0].1.len(); // first row of the second group
        app.model_picker_state().select(Some(idx));
        assert_eq!(
            app.select_model_picker(),
            Some((groups[1].0.clone(), groups[1].1[0].clone()))
        );
    }
}

#[test]
fn model_changed_event_updates_the_context_bar() {
    // A live switch (#218) surfaces `ModelChanged`; the head updates its global
    // model display from it without re-reading the catalog.
    let sid = SessionId::new("test");
    let mut app = App::new_for_test(sid.clone());
    app.handle_out_event(OutEvent::ModelChanged {
        session: sid,
        provider: "anthropic".to_string(),
        model: "claude-x".to_string(),
        context_window: Some(200_000),
    });

    let info = app.model_info();
    assert_eq!(info.id, "claude-x");
    assert_eq!(info.context_window, Some(200_000));
}

#[test]
fn set_model_info_preserves_resolved_context_window() {
    // Regression (issue #103): the resolved `ModelInfo` — context window
    // included — must be carried verbatim. Re-deriving it from the catalog
    // by id would drop the window for ids that aren't catalog keys.
    let mut app = App::new_for_test(SessionId::new("test"));
    app.set_model_info(ModelInfo {
        id: "claude-sonnet-4-5".to_string(),
        display_name: "Claude Sonnet 4.5".to_string(),
        context_window: Some(200_000),
    });

    let info = app.model_info();
    assert_eq!(info.id, "claude-sonnet-4-5");
    assert_eq!(info.display_name, "Claude Sonnet 4.5");
    assert_eq!(info.context_window, Some(200_000));
}

#[test]
fn tab_cycle_skips_mode_all_agents() {
    // The implicit Tab cycle ring is `mode: primary` only (#322): a cross-vendor
    // `all`-mode agent stays out of the ring, so cycling only ever visits
    // build↔plan.
    let sid = SessionId::new("test");
    let mut app = app_with_mixed_modes(sid.clone());
    set_agent(&mut app, &sid, "build");

    assert_eq!(app.cycle_primary_profile().as_deref(), Some("plan"));
    assert_eq!(app.cycle_primary_profile().as_deref(), Some("build"));
    assert_eq!(app.cycle_primary_profile().as_deref(), Some("plan"));
}

#[test]
fn tab_cycle_from_off_ring_agent_lands_on_first_primary() {
    // Picking `helper` (an `all`-mode agent) via Ctrl+A puts the session off the
    // ring; Tab must land on the first ring entry, not the one after index 0.
    let sid = SessionId::new("test");
    let mut app = app_with_mixed_modes(sid.clone());
    set_agent(&mut app, &sid, "helper");

    assert_eq!(app.cycle_primary_profile().as_deref(), Some("build"));
}

#[test]
fn reverse_tab_cycle_wraps_and_lands_on_last_primary_off_ring() {
    let sid = SessionId::new("test");
    let mut app = app_with_mixed_modes(sid.clone());

    // On-ring: from build, backwards wraps to plan (the last ring entry).
    set_agent(&mut app, &sid, "build");
    assert_eq!(app.cycle_primary_profile_back().as_deref(), Some("plan"));
    assert_eq!(app.cycle_primary_profile_back().as_deref(), Some("build"));

    // Off-ring `helper` → the last ring entry.
    set_agent(&mut app, &sid, "helper");
    assert_eq!(app.cycle_primary_profile_back().as_deref(), Some("plan"));
}

#[test]
fn agent_picker_still_lists_all_entry_agents() {
    // The Ctrl+A picker roster is unchanged — it still lists every entry agent
    // (`primary | all`), including the `all`-mode `helper` the Tab ring skips.
    let sid = SessionId::new("test");
    let app = app_with_mixed_modes(sid);

    let names: Vec<&str> = app
        .available_profiles()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, vec!["build", "plan", "helper"]);
}
