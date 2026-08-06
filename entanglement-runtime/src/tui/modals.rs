use ratatui::layout::{Constraint, Direction, Layout, Rect};

mod inspect;
mod popups;
mod sessions;
mod stop_confirm;
mod tool_popups;

pub use inspect::draw_inspect_overlay;
pub use popups::{
    draw_command_palette, draw_help_dialog, draw_key_dialog, draw_mention_popup,
    draw_slash_autocomplete, draw_which_key_popup,
};
pub use sessions::{
    draw_model_picker, draw_profile_picker, draw_resume_modal, draw_sessions_modal,
};
pub use stop_confirm::draw_stop_confirm_modal;
pub use tool_popups::{draw_mcp_panel, draw_session_tools_dialog, draw_tools_dialog};

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// The slash-autocomplete popup rect (Issue 1 + Issue 2): anchored above the
/// input box, sized to the current match count (capped), never overflowing the
/// top of the screen. Shared between `draw_slash_autocomplete` and the mouse
/// hit-test path so the two can't drift.
pub(crate) fn slash_popup_area(input_area: Rect, item_count: usize) -> Rect {
    let height = (item_count as u16 + 2).min(15).min(input_area.y);
    Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width.min(60),
        height,
    }
}

/// The `@file` mention popup rect — the slash popup's shape at a slightly
/// wider max width. Shared between `draw_mention_popup` and the mouse hit-test
/// path so the two can't drift.
pub(crate) fn mention_popup_area(input_area: Rect, item_count: usize) -> Rect {
    let height = (item_count as u16 + 2).min(15).min(input_area.y);
    Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width: input_area.width.min(70),
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::centered_rect;
    use ratatui::layout::Rect;

    #[test]
    fn centered_rect_is_centered_and_proportional() {
        let full = Rect::new(0, 0, 100, 100);
        let inner = centered_rect(60, 40, full);
        assert_eq!(inner.width, 60);
        assert_eq!(inner.height, 40);
        // Symmetric margins: (100 - 60) / 2 == 20 on each side.
        assert_eq!(inner.x, 20);
        assert_eq!(inner.y, 30);
        // Fully contained within the parent area.
        assert!(inner.right() <= full.right() && inner.bottom() <= full.bottom());
    }
}
