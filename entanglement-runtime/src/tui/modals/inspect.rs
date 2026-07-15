use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use super::centered_rect;
use crate::inspect::InspectItem;
use crate::tui::app::{App, InspectTab};

/// Draw the in-session inspection overlay (#214, drill-down #331): a large
/// centred pane with a tab header (Prompt / Agents / Skills, active one
/// highlighted). The Prompt tab and any two-level tab's detail pane render a
/// scroll-only document; a two-level tab's **list** level renders a selectable
/// list (name + one-line summary + winning layer) whose `Enter` opens the
/// per-item detail pane.
pub fn draw_inspect_overlay(f: &mut Frame, app: &App) {
    let area = centered_rect(88, 88, f.area());
    f.render_widget(Clear, area);

    let block = tab_header_block(app.inspect_tab(), app.inspect_showing_list());

    if app.inspect_showing_list() {
        draw_list_pane(f, area, block, app.inspect_items(), app.inspect_selected());
    } else {
        draw_text_pane(f, area, block, app.inspect_content(), app.inspect_scroll());
    }
}

/// Build the bordered block with the tab header + the level-appropriate hint.
/// The hint line tells the user what the current keys do at this level.
fn tab_header_block(active: InspectTab, showing_list: bool) -> Block<'static> {
    let mut title: Vec<Span> = vec![Span::raw(" ")];
    for tab in [InspectTab::Prompt, InspectTab::Agents, InspectTab::Skills] {
        let style = if tab == active {
            Style::default().fg(Color::Black).bg(Color::Cyan).bold()
        } else {
            Style::default().dim()
        };
        title.push(Span::styled(format!(" {} ", tab.title()), style));
        title.push(Span::raw(" "));
    }
    // The hint adapts to the level so the available actions are always surfaced.
    let hint = if active.list_tab().is_some() && showing_list {
        "· Tab/←→: switch · ↑↓/jk: move · Enter: detail · Esc: close"
    } else if active.list_tab().is_some() {
        "· Tab/←→: switch · ↑↓/jk: scroll · Esc/⌫: back · Esc: close"
    } else {
        "· Tab/←→: switch · ↑↓/PgUp/PgDn: scroll · Esc: close"
    };
    title.push(Span::styled(hint, Style::default().dim()));

    Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title))
}

/// The list level (#331): a `List` with the selectable rows, the highlighted
/// row driven by a one-shot `ListState` so the `App` stays the single source of
/// truth for the selection.
fn draw_list_pane(
    f: &mut Frame,
    area: ratatui::layout::Rect,
    block: Block<'static>,
    items: &[InspectItem],
    selected: usize,
) {
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::styled(&item.name, Style::default().bold()),
                Span::raw("  "),
                Span::styled(
                    format!("[{}]", item.layer),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw("  "),
                Span::styled(&item.summary, Style::default().dim()),
            ]))
        })
        .collect();

    let list = List::new(list_items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray));

    let mut state = ListState::default();
    // Clamp the selection defensively: the App already clamps, but a `List`
    // would panic on an out-of-range `select`, so guard it here too.
    if items.is_empty() {
        state.select(None);
    } else {
        state.select(Some(selected.min(items.len() - 1)));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// The scroll-only document level (Prompt tab always; two-level tabs' detail
/// pane and the pre-resolution summary text).
fn draw_text_pane(
    f: &mut Frame,
    area: ratatui::layout::Rect,
    block: Block<'static>,
    content: &str,
    scroll: u16,
) {
    let paragraph = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use entanglement_core::SessionId;
    use ratatui::{backend::TestBackend, Terminal};

    fn render_to_string<F>(draw: F) -> String
    where
        F: FnOnce(&mut Frame),
    {
        // The hint line in the tab header is long; use a wide terminal so it
        // isn't truncated by the border (a real terminal scrolls the pane body
        // but the title is clipped past the width).
        let mut terminal = Terminal::new(TestBackend::new(140, 12)).unwrap();
        terminal.draw(draw).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..12)
            .map(|y| {
                (0..140)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn list_level_renders_highlighted_row_name() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.toggle_inspect();
        app.inspect_next_tab(); // Agents list.
        let first_name = app.inspect_items()[0].name.clone();

        let rendered = render_to_string(|f| draw_inspect_overlay(f, &app));
        assert!(
            rendered.contains(&first_name),
            "list level should render the first agent's name `{first_name}`, got:\n{rendered}"
        );
        // The list-level hint surfaces the Enter action.
        assert!(
            rendered.contains("Enter: detail"),
            "list hint should mention Enter: detail, got:\n{rendered}"
        );
    }

    #[test]
    fn detail_level_renders_detail_hint() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.toggle_inspect();
        app.inspect_next_tab(); // Agents list.
        app.inspect_open_detail(); // Drill into the first agent.

        let rendered = render_to_string(|f| draw_inspect_overlay(f, &app));
        assert!(
            rendered.contains("Esc/⌫: back"),
            "detail hint should mention back, got:\n{rendered}"
        );
    }
}
