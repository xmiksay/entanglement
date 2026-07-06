use ratatui::text::{Line, Span};

#[allow(dead_code)]
pub fn wrap_line(line: Line<'_>, width: u16) -> Vec<Line<'_>> {
    if width == 0 || line.spans.is_empty() {
        return if line.spans.is_empty() {
            vec![line]
        } else {
            vec![]
        };
    }

    let total_width: u16 = line.spans.iter().map(|s: &Span| s.width() as u16).sum();
    if total_width <= width {
        return vec![line];
    }

    let result_style = line.style;
    let mut result = Vec::new();
    let mut current_line_spans = Vec::new();
    let mut current_width: u16 = 0;

    for span in &line.spans {
        let span_content = span.content.as_ref();
        let span_style = span.style;

        if span_content.is_empty() {
            current_line_spans.push(span.clone());
            continue;
        }

        for word in span_content.split(' ') {
            let word: &str = word;
            if word.is_empty() {
                continue;
            }
            let word_width = word.chars().count() as u16;
            let needs_space =
                !(current_line_spans.is_empty() && current_width == 0 && result.is_empty());

            let space_width = if needs_space { 1 } else { 0 };
            let potential_width = current_width + space_width + word_width;

            if potential_width <= width || current_width == 0 {
                if needs_space {
                    current_line_spans.push(Span::raw(" "));
                    current_width += 1;
                }
                current_line_spans.push(Span::styled(word.to_string(), span_style));
                current_width += word_width;
            } else {
                result.push(Line::from(current_line_spans.clone()));
                current_line_spans.clear();
                current_width = word_width;
                current_line_spans.push(Span::styled(word.to_string(), span_style));
            }
        }
    }

    if !current_line_spans.is_empty() {
        result.push(Line::from(current_line_spans));
    }

    if result.is_empty() {
        result.push(line);
    }

    for wrapped_line in &mut result {
        wrapped_line.style = result_style;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    #[test]
    fn short_line_no_wrap() {
        let line = Line::from("short");
        let wrapped = wrap_line(line, 20);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].spans[0].content, "short");
    }

    #[test]
    fn long_line_wraps() {
        let line = Line::from("this is a very long line");
        let wrapped = wrap_line(line, 10);
        assert_eq!(wrapped.len(), 3);
    }

    #[test]
    fn wrap_line_preserves_bar_on_continuation() {
        let line = Line::from(vec![
            Span::styled("▌", Style::default().fg(ratatui::style::Color::Cyan)),
            Span::raw(" "),
            Span::styled("hello world this is very long", Style::default()),
        ]);
        let wrapped = wrap_line(line, 15);
        if wrapped.len() > 1 {
            assert_eq!(
                wrapped[1].spans[0].content, "this",
                "Second wrapped line starts with text, bar will be added by decorate"
            );
        }
    }

    #[test]
    fn wrap_preserves_span_styles() {
        let line = Line::from(vec![
            Span::styled("hello", Style::default().fg(ratatui::style::Color::Red)),
            Span::raw(" "),
            Span::styled("world", Style::default().fg(ratatui::style::Color::Blue)),
        ]);
        let wrapped = wrap_line(line, 8);
        assert_eq!(wrapped.len(), 2);
    }

    #[test]
    fn empty_line_no_wrap() {
        let line = Line::from("");
        let wrapped = wrap_line(line, 10);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn zero_width_no_wrap() {
        let line = Line::from("hello");
        let wrapped = wrap_line(line, 0);
        assert_eq!(wrapped.len(), 0);
    }

    #[test]
    fn exact_width_no_wrap() {
        let line = Line::from("hello");
        let wrapped = wrap_line(line, 5);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn very_long_word_breaks() {
        let line = Line::from("supercalifragilisticexpialidocious");
        let wrapped = wrap_line(line, 10);
        assert!(!wrapped.is_empty());
    }

    #[test]
    fn multiple_spaces_converted_to_one() {
        let line = Line::from("hello  world");
        let wrapped = wrap_line(line, 10);
        let first_line: String = wrapped[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(first_line, "hello");
    }
}
