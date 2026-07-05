use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::tui::app::App;

pub fn draw(f: &mut Frame, app: &mut App) {
    let size = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(size);

    draw_header(f, chunks[0]);
    draw_body(f, chunks[1], app);
    draw_footer(f, chunks[2]);

    app.clear_dirty();
}

fn draw_header(f: &mut Frame, area: Rect) {
    let header = Line::from(vec![
        Span::styled("skutter", Style::default().bold()),
        Span::raw(" — TUI mode (Press 'q' or Ctrl+C to quit)"),
    ]);

    let paragraph = Paragraph::new(header)
        .alignment(Alignment::Center)
        .block(Block::new().borders(Borders::BOTTOM));

    f.render_widget(paragraph, area);
}

fn draw_body(f: &mut Frame, area: Rect, app: &App) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from("  TUI scaffold running."),
        Line::from("  Session ID: ").blue().bold(),
        Line::from(format!("    {}", app.session_id())),
        Line::from(""),
        Line::from("  Features to be implemented:"),
        Line::from("    - Holly event rendering (#3)"),
        Line::from("    - Input handling (#4)"),
        Line::from("    - Keybindings (#7)"),
    ]);

    let paragraph = Paragraph::new(text)
        .wrap(Wrap { trim: true })
        .block(Block::new().borders(Borders::ALL));

    f.render_widget(paragraph, area);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let footer = Line::from(vec![
        Span::styled("Ready", Style::default().bold()),
        Span::raw(" | "),
        Span::styled("Ctrl+C", Style::default().dim()),
        Span::raw(" or "),
        Span::styled("q", Style::default().dim()),
        Span::raw(" to quit"),
    ]);

    let paragraph = Paragraph::new(footer)
        .alignment(Alignment::Center)
        .block(Block::new().borders(Borders::TOP));

    f.render_widget(paragraph, area);
}
