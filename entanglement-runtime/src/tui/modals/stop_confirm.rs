use ratatui::{
    layout::Alignment,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use super::centered_rect;
use crate::tui::app::App;
use crate::tui::format::short_id;

/// The cascade-vs-detach `Stop` confirm (#626, ADR-0145 "Consequences"): a
/// plan session parked on a live sponsored `propose_plan` build child offers
/// the choice instead of always detaching.
pub fn draw_stop_confirm_modal(f: &mut Frame, app: &App) {
    let Some(confirm) = app.stop_confirm() else {
        return;
    };

    let lines = vec![
        Line::from(Span::styled(
            "Stop the plan session?",
            Style::default().fg(Color::Yellow).bold(),
        )),
        Line::from(""),
        Line::from(format!(
            "It has a sponsored build session (`{}`) still running.",
            short_id(&confirm.build_child)
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("[Enter/y/d]", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" detach — leave the build running"),
        ]),
        Line::from(vec![
            Span::styled("[c]", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" cascade — stop the build too"),
        ]),
        Line::from(vec![
            Span::styled("[Esc/n]", Style::default().fg(Color::Cyan).bold()),
            Span::raw(" cancel"),
        ]),
    ];

    let area = centered_rect(50, 40, f.area());
    f.render_widget(Clear, area);
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Stop "))
        .wrap(Wrap { trim: true })
        .alignment(Alignment::Left);
    f.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use entanglement_core::SessionId;
    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    fn rendered(f: impl FnOnce(&mut ratatui::Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|frame| f(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..30)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn armed_confirm_renders_the_build_child_and_both_choices() {
        let plan = SessionId::new("plan");
        let build = SessionId::new("build-child-id");
        let mut app = App::new_for_test(plan.clone());
        app.arm_stop_confirm(plan, build.clone());

        let text = rendered(|f| draw_stop_confirm_modal(f, &app));

        assert!(text.contains(&crate::tui::format::short_id(&build)));
        assert!(text.contains("detach"));
        assert!(text.contains("cascade"));
    }

    #[test]
    fn no_confirm_armed_renders_nothing() {
        let plan = SessionId::new("plan");
        let app = App::new_for_test(plan);

        let text = rendered(|f| draw_stop_confirm_modal(f, &app));

        assert!(text.trim().is_empty(), "got:\n{text}");
    }
}
