//! Draws bare `/set`'s tabbed settings dialog and its re-pin confirmation.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

use super::centered_rect;
use crate::tui::app::App;
use crate::tui::settings_dialog::{RowView, Stage, Tab};

const TAB_SEPARATOR: &str = "│";

fn tab_label(tab: Tab) -> String {
    format!(" {} ", tab.title())
}

/// The tab under `column` in a tab bar drawn from `x0` — the same layout
/// [`draw_settings_dialog`] renders, so clicks and drawing can't drift.
pub fn settings_tab_at(x0: u16, column: u16) -> Option<Tab> {
    let mut x = x0;
    for tab in Tab::ALL {
        let width = tab_label(tab).chars().count() as u16;
        if column >= x && column < x + width {
            return Some(tab);
        }
        x += width + TAB_SEPARATOR.chars().count() as u16;
    }
    None
}

fn row_item(row: &RowView, label_width: usize) -> ListItem<'static> {
    let marker = if row.changed { "* " } else { "  " };
    let label = format!("{marker}{:<label_width$} ", row.label);
    let mut spans = if row.id == crate::tui::settings_dialog::RowId::Note && row.value.is_empty() {
        vec![Span::styled(
            format!("  {}", row.label),
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        let style = if row.disabled.is_some() {
            Style::default().dim()
        } else if row.changed {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        vec![
            Span::styled(label, style),
            Span::styled(row.value.clone(), style),
        ]
    };
    if let Some(reason) = row.disabled {
        spans.push(Span::styled(
            format!("  — {reason}"),
            Style::default().dim(),
        ));
    }
    ListItem::new(Line::from(spans))
}

pub fn draw_settings_dialog(f: &mut Frame, app: &mut App) {
    let Some(dialog) = app.settings_dialog() else {
        return;
    };
    let (active, stage, rows, focused) = (
        dialog.tab(),
        dialog.stage(),
        dialog.rows(),
        dialog.focused_row(),
    );
    let pending = dialog.pending();

    let area = centered_rect(80, 80, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Session settings (Tab: switch, Enter: apply, Esc: cancel)");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let footer_height = 3;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(footer_height),
        ])
        .split(inner);

    let mut tab_spans = Vec::new();
    for (i, tab) in Tab::ALL.into_iter().enumerate() {
        if i > 0 {
            tab_spans.push(Span::styled(TAB_SEPARATOR, Style::default().dim()));
        }
        let style = if tab == active {
            Style::default().fg(Color::Black).bg(Color::Cyan).bold()
        } else {
            Style::default()
        };
        tab_spans.push(Span::styled(tab_label(tab), style));
    }
    f.render_widget(Paragraph::new(Line::from(tab_spans)), chunks[0]);

    let label_width = rows
        .iter()
        .filter(|r| !r.value.is_empty())
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(usize::from(inner.width / 2));
    let items: Vec<ListItem> = rows.iter().map(|r| row_item(r, label_width)).collect();
    let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));

    let pending_text = if pending.is_empty() {
        "Pending: none".to_string()
    } else {
        format!("Pending ({}): {}", pending.len(), pending.join("; "))
    };
    let footer = Paragraph::new(vec![
        Line::from(Span::styled(
            pending_text,
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            "↑↓ move · ←→/Space change · a auto-allow · PgUp/PgDn",
            Style::default().dim(),
        )),
    ])
    .wrap(Wrap { trim: true });

    let (tabs_rect, body_rect) = (chunks[0], chunks[1]);
    if let Some(d) = app.settings_dialog_mut() {
        let state = d.list_state();
        state.select(Some(focused));
        f.render_stateful_widget(list, body_rect, state);
    }
    f.render_widget(footer, chunks[2]);
    app.set_settings_rects(tabs_rect, body_rect);

    if stage == Stage::ConfirmRepin {
        draw_repin_confirm(f, area, &pending);
    }
}

fn draw_repin_confirm(f: &mut Frame, over: Rect, pending: &[String]) {
    let area = centered_rect(80, 60, over);
    f.render_widget(Clear, area);
    let changes: Vec<&String> = pending
        .iter()
        .filter(|p| p.starts_with("tool advertising") || p.starts_with("discovery"))
        .collect();
    let mut lines = vec![Line::from(Span::styled(
        "Re-pin tool advertising for this session?",
        Style::default().bold(),
    ))];
    lines.extend(changes.iter().map(|c| Line::from(format!("  {c}"))));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "This rebuilds the prompt cache and changes the tool surface for this session.",
        Style::default().fg(Color::Yellow),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter/y: apply all · n: back · Esc: cancel",
        Style::default().dim(),
    )));
    let para = Paragraph::new(lines).wrap(Wrap { trim: true }).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Confirm re-pin"),
    );
    f.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::settings_dialog::RowId;
    use entanglement_core::SessionId;
    use ratatui::{backend::TestBackend, Terminal};

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw_settings_dialog(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_tabs_rows_and_footer_at_80_by_24() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_settings_dialog();
        let text = screen(&mut app, 80, 24);
        for needle in [
            "Session",
            "Generation",
            "Tools",
            "Aux",
            "agent",
            "model",
            "Pending: none",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn renders_at_60_columns_and_the_repin_confirmation() {
        let mut app = App::new_for_test(SessionId::new("s1"));
        app.open_settings_dialog();
        let text = screen(&mut app, 60, 24);
        assert!(text.contains("Session") && text.contains("agent"), "{text}");

        // Stand in an advertising state so the mode row is live.
        app.set_advertising_state(std::sync::Arc::new(
            crate::tool_advertising::AdvertisingState::new(),
        ));
        app.open_settings_dialog();
        let d = app.settings_dialog_mut().unwrap();
        d.set_tab(Tab::Tools);
        let mode = d.rows().iter().position(|r| r.id == RowId::Mode).unwrap();
        d.focus_row(mode);
        d.activate(true);
        let _ = d.confirm();
        let text = screen(&mut app, 60, 24);
        assert!(text.contains("prompt cache"), "{text}");
    }

    #[test]
    fn tab_hit_test_matches_the_drawn_labels() {
        assert_eq!(settings_tab_at(0, 0), Some(Tab::Session));
        // " Session " is 9 wide, then the separator.
        assert_eq!(settings_tab_at(0, 9), None);
        assert_eq!(settings_tab_at(0, 10), Some(Tab::Generation));
        assert_eq!(settings_tab_at(0, 200), None);
    }
}
