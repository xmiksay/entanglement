use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem},
    Frame,
};

use crate::tui::app::App;
use crate::tui::keybindings::KeyMap;
use crate::tui::ui::agent_color;

pub fn draw_profile_picker(f: &mut Frame, app: &mut App) {
    let profiles = app.available_profiles().to_vec();
    let items: Vec<ListItem> = profiles
        .iter()
        .map(|p| {
            let color = agent_color(&p.name);
            ListItem::new(Line::from(vec![
                Span::styled("[", Style::default().dim()),
                Span::styled(&p.name, Style::default().fg(color).bold()),
                Span::styled("]", Style::default().dim()),
                Span::raw(" "),
                Span::styled(&p.description, Style::default().dim()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Select Agent Profile (Esc to close, Enter to select)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(60, 40, f.area());
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.profile_picker_state());
}

pub fn draw_sessions_modal(f: &mut Frame, app: &mut App) {
    let active = app.active_session_id().clone();
    let rows: Vec<ListItem> = app
        .sessions()
        .into_iter()
        .map(|(id, view)| {
            let marker = if *id == active { "▸ " } else { "  " };
            let color = agent_color(view.agent());
            let mut spans = vec![
                Span::raw(marker),
                Span::styled(id.to_string(), Style::default().bold()),
                Span::raw(" "),
                Span::styled("[", Style::default().dim()),
                Span::styled(view.agent().to_string(), Style::default().fg(color).bold()),
                Span::styled("]", Style::default().dim()),
                Span::raw(format!(" {:?}", view.state())),
            ];
            if view.is_waiting_approval() {
                spans.push(Span::styled(
                    " ⏳ approval",
                    Style::default().fg(Color::Yellow),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(rows)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Sessions (Enter: switch, n: new, Esc: close)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(60, 40, f.area());
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.sessions_modal_state());
}

pub fn draw_which_key_popup(f: &mut Frame, keymap: &KeyMap) {
    let bindings = keymap.all_bindings();
    let mut lines = Vec::new();

    for (sequence, action) in bindings {
        let key_str = format!("{}", sequence);
        let description = action.description();
        lines.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!(" {:8} ", key_str),
                Style::default().fg(Color::Cyan).bold(),
            ),
            Span::styled(description, Style::default()),
        ])));
    }

    let list = List::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Keybindings (Esc to cancel)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(80, 60, f.area());
    f.render_widget(Clear, area);
    f.render_widget(list, area);
}

pub fn draw_help_dialog(f: &mut Frame, keymap: &KeyMap) {
    let bindings = keymap.all_bindings();
    let mut current_category = String::new();
    let mut lines = Vec::new();

    for (sequence, action) in bindings {
        let category = action.category();
        if category != current_category {
            if !current_category.is_empty() {
                lines.push(ListItem::new(Line::from("")));
            }
            lines.push(ListItem::new(Line::from(vec![Span::styled(
                format!("{}:", category),
                Style::default().fg(Color::Yellow).bold(),
            )])));
            current_category = category.to_string();
        }

        let key_str = format!("{}", sequence);
        let description = action.description();
        lines.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("  {:12} ", key_str),
                Style::default().fg(Color::Cyan).bold(),
            ),
            Span::styled(description, Style::default()),
        ])));
    }

    let list = List::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Keybindings Help (Esc to close)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);
    f.render_widget(list, area);
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
