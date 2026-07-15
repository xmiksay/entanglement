use ratatui::layout::{Constraint, Direction, Layout, Rect};

mod inspect;
mod popups;
mod sessions;

pub use inspect::draw_inspect_overlay;
pub use popups::{
    draw_command_palette, draw_help_dialog, draw_key_dialog, draw_mention_popup,
    draw_slash_autocomplete, draw_which_key_popup,
};
pub use sessions::{
    draw_model_picker, draw_profile_picker, draw_resume_modal, draw_sessions_modal,
};

type ParentLinks =
    std::collections::HashMap<entanglement_core::SessionId, Option<entanglement_core::SessionId>>;

/// Depth of `id` in the spawn tree by walking parent links. Bounded at 100 hops
/// so a corrupt cyclic link map (a session that is transitively its own parent)
/// terminates instead of spinning forever.
fn get_depth(id: &entanglement_core::SessionId, parent_links: &ParentLinks) -> usize {
    let mut depth = 0;
    let mut current = id;
    while let Some(parent) = parent_links.get(current).and_then(|p| p.as_ref()) {
        depth += 1;
        current = parent;
        if depth > 100 {
            break;
        }
    }
    depth
}

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

#[cfg(test)]
mod tests {
    use super::{centered_rect, get_depth, ParentLinks};
    use entanglement_core::SessionId;
    use ratatui::layout::Rect;

    fn links(pairs: &[(&str, Option<&str>)]) -> ParentLinks {
        pairs
            .iter()
            .map(|(id, parent)| (SessionId::new(*id), parent.map(SessionId::new)))
            .collect()
    }

    #[test]
    fn depth_of_root_is_zero() {
        let map = links(&[("root", None)]);
        assert_eq!(get_depth(&SessionId::new("root"), &map), 0);
    }

    #[test]
    fn depth_counts_the_parent_chain() {
        let map = links(&[
            ("root", None),
            ("child", Some("root")),
            ("grandchild", Some("child")),
        ]);
        assert_eq!(get_depth(&SessionId::new("grandchild"), &map), 2);
        assert_eq!(get_depth(&SessionId::new("child"), &map), 1);
    }

    #[test]
    fn unknown_id_is_depth_zero() {
        let map = links(&[("root", None)]);
        assert_eq!(get_depth(&SessionId::new("ghost"), &map), 0);
    }

    #[test]
    fn cyclic_links_terminate_via_the_hop_guard() {
        // A corrupt map where two sessions are each other's parent must not spin.
        let map = links(&[("a", Some("b")), ("b", Some("a"))]);
        assert_eq!(get_depth(&SessionId::new("a"), &map), 101);
    }

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
