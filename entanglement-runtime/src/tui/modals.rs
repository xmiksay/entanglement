use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::tui::app::App;
use crate::tui::keybindings::KeyMap;

pub fn draw_profile_picker(f: &mut Frame, app: &mut App) {
    let profiles = app.available_profiles().to_vec();
    let items: Vec<ListItem> = profiles
        .iter()
        .map(|p| {
            let color = app.profile_color_for(&p.name);
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
    let sessions_vec: Vec<_> = app.sessions().into_iter().collect();
    let mut parent_links: std::collections::HashMap<
        entanglement_core::SessionId,
        Option<entanglement_core::SessionId>,
    > = std::collections::HashMap::new();

    for (id, view) in sessions_vec.iter() {
        parent_links.insert((*id).clone(), view.parent().cloned());
    }

    fn get_depth(
        id: &entanglement_core::SessionId,
        parent_links: &std::collections::HashMap<
            entanglement_core::SessionId,
            Option<entanglement_core::SessionId>,
        >,
    ) -> usize {
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

    // Wall clock (ms) for the live spawn-duration of in-flight sub-agents (#89).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let rows: Vec<ListItem> = sessions_vec
        .into_iter()
        .map(|(id, view)| {
            let depth = get_depth(id, &parent_links);
            let indent = "  ".repeat(depth);
            let marker = if *id == active { "▸ " } else { "  " };
            let color = app.profile_color_for(view.agent());
            let mut spans = vec![
                Span::raw(format!("{}{}", indent, marker)),
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
            // Sub-agents (depth > 0) show their spawn duration: live while
            // running, fixed once ended (#89, ADR-0026).
            if depth > 0 {
                if let Some(secs) = view.elapsed_secs(now_ms) {
                    let (glyph, style) = if view.has_ended() {
                        (" ✓ ", Style::default().dim())
                    } else {
                        (" ⏱ ", Style::default().fg(Color::Cyan))
                    };
                    spans.push(Span::styled(format!("{glyph}{secs}s"), style));
                }
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

pub fn draw_command_palette(f: &mut Frame, app: &mut App) {
    let palette = app.command_palette();
    let query = palette.query().to_string();
    let commands = palette.filtered_commands().to_vec();

    let items: Vec<ListItem> = commands
        .iter()
        .map(|cmd| {
            let name = cmd.slash_name();
            let description = cmd.description();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", name),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::styled(description, Style::default().dim()),
            ]))
        })
        .collect();

    let input_paragraph = Paragraph::new(if query.is_empty() {
        "Type to filter commands..."
    } else {
        query.as_str()
    })
    .style(Style::default().fg(Color::Yellow));

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Command Palette (Esc to close, Enter to execute)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(60, 50, f.area());

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
        .split(area);

    f.render_widget(Clear, area);
    f.render_widget(input_paragraph, chunks[0]);
    f.render_stateful_widget(list, chunks[1], palette.state());
}

pub fn draw_model_picker(f: &mut Frame, app: &mut App) {
    let available_models = app.available_models().to_vec();
    let mut items = Vec::new();
    let mut model_list_indices = Vec::new();
    let mut global_model_index = 0;

    for (provider, model_list) in &available_models {
        items.push(ListItem::new(Line::from(vec![Span::styled(
            format!("{}:", provider),
            Style::default().fg(Color::Cyan).bold(),
        )])));

        for model in model_list {
            model_list_indices.push(items.len());
            let is_selected = app.model_picker_state().selected() == Some(global_model_index);
            let prefix = if is_selected { "▸ " } else { "  " };
            items.push(ListItem::new(Line::from(vec![
                Span::raw(prefix),
                Span::styled(model, Style::default()),
            ])));
            global_model_index += 1;
        }

        items.push(ListItem::new(Line::from("")));
    }

    let list_index = app
        .model_picker_state()
        .selected()
        .and_then(|i| model_list_indices.get(i).copied());

    let mut list_state = ratatui::widgets::ListState::default();
    list_state.select(list_index);

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Select Model (Esc to close, Enter: display only — restart to apply)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(50, 50, f.area());
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, &mut list_state);

    let hint = Paragraph::new("Runtime switching requires engine extension (see issue #12)")
        .style(Style::default().fg(Color::Yellow).dim())
        .alignment(Alignment::Center);
    let hint_area = Rect {
        x: area.x,
        y: area.bottom().saturating_sub(3),
        width: area.width,
        height: 3,
    };
    f.render_widget(hint, hint_area);
}

pub fn draw_slash_autocomplete(f: &mut Frame, app: &mut App, input_area: Rect) {
    let input_text = app.input().lines().join("\n");

    if !input_text.starts_with('/') || input_text.chars().count() > 1 {
        return;
    }

    let commands = crate::tui::commands::all_commands();

    let items: Vec<ListItem> = commands
        .iter()
        .map(|cmd| {
            let name = cmd.slash_name();
            let description = cmd.description();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", name),
                    Style::default().fg(Color::Cyan).bold(),
                ),
                Span::styled(description, Style::default().dim()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Commands (Tab to select)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let popup_area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(15),
        width: input_area.width.min(60),
        height: 15.min(input_area.y),
    };

    f.render_widget(Clear, popup_area);
    f.render_widget(list, popup_area);
}

pub fn draw_resume_modal(f: &mut Frame, app: &mut App) {
    let sessions = app.available_sessions().to_vec();
    let items: Vec<ListItem> = sessions
        .iter()
        .map(|meta| {
            let id_string = meta.id.to_string();
            let agent_string = meta.agent.clone();
            let model_string = meta.model.as_deref().unwrap_or("default").to_string();
            ListItem::new(Line::from(vec![
                Span::styled(id_string, Style::default().bold()),
                Span::raw(" "),
                Span::styled("[", Style::default().dim()),
                Span::styled(agent_string, Style::default().fg(Color::Cyan).bold()),
                Span::styled("]", Style::default().dim()),
                Span::raw(format!(" {}", model_string)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Resume Session (Enter: select, Esc: close)"),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let area = centered_rect(60, 40, f.area());
    f.render_widget(Clear, area);
    f.render_stateful_widget(list, area, app.resume_state());
}
