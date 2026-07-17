use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};

use crate::tui::wrap;

/// Render a pending `ask_user` question (ADR-0027) Claude-style: the prompt, a
/// numbered list of labelled choices with the highlighted one marked, an
/// optional "Other" free-text entry (showing the typed answer while active),
/// and a key hint footer. Every line is wrapped to `available_width` so the box
/// never overflows horizontally (#wrap).
pub(super) fn render_question<'a>(
    lines: &mut Vec<Line<'a>>,
    q: &crate::tui::session_view::PendingQuestion,
    input_text: &str,
    available_width: u16,
) {
    let accent = Color::Cyan;
    let rule_w = available_width.max(1) as usize;
    let rule = "─".repeat(rule_w);
    lines.push(Line::from(""));
    lines.push(Line::from(rule.clone()).fg(accent));

    // The "? <question>" header: wrap the question text under the 2-col "? "
    // prefix so continuation lines align under the question.
    push_wrapped_prefix(
        lines,
        "? ",
        q.question.as_str(),
        Style::default().fg(accent).bold(),
        available_width,
    );
    lines.push(Line::from(""));

    let selecting = !q.entering_free_form;
    for (i, opt) in q.options.iter().enumerate() {
        let picked = selecting && i == q.selected;
        push_choice(lines, i + 1, picked, &opt.label, available_width, accent);
        if let Some(desc) = &opt.description {
            // Description is indented 6 cols under the label text; wrap
            // continuation lines to the same indent.
            push_wrapped_indent(lines, "      ", desc, available_width);
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
        let prefix = format!(" {marker} {}. ", idx + 1);
        // "Other (type a custom answer)" — the label is bold, the parenthetical
        // dim. Wrap as one styled run under the prefix indent.
        let prefix_w = prefix.chars().count();
        let combined = "Other (type a custom answer)";
        push_wrapped_styled_prefix(lines, &prefix, combined, style, available_width, prefix_w);
        if q.entering_free_form {
            let shown = if input_text.is_empty() {
                "…".to_string()
            } else {
                input_text.to_string()
            };
            push_wrapped_indent(lines, "      › ", &shown, available_width);
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
    for wline in wrap::wrap_line(Line::from(footer), available_width) {
        lines.push(wline);
    }
    lines.push(Line::from(rule).fg(accent));
}

/// One numbered choice row, highlighted when `picked`. The label wraps under
/// the ` ❯ N. ` prefix so continuation lines align under the text.
fn push_choice<'a>(
    lines: &mut Vec<Line<'a>>,
    num: usize,
    picked: bool,
    label: &str,
    available_width: u16,
    accent: Color,
) {
    let (marker, style) = if picked {
        ("❯", Style::default().fg(accent).bold())
    } else {
        (" ", Style::default())
    };
    let prefix = format!(" {marker} {num}. ");
    let prefix_w = prefix.chars().count();
    push_wrapped_styled_prefix(lines, &prefix, label, style, available_width, prefix_w);
}

/// Wrap `text` under a leading `prefix` string, applying `style` to the text
/// spans (the prefix is unstyled raw). Continuation lines align under the text
/// via `prefix_w` cols of leading spaces.
fn push_wrapped_styled_prefix<'a>(
    lines: &mut Vec<Line<'a>>,
    prefix: &str,
    text: &str,
    style: Style,
    available_width: u16,
    prefix_w: usize,
) {
    let body_width = available_width.saturating_sub(prefix_w as u16);
    let wrapped = wrap::wrap_line(
        Line::from(Span::styled(text.to_string(), style)),
        body_width,
    );
    if wrapped.is_empty() {
        lines.push(Line::from(prefix.to_string()));
        return;
    }
    for (i, wline) in wrapped.into_iter().enumerate() {
        if i == 0 {
            let mut spans = vec![Span::raw(prefix.to_string())];
            spans.extend(wline.spans);
            let mut line = Line::from(spans);
            line.style = wline.style;
            lines.push(line);
        } else {
            let mut spans = vec![Span::raw(" ".repeat(prefix_w))];
            spans.extend(wline.spans);
            let mut line = Line::from(spans);
            line.style = wline.style;
            lines.push(line);
        }
    }
}

/// Wrap `text` (dim) under a fixed leading indent string, aligning wrapped
/// continuation lines under the text that follows the indent.
fn push_wrapped_indent<'a>(
    lines: &mut Vec<Line<'a>>,
    indent: &str,
    text: &str,
    available_width: u16,
) {
    let prefix_w = indent.chars().count();
    push_wrapped_styled_prefix(
        lines,
        indent,
        text,
        Style::default().dim(),
        available_width,
        prefix_w,
    );
}

/// Wrap a bold `prefix` (e.g. "? ") + `text` to the panel, continuing under the
/// prefix width. Used for the question header.
fn push_wrapped_prefix<'a>(
    lines: &mut Vec<Line<'a>>,
    prefix: &str,
    text: &str,
    text_style: Style,
    available_width: u16,
) {
    let prefix_w = prefix.chars().count();
    push_wrapped_styled_prefix(lines, prefix, text, text_style, available_width, prefix_w);
}
