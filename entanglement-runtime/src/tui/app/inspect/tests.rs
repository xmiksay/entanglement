//! Tests for the in-session inspection overlay (#214, drill-down #331). Sits in
//! the `inspect` module so the tests reach the private `InspectState` fields and
//! the `InspectLevel` enum directly.

use super::*;
use entanglement_core::SessionId;

#[test]
fn tab_cycles_forward_and_back() {
    assert_eq!(InspectTab::Prompt.next(), InspectTab::Agents);
    assert_eq!(InspectTab::Agents.next(), InspectTab::Skills);
    assert_eq!(InspectTab::Skills.next(), InspectTab::Prompt);
    assert_eq!(InspectTab::Prompt.prev(), InspectTab::Skills);
}

#[test]
fn prompt_tab_has_no_list_level() {
    assert!(InspectTab::Prompt.list_tab().is_none());
    assert!(InspectTab::Agents.list_tab().is_some());
    assert!(InspectTab::Skills.list_tab().is_some());
}

#[test]
fn toggle_opens_populated_overlay_and_tabs_cycle() {
    let mut app = App::new_for_test(SessionId::new("s1"));
    assert!(!app.showing_inspect());

    // Opening resolves all three views from the built-in registries (always
    // present via `include_str!`, so this is cwd-independent).
    app.toggle_inspect();
    assert!(app.showing_inspect());
    assert_eq!(app.inspect_tab(), InspectTab::Prompt);
    assert!(!app.inspect_content().is_empty());
    // The Prompt tab is a scroll-only document, never a list.
    assert!(!app.inspect_showing_list());

    app.inspect_next_tab();
    assert_eq!(app.inspect_tab(), InspectTab::Agents);
    // The flat agents view lists the built-in roster as a fallback summary.
    assert!(app.inspect_content().contains("build"));
    // The Agents tab opens on its list level, with the built-in roster as
    // selectable rows.
    assert!(app.inspect_showing_list());
    assert!(!app.inspect_items().is_empty());
    assert_eq!(app.inspect_scroll(), 0, "tab switch resets scroll");

    app.inspect_scroll_down(2);
    assert!(app.inspect_scroll() > 0);
    app.inspect_prev_tab();
    assert_eq!(app.inspect_scroll(), 0, "tab switch resets scroll");

    app.toggle_inspect();
    assert!(!app.showing_inspect());
}

#[test]
fn scroll_clamps_at_top_and_bottom() {
    let mut st = InspectState {
        prompt: "a\nb\nc".to_string(),
        ..Default::default()
    };
    // Up from 0 stays at 0.
    st.scroll = 0;
    st.scroll = st.scroll.saturating_sub(5);
    assert_eq!(st.scroll, 0);
    // Bottom clamps to last line (3 lines → max offset 2).
    let max = st.content().lines().count().saturating_sub(1) as u16;
    st.scroll = 99u16.min(max);
    assert_eq!(st.scroll, 2);
}

#[test]
fn list_navigation_clamps_within_bounds() {
    let mut app = App::new_for_test(SessionId::new("s1"));
    app.toggle_inspect();
    app.inspect_next_tab(); // Agents list.

    let n = app.inspect_items().len();
    assert!(n >= 1, "built-in roster should expose at least one agent");
    let max = n - 1;

    app.inspect_list_down(n + 5); // Past the end clamps to the last row.
    assert_eq!(app.inspect_selected(), max);

    app.inspect_list_up(100); // Above 0 clamps to the first row.
    assert_eq!(app.inspect_selected(), 0);
}

#[test]
fn enter_opens_detail_backspace_returns_to_list() {
    let mut app = App::new_for_test(SessionId::new("s1"));
    app.toggle_inspect();
    app.inspect_next_tab(); // Agents list.
    assert_eq!(app.inspect.level, InspectLevel::List);
    assert!(app.inspect_showing_list());
    assert!(app.inspect.detail.is_none());

    app.inspect_open_detail();
    assert_eq!(
        app.inspect.level,
        InspectLevel::Detail,
        "Enter opens detail"
    );
    assert!(
        app.inspect.detail.is_some(),
        "detail string should be populated"
    );
    assert!(
        !app.inspect_showing_list(),
        "detail level is not a list anymore"
    );
    // The detail pane content should mention the highlighted agent's name.
    assert!(
        app.inspect_content().contains("name:"),
        "detail pane should render the per-agent detail, got: {}",
        app.inspect_content()
    );

    app.inspect_back_to_list();
    assert_eq!(
        app.inspect.level,
        InspectLevel::List,
        "Backspace returns to list"
    );
    assert!(
        app.inspect.detail.is_none(),
        "detail cleared on return to list"
    );
}

#[test]
fn tab_switch_from_detail_resets_to_list_level() {
    let mut app = App::new_for_test(SessionId::new("s1"));
    app.toggle_inspect();
    app.inspect_next_tab(); // Agents list.
    app.inspect_open_detail(); // Drill into the first agent.
    assert_eq!(app.inspect.level, InspectLevel::Detail);

    // Switching tabs from the detail level lands on the new tab's list.
    app.inspect_next_tab();
    assert_eq!(
        app.inspect.level,
        InspectLevel::List,
        "tab switch resets to list level"
    );
    assert!(app.inspect.detail.is_none(), "detail cleared on tab switch");
}

#[test]
fn prompt_tab_enter_is_a_noop() {
    let mut app = App::new_for_test(SessionId::new("s1"));
    app.toggle_inspect();
    // Prompt tab: Enter/open-detail is unreachable (no list level).
    app.inspect_open_detail();
    assert_eq!(app.inspect.level, InspectLevel::List);
    assert!(app.inspect.detail.is_none());
}
