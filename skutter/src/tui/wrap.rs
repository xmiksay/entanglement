use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Truncate `text` to at most `max_width` display columns, appending `...` when
/// it overflows. Char-boundary safe: it walks whole `char`s and never slices
/// mid-codepoint (unlike a raw byte slice), and it measures display width so
/// wide CJK/emoji glyphs count as they render, not as one byte each.
pub fn truncate(text: &str, max_width: usize) -> String {
    let total: usize = text.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= max_width {
        return text.to_string();
    }
    // Not enough room for content beside the ellipsis: emit as many dots as fit.
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let budget = max_width - 3;
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        used += cw;
        out.push(ch);
    }
    out.push_str("...");
    out
}

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
            // Measure in display columns so CJK (width 2) and wide emoji fit
            // the same budget as ASCII — matching `Span::width()` above.
            let word_width = UnicodeWidthStr::width(word) as u16;
            let needs_space =
                !(current_line_spans.is_empty() && current_width == 0 && result.is_empty());

            let space_width = if needs_space { 1 } else { 0 };
            let potential_width = current_width + space_width + word_width;

            // A single word wider than the whole line (a long URL, a minified
            // token, an unbroken CJK run, a wide markdown-table cell) can't be
            // placed whole without overflowing. Hard-break it at the character
            // boundary to fit `width`, multibyte-safe — regardless of how much
            // room is left on the current line.
            if word_width > width {
                // If the current line already has content, finish it first so
                // the broken word starts fresh on its own line(s).
                if current_width > 0 {
                    result.push(Line::from(std::mem::take(&mut current_line_spans)));
                    current_width = 0;
                }
                for chunk in hard_break_word(word, width) {
                    let chunk_width = UnicodeWidthStr::width(chunk.as_str()) as u16;
                    if current_width + chunk_width > width && !current_line_spans.is_empty() {
                        result.push(Line::from(std::mem::take(&mut current_line_spans)));
                        current_width = 0;
                    }
                    current_line_spans.push(Span::styled(chunk, span_style));
                    current_width += chunk_width;
                }
                continue;
            }

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

/// Hard-break a single unbreakable `word` (wider than `width`) into chunks each
/// no wider than `width` display columns. Walks `char`s (never slicing
/// mid-codepoint) and measures each via `UnicodeWidthChar`, so CJK and emoji
/// glyphs count as they render.
fn hard_break_word(word: &str, width: u16) -> Vec<String> {
    if width == 0 {
        return vec![word.to_string()];
    }
    let max = width as usize;
    let mut chunks = Vec::new();
    let mut buf = String::new();
    let mut used: usize = 0;
    for ch in word.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > max && !buf.is_empty() {
            chunks.push(std::mem::take(&mut buf));
            used = 0;
        }
        // A single glyph wider than `max` (e.g. some wide emoji at width 1)
        // can't be split further — emit it alone on its own line.
        buf.push(ch);
        used += cw;
    }
    if !buf.is_empty() {
        chunks.push(buf);
    }
    if chunks.is_empty() {
        vec![word.to_string()]
    } else {
        chunks
    }
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
    fn long_token_hard_breaks_within_width() {
        // An 80-char unbreakable word at width 20 must wrap into multiple
        // lines, each no wider than 20 display columns.
        let token = "x".repeat(80);
        let wrapped = wrap_line(Line::from(token.as_str()), 20);
        assert!(wrapped.len() > 1, "long token should hard-break");
        for (i, line) in wrapped.iter().enumerate() {
            let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 20, "wrapped line {i} is {w} cols wide, expected ≤ 20");
        }
    }

    #[test]
    fn long_token_multibyte_never_panics() {
        // CJK + emoji: hard-break must walk whole codepoints and never slice
        // mid-glyph, and every wrapped line must stay within `width`.
        let s = "日本語のとても長いテキストです🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀";
        let wrapped = wrap_line(Line::from(s), 10);
        assert!(!wrapped.is_empty());
        for line in &wrapped {
            let w: usize = line
                .spans
                .iter()
                .flat_map(|s| s.content.chars())
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            assert!(w <= 10, "multibyte wrapped line exceeds width: {w}");
        }
    }

    #[test]
    fn truncate_short_string_untouched() {
        assert_eq!(truncate("hello", 40), "hello");
    }

    #[test]
    fn truncate_ascii_appends_ellipsis() {
        let s = "a".repeat(50);
        let out = truncate(&s, 40);
        assert_eq!(out.chars().count(), 40);
        assert!(out.ends_with("..."));
        assert_eq!(&out[..37], &"a".repeat(37));
    }

    #[test]
    fn truncate_multibyte_never_panics_and_stays_within_width() {
        // Accented + CJK + emoji, each glyph several bytes: a byte slice at
        // offset 37 would land mid-codepoint and panic. Width-based truncation
        // must not, and must never exceed the budget.
        let s = "héllo café 日本語テキスト 🚀🚀🚀 more text here padding padding";
        let out = truncate(s, 40);
        let width: usize = out.chars().map(|c| c.width().unwrap_or(0)).sum();
        assert!(width <= 40, "truncated width {width} exceeds 40");
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncate_tiny_width_degrades_to_dots() {
        assert_eq!(truncate("hello", 3), "...");
        assert_eq!(truncate("hello", 2), "..");
        assert_eq!(truncate("hello", 0), "");
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
