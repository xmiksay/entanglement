use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use super::{centered_rect, get_depth};
use crate::tui::app::App;

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

pub fn draw_resume_modal(f: &mut Frame, app: &mut App) {
    let sessions = app.available_sessions().to_vec();
    let items: Vec<ListItem> = sessions
        .iter()
        .map(|meta| {
            let id_string = meta.id.to_string();
            let agent_string = meta.agent.clone();
            let model_string = meta.model.as_deref().unwrap_or("default").to_string();
            let mut spans = vec![
                Span::styled(id_string, Style::default().bold()),
                Span::raw(" "),
                Span::styled("[", Style::default().dim()),
                Span::styled(agent_string, Style::default().fg(Color::Cyan).bold()),
                Span::styled("]", Style::default().dim()),
                Span::raw(format!(" {}", model_string)),
            ];
            // First-prompt snippet after id/agent/model, dimmed (#327).
            if let Some(snippet) = meta.first_prompt.as_deref() {
                spans.push(Span::styled(
                    format!("  {}", snippet),
                    Style::default().dim(),
                ));
            }
            ListItem::new(Line::from(spans))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionMeta;
    use entanglement_core::SessionId;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn resume_modal_shows_first_prompt_snippet() {
        let sid = SessionId::new("abc123");
        let mut app = App::new_for_test(sid.clone());
        app.set_available_sessions_for_test(vec![SessionMeta {
            id: sid,
            agent: "build".to_string(),
            model: Some("glm-5.2".to_string()),
            created: 0,
            last_active: 0,
            parent: None,
            root: true,
            first_prompt: Some("fix the login bug".to_string()),
        }]);

        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|f| draw_resume_modal(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rendered: String = (0..10)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("fix the login bug"),
            "resume modal should render the first-prompt snippet, got:\n{rendered}"
        );
        assert!(rendered.contains("abc123"), "id should still render");
        assert!(rendered.contains("build"), "agent should still render");
    }
}
