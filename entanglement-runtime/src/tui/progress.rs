use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::Paragraph,
    Frame,
};
use std::time::Instant;

use crate::tui::theme::{darken, Theme};

pub(crate) fn draw_ship_cruise(
    f: &mut Frame,
    area: Rect,
    since: Instant,
    profile_color: Color,
    theme: Theme,
) {
    if area.width < 2 || area.height < 1 {
        return;
    }

    let w = area.width as usize;
    let trail = 3.min(w.saturating_sub(1));
    let span = w - trail;

    let period_ms = 1400;
    let phase = (since.elapsed().as_millis() % period_ms) as f32 / period_ms as f32;
    let t = if phase < 0.5 {
        phase * 2.0
    } else {
        (1.0 - phase) * 2.0
    };

    let nose = (t * (span - 1) as f32) as usize;
    let going_right = phase < 0.5;

    let mut spans = Vec::new();
    let ship_char = if going_right {
        theme.ship_right
    } else {
        theme.ship_left
    };

    for i in 0..w {
        if i == nose {
            spans.push(ship_char);
        } else if going_right && i > nose && i < nose + trail {
            let _trail_factor = if trail > 1 {
                1.0 - (i - nose) as f32 / trail as f32
            } else {
                0.6
            };
            spans.push(theme.trail_glyph);
        } else if !going_right && i < nose && i > nose.saturating_sub(trail) {
            let _trail_factor = if trail > 1 {
                1.0 - (nose - i) as f32 / trail as f32
            } else {
                0.6
            };
            spans.push(theme.trail_glyph);
        } else {
            spans.push(' ');
        }
    }

    let line = Line::from(
        spans
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                let style = if i == nose {
                    Style::default().fg(profile_color)
                } else if c == theme.trail_glyph {
                    Style::default().fg(darken(profile_color, 0.5))
                } else {
                    Style::default()
                };
                ratatui::text::Span::styled(c.to_string(), style)
            })
            .collect::<Vec<_>>(),
    );

    // Match the input panel's background so the animated indicator doesn't
    // render as a black strip against the panel — the ship occupies the same
    // row as the profile badge, which sits on `theme.input_bg`.
    let paragraph = Paragraph::new(line)
        .alignment(ratatui::layout::Alignment::Left)
        .style(Style::default().bg(theme.input_bg));
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn test_draw_ship_cruise_renders() {
        let backend = TestBackend::new(20, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let theme = Theme::default();
        let since = Instant::now();
        let profile_color = Color::Cyan;

        terminal
            .draw(|f| {
                let area = Rect::new(0, 1, 20, 1);
                draw_ship_cruise(f, area, since, profile_color, theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let has_content = buffer.content().iter().any(|cell| cell.symbol() != " ");
        assert!(has_content, "Ship cruise should render content");
    }

    #[test]
    fn test_draw_ship_cruise_respects_bounds() {
        let backend = TestBackend::new(5, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let theme = Theme::default();
        let since = Instant::now();
        let profile_color = Color::Cyan;

        terminal
            .draw(|f| {
                let area = Rect::new(0, 1, 2, 1);
                draw_ship_cruise(f, area, since, profile_color, theme);
            })
            .unwrap();
    }
}
