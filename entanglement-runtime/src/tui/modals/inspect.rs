use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use super::centered_rect;
use crate::tui::app::{App, InspectTab};

/// Draw the in-session inspection overlay (#214): a large centred pane with a
/// tab header (Prompt / Agents / Skills, active one highlighted) and the current
/// tab's pre-rendered text, vertically scrollable.
pub fn draw_inspect_overlay(f: &mut Frame, app: &App) {
    let area = centered_rect(88, 88, f.area());
    f.render_widget(Clear, area);

    let active = app.inspect_tab();
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
    title.push(Span::styled(
        "· Tab/←→: switch · ↑↓/PgUp/PgDn: scroll · Esc: close",
        Style::default().dim(),
    ));

    let paragraph = Paragraph::new(app.inspect_content())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Line::from(title)),
        )
        .wrap(Wrap { trim: false })
        .scroll((app.inspect_scroll(), 0));

    f.render_widget(paragraph, area);
}
