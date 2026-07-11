use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

/// Render a pending `ask_user` question (ADR-0027) Claude-style: the prompt, a
/// numbered list of labelled choices with the highlighted one marked, an
/// optional "Other" free-text entry (showing the typed answer while active),
/// and a key hint footer.
pub(super) fn render_question<'a>(
    lines: &mut Vec<Line<'a>>,
    q: &crate::tui::session_view::PendingQuestion,
    input_text: &str,
) {
    let accent = Color::Cyan;
    lines.push(Line::from(""));
    lines.push(Line::from("─".repeat(60)).fg(accent));
    lines.push(Line::from(vec![
        Span::styled("?", Style::default().fg(accent).bold()),
        Span::raw(" "),
        Span::styled(q.question.clone(), Style::default().bold()),
    ]));
    lines.push(Line::from(""));

    let selecting = !q.entering_free_form;
    for (i, opt) in q.options.iter().enumerate() {
        let picked = selecting && i == q.selected;
        push_choice(lines, i + 1, picked, &opt.label);
        if let Some(desc) = &opt.description {
            lines.push(Line::from(Span::styled(
                format!("      {desc}"),
                Style::default().dim(),
            )));
        }
    }

    if q.allow_free_form {
        let idx = q.options.len();
        let picked = q.selected == idx;
        let marker = if picked { "❯" } else { " " };
        let style = if picked {
            Style::default().fg(accent).bold()
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} {}. ", idx + 1), style),
            Span::styled("Other", style),
            Span::styled(" (type a custom answer)", Style::default().dim()),
        ]));
        if q.entering_free_form {
            let shown = if input_text.is_empty() {
                "…".to_string()
            } else {
                input_text.to_string()
            };
            lines.push(Line::from(vec![
                Span::raw("      › "),
                Span::styled(shown, Style::default().fg(Color::White)),
            ]));
        }
    }

    lines.push(Line::from(""));
    let footer = if q.entering_free_form {
        vec![
            Span::styled("[Enter]", Style::default().fg(Color::Green).bold()),
            Span::raw(" submit  "),
            Span::styled("[Esc]", Style::default().fg(Color::Gray).bold()),
            Span::raw(" back"),
        ]
    } else {
        vec![
            Span::styled("[↑/↓]", Style::default().fg(accent).bold()),
            Span::raw(" select  "),
            Span::styled("[1-9]", Style::default().fg(accent).bold()),
            Span::raw(" pick  "),
            Span::styled("[Enter]", Style::default().fg(Color::Green).bold()),
            Span::raw(" choose  "),
            Span::styled("[Esc]", Style::default().fg(Color::Gray).bold()),
            Span::raw(" interrupt"),
        ]
    };
    lines.push(Line::from(footer));
    lines.push(Line::from("─".repeat(60)).fg(accent));
}

/// One numbered choice row, highlighted when `picked`.
fn push_choice<'a>(lines: &mut Vec<Line<'a>>, num: usize, picked: bool, label: &str) {
    let (marker, style) = if picked {
        ("❯", Style::default().fg(Color::Cyan).bold())
    } else {
        (" ", Style::default())
    };
    lines.push(Line::from(vec![
        Span::styled(format!(" {marker} {num}. "), style),
        Span::styled(label.to_string(), style),
    ]));
}
