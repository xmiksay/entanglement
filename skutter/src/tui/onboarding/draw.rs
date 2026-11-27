//! Rendering for the onboarding dialog. Plain styles only: it draws before the
//! TUI's theme/config machinery exists.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use super::state::{Onboarding, Stage};

const TITLE: &str = " skutter — first-run setup: no LLM provider configured ";

pub(crate) fn draw(f: &mut Frame, o: &Onboarding) {
    let area = centered(f.area());
    f.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(TITLE);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);

    let selected = o.selected();
    let name = selected.map(|p| p.name.as_str()).unwrap_or_default();
    match o.stage() {
        Stage::Provider => {
            let items = o.providers().iter().map(|p| {
                let tag = p.key_env.clone().unwrap_or_else(|| "keyless".into());
                ListItem::new(Line::from(vec![
                    Span::styled(p.name.clone(), bold()),
                    Span::raw("  "),
                    Span::styled(tag, dim()),
                ]))
            });
            list(f, body, items.collect(), o.provider_index());
        }
        Stage::Key => {
            let env = selected.and_then(|p| p.key_env.clone()).unwrap_or_default();
            let lines = vec![
                Line::from(vec![Span::raw("Provider: "), Span::styled(name, bold())]),
                Line::from(vec![Span::raw("Saved as: "), Span::styled(env, dim())]),
                Line::from(""),
                Line::from(vec![Span::raw("Key: "), Span::raw(o.masked_key())]),
            ];
            f.render_widget(Paragraph::new(lines), body);
        }
        Stage::Base => {
            let lines = vec![
                Line::from(vec![Span::raw("Provider: "), Span::styled(name, bold())]),
                Line::from(""),
                Line::from(vec![Span::raw("Base URL: "), Span::raw(o.base_text())]),
            ];
            f.render_widget(Paragraph::new(lines), body);
        }
        Stage::Model => {
            let models = selected.map(|p| p.models.as_slice()).unwrap_or_default();
            let default = selected.map(|p| p.default_model.as_str());
            let items = models.iter().map(|m| {
                let mut spans = vec![Span::raw(m.clone())];
                if Some(m.as_str()) == default {
                    spans.push(Span::styled("  (default)", dim()));
                }
                ListItem::new(Line::from(spans))
            });
            list(f, body, items.collect(), o.model_index());
        }
    }
    let help = match o.stage() {
        Stage::Provider => {
            "↑/↓ choose · Enter next · Esc continue in echo mode (no LLM) · Ctrl+C quit"
        }
        Stage::Key => "Type the API key (hidden) · Enter next · Esc back",
        Stage::Base => "Edit the endpoint URL · Enter next · Esc back",
        Stage::Model => "↑/↓ choose · Enter save and start · Esc back",
    };
    let help = Paragraph::new(Span::styled(help, dim())).wrap(Wrap { trim: true });
    f.render_widget(help, footer);
}

fn list(f: &mut Frame, area: Rect, items: Vec<ListItem>, selected: usize) {
    let mut state = ListState::default().with_selected(Some(selected));
    let list = List::new(items)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut state);
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// A dialog-sized rect centred in `area`, never larger than the screen.
fn centered(area: Rect) -> Rect {
    let width = area.width.min(76);
    let height = area.height.min(18);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use entanglement_provider::Catalog;
    use ratatui::{
        backend::TestBackend,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
        Terminal,
    };

    use super::super::state::options;
    use super::*;

    fn screen(o: &Onboarding) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| draw(f, o)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn the_key_never_reaches_the_screen() {
        let mut o = Onboarding::new(options(&Catalog::builtin()));
        o.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for c in "hunter2".chars() {
            o.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let shown = screen(&o);
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("•••••••"), "{shown}");
    }

    #[test]
    fn the_first_stage_explains_the_echo_escape_hatch() {
        let o = Onboarding::new(options(&Catalog::builtin()));
        let shown = screen(&o);
        assert!(shown.contains("echo mode"), "{shown}");
        assert!(shown.contains("ollama"), "{shown}");
    }
}
