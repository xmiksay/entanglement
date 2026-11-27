//! Mouse-driven text selection over the transcript. The transcript renders as a
//! single flat `Paragraph`, so there is no native selection and mouse capture
//! disables the terminal's own — this module models a selection in
//! rendered-line coordinates and provides the two pure operations the view
//! needs: extract the selected text (to copy) and paint a highlight over it.
//!
//! Coordinates are `(rendered_line_idx, char_col)` in absolute (pre-scroll)
//! line space, matching the geometry `App` records in its chat hit-test.
//! Columns are counted in `char`s (not display cells); transcript text is
//! overwhelmingly simple, and a copy interaction is transient, so grapheme-width
//! precision isn't worth the complexity.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// An in-progress or settled selection. `anchor` is where the drag began,
/// `cursor` its current end; `moved` distinguishes a real drag from a bare click
/// (which the view treats as a block toggle, preserving the pre-selection UX).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub cursor: (usize, usize),
    pub moved: bool,
}

impl Selection {
    /// Start a fresh, zero-width selection at `pos` (a mouse-down).
    pub fn new(pos: (usize, usize)) -> Self {
        Self {
            anchor: pos,
            cursor: pos,
            moved: false,
        }
    }

    /// `(start, end)` ordered so downstream code ignores drag direction. Tuples
    /// compare lexicographically → orders by line, then column.
    pub fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// A collapsed selection (anchor == cursor) covers no text.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }
}

/// The plain text of a rendered line — its spans' contents concatenated.
pub fn line_to_string(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The selected text across `lines`, newline-joined. Out-of-range indices/columns
/// are clamped, so a drag past the end of content yields the content up to there.
pub fn selection_text(lines: &[String], sel: &Selection) -> String {
    if sel.is_empty() || lines.is_empty() {
        return String::new();
    }
    let ((s_line, s_col), (e_line, e_col)) = sel.ordered();
    if s_line >= lines.len() {
        return String::new();
    }
    let e_line = e_line.min(lines.len() - 1);
    if s_line == e_line {
        return slice_chars(&lines[s_line], s_col, e_col);
    }
    let mut out = slice_chars(&lines[s_line], s_col, usize::MAX);
    for line in &lines[s_line + 1..e_line] {
        out.push('\n');
        out.push_str(line);
    }
    out.push('\n');
    out.push_str(&slice_chars(&lines[e_line], 0, e_col));
    out
}

/// Repaint the selected span of each covered line with `hl` (patched over the
/// existing per-span styling, so a reversed-video highlight reads cleanly over
/// any theme colors). A no-op for an empty selection.
pub fn apply_highlight(lines: &mut [Line<'static>], sel: &Selection, hl: Style) {
    if sel.is_empty() || lines.is_empty() {
        return;
    }
    let ((s_line, s_col), (e_line, e_col)) = sel.ordered();
    let last = lines.len() - 1;
    if s_line > last {
        return;
    }
    let e_line = e_line.min(last);
    for (idx, line) in lines.iter_mut().enumerate().take(e_line + 1).skip(s_line) {
        let (start, end) = if s_line == e_line {
            (s_col, e_col)
        } else if idx == s_line {
            (s_col, usize::MAX)
        } else if idx == e_line {
            (0, e_col)
        } else {
            (0, usize::MAX)
        };
        *line = highlight_line(line, start, end, hl);
    }
}

/// Return a copy of `line` with the `[start, end)` char range patched with `hl`,
/// splitting spans at the range boundaries so unselected text keeps its style.
fn highlight_line(line: &Line<'static>, start: usize, end: usize, hl: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    let mut col = 0usize;
    for span in &line.spans {
        let chars: Vec<char> = span.content.chars().collect();
        let span_start = col;
        let span_end = col + chars.len();
        let lo = start.max(span_start);
        let hi = end.min(span_end);
        if lo >= hi {
            spans.push(span.clone()); // no overlap
        } else {
            let (a, b, c) = (lo - span_start, hi - span_start, chars.len());
            let before: String = chars[..a].iter().collect();
            let mid: String = chars[a..b].iter().collect();
            let after: String = chars[b..c].iter().collect();
            if !before.is_empty() {
                spans.push(Span::styled(before, span.style));
            }
            spans.push(Span::styled(mid, span.style.patch(hl)));
            if !after.is_empty() {
                spans.push(Span::styled(after, span.style));
            }
        }
        col = span_end;
    }
    Line::from(spans)
}

/// Slice `s` by `char` index `[start, end)`, clamped.
fn slice_chars(s: &str, start: usize, end: usize) -> String {
    s.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn single_line_selection_slices_by_column() {
        let ls = lines(&["hello world"]);
        let sel = Selection {
            anchor: (0, 6),
            cursor: (0, 11),
            moved: true,
        };
        assert_eq!(selection_text(&ls, &sel), "world");
    }

    #[test]
    fn reversed_drag_direction_normalizes() {
        let ls = lines(&["hello world"]);
        // cursor before anchor — same result as the forward drag.
        let sel = Selection {
            anchor: (0, 11),
            cursor: (0, 6),
            moved: true,
        };
        assert_eq!(selection_text(&ls, &sel), "world");
    }

    #[test]
    fn multi_line_selection_joins_with_newlines() {
        let ls = lines(&["first line", "middle", "last line"]);
        let sel = Selection {
            anchor: (0, 6),
            cursor: (2, 4),
            moved: true,
        };
        assert_eq!(selection_text(&ls, &sel), "line\nmiddle\nlast");
    }

    #[test]
    fn out_of_range_columns_clamp() {
        let ls = lines(&["short"]);
        let sel = Selection {
            anchor: (0, 2),
            cursor: (0, 999),
            moved: true,
        };
        assert_eq!(selection_text(&ls, &sel), "ort");
    }

    #[test]
    fn empty_selection_is_blank() {
        let ls = lines(&["abc"]);
        let sel = Selection::new((0, 1));
        assert!(selection_text(&ls, &sel).is_empty());
    }

    #[test]
    fn line_to_string_concatenates_spans() {
        let line = Line::from(vec![Span::raw("foo"), Span::raw("bar")]);
        assert_eq!(line_to_string(&line), "foobar");
    }

    #[test]
    fn highlight_splits_spans_at_selection_bounds() {
        use ratatui::style::Modifier;
        let mut ls = vec![Line::from("hello world")];
        let sel = Selection {
            anchor: (0, 6),
            cursor: (0, 11),
            moved: true,
        };
        apply_highlight(
            &mut ls,
            &sel,
            Style::default().add_modifier(Modifier::REVERSED),
        );
        // "hello " keeps default; "world" gains the reversed modifier.
        let spans = &ls[0].spans;
        assert_eq!(line_to_string(&ls[0]), "hello world");
        let reversed: String = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(reversed, "world");
    }
}
