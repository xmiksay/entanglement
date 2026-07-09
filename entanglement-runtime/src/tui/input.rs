//! The TUI input editor buffer. A `cursor_col` is a **byte** offset into the
//! current line kept on a UTF-8 char boundary; movement/edit helpers step whole
//! `char`s so multibyte input never splits a code point (issue #101).

use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Default)]
pub struct SimpleInput {
    lines: Vec<String>,
    cursor_row: usize,
    /// Byte offset of the cursor within `lines[cursor_row]`, always kept on a
    /// UTF-8 char boundary. All movement/edit helpers step by whole `char`s so
    /// multibyte input (`é`, emoji, CJK) never splits a code point (issue #101).
    cursor_col: usize,
    scroll_offset: u16,
}

/// Largest char boundary `<= idx` in `s` (clamps `idx` into range first). Lets
/// callers land a byte cursor on a valid boundary after clamping to a line len.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

impl SimpleInput {
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    #[allow(dead_code)]
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// Cursor position as a byte offset into the current line. For the terminal
    /// cursor column, use [`cursor_display_col`](Self::cursor_display_col).
    #[allow(dead_code)]
    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    /// The current line's text (empty if the buffer has no line yet).
    fn line_str(&self) -> &str {
        self.lines
            .get(self.cursor_row)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Terminal column of the cursor: the display width of the text left of it.
    /// Distinct from `cursor_col` (bytes) for multibyte / wide glyphs.
    pub fn cursor_display_col(&self) -> usize {
        let line = self.line_str();
        let col = floor_char_boundary(line, self.cursor_col);
        UnicodeWidthStr::width(&line[..col])
    }

    pub fn insert_char(&mut self, c: char) {
        if self.cursor_row >= self.lines.len() {
            self.lines.resize(self.cursor_row + 1, String::new());
        }
        let line = &mut self.lines[self.cursor_row];
        if self.cursor_col > line.len() {
            line.extend(std::iter::repeat_n(' ', self.cursor_col - line.len()));
        }
        line.insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert_char(c);
        }
    }

    pub fn insert_newline(&mut self) {
        if self.cursor_row >= self.lines.len() {
            self.lines.resize(self.cursor_row + 1, String::new());
        }
        let col = floor_char_boundary(&self.lines[self.cursor_row], self.cursor_col);
        let after = self.lines[self.cursor_row].split_off(col);
        self.lines.insert(self.cursor_row + 1, after);
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    pub fn delete_char(&mut self) {
        if self.cursor_col > 0 {
            let Some(line) = self.lines.get_mut(self.cursor_row) else {
                self.cursor_col = 0;
                return;
            };
            let col = floor_char_boundary(line, self.cursor_col);
            if let Some(prev) = line[..col].chars().next_back() {
                let start = col - prev.len_utf8();
                line.remove(start);
                self.cursor_col = start;
            }
        } else if self.cursor_row > 0 && self.cursor_row < self.lines.len() {
            let current_line = self.lines.remove(self.cursor_row);
            let prev_line = &mut self.lines[self.cursor_row - 1];
            self.cursor_col = prev_line.len();
            prev_line.push_str(&current_line);
            self.cursor_row -= 1;
        }
    }

    pub fn delete_line_by_end(&mut self) {
        if let Some(line) = self.lines.get_mut(self.cursor_row) {
            let col = floor_char_boundary(line, self.cursor_col);
            line.truncate(col);
        }
    }

    pub fn delete_line_by_head(&mut self) {
        if let Some(line) = self.lines.get_mut(self.cursor_row) {
            let col = floor_char_boundary(line, self.cursor_col);
            *line = line.split_off(col);
        }
        self.cursor_col = 0;
    }

    pub fn delete_word(&mut self) {
        let Some(line) = self.lines.get_mut(self.cursor_row) else {
            return;
        };
        if self.cursor_col == 0 {
            return;
        }
        let col = floor_char_boundary(line, self.cursor_col);
        let before = &line[..col];
        let after = &line[col..];
        let new_before = before.trim_end();
        let removed = before.len() - new_before.len();
        if removed > 0 {
            *line = format!("{}{}", new_before, after);
            self.cursor_col = col - removed;
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_offset = 0;
    }

    pub fn move_cursor_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = floor_char_boundary(self.line_str(), self.cursor_col);
        }
    }

    pub fn move_cursor_down(&mut self) {
        if self.cursor_row < self.lines.len().saturating_sub(1) {
            self.cursor_row += 1;
            self.cursor_col = floor_char_boundary(self.line_str(), self.cursor_col);
        }
    }

    pub fn move_cursor_left(&mut self) {
        let line = self.line_str();
        let col = floor_char_boundary(line, self.cursor_col);
        if let Some(prev) = line[..col].chars().next_back() {
            self.cursor_col = col - prev.len_utf8();
        }
    }

    pub fn move_cursor_right(&mut self) {
        let line = self.line_str();
        let col = floor_char_boundary(line, self.cursor_col);
        if let Some(next) = line[col..].chars().next() {
            self.cursor_col = col + next.len_utf8();
        }
    }

    pub fn move_cursor_to_head(&mut self) {
        self.cursor_col = 0;
    }

    pub fn move_cursor_to_end(&mut self) {
        self.cursor_col = self.line_str().len();
    }

    #[allow(dead_code)]
    pub fn set_scroll_offset(&mut self, offset: u16) {
        self.scroll_offset = offset;
    }

    pub fn scroll_offset(&self) -> u16 {
        self.scroll_offset
    }

    /// The cursor's line, truncated to the bytes left of the cursor. Used to
    /// detect an active `@file` mention token (ADR-0030).
    pub fn current_line_before_cursor(&self) -> &str {
        let line = self.line_str();
        let col = floor_char_boundary(line, self.cursor_col);
        &line[..col]
    }

    /// Replace the byte range `[start, end)` on the cursor's line with `text`,
    /// leaving the cursor just past the inserted text. Used to swap an `@query`
    /// token for the selected path.
    pub fn replace_on_cursor_line(&mut self, start: usize, end: usize, text: &str) {
        let Some(line) = self.lines.get_mut(self.cursor_row) else {
            return;
        };
        let end = end.min(line.len());
        let start = start.min(end);
        line.replace_range(start..end, text);
        self.cursor_col = start + text.len();
    }
}

#[cfg(test)]
mod tests {
    use super::SimpleInput;

    /// The exact repro from issue #101: a single multibyte char followed by the
    /// mention recompute that slices the line — must not split the code point.
    #[test]
    fn multibyte_insert_then_before_cursor_slice() {
        let mut input = SimpleInput::default();
        input.insert_char('é');
        assert_eq!(input.lines(), &["é".to_string()]);
        assert_eq!(input.cursor(), (0, 'é'.len_utf8()));
        // Previously panicked: byte index 1 is not a char boundary.
        assert_eq!(input.current_line_before_cursor(), "é");
    }

    #[test]
    fn multibyte_str_insert_advances_by_bytes() {
        let mut input = SimpleInput::default();
        input.insert_str("aé🚀c");
        assert_eq!(input.lines(), &["aé🚀c".to_string()]);
        assert_eq!(input.cursor_col(), "aé🚀c".len());
        assert_eq!(input.current_line_before_cursor(), "aé🚀c");
    }

    #[test]
    fn multibyte_delete_removes_whole_char() {
        let mut input = SimpleInput::default();
        input.insert_str("aé");
        input.delete_char();
        assert_eq!(input.lines(), &["a".to_string()]);
        assert_eq!(input.cursor_col(), 1);
        input.delete_char();
        assert_eq!(input.lines(), &[String::new()]);
        assert_eq!(input.cursor_col(), 0);
    }

    #[test]
    fn multibyte_left_right_step_over_code_points() {
        let mut input = SimpleInput::default();
        input.insert_str("é🚀");
        input.move_cursor_left();
        assert_eq!(input.cursor_col(), 'é'.len_utf8());
        input.move_cursor_left();
        assert_eq!(input.cursor_col(), 0);
        input.move_cursor_left(); // clamped at head
        assert_eq!(input.cursor_col(), 0);
        input.move_cursor_right();
        assert_eq!(input.cursor_col(), 'é'.len_utf8());
        input.move_cursor_right();
        assert_eq!(input.cursor_col(), "é🚀".len());
        input.move_cursor_right(); // clamped at end
        assert_eq!(input.cursor_col(), "é🚀".len());
    }

    #[test]
    fn multibyte_head_end_and_display_col() {
        let mut input = SimpleInput::default();
        input.insert_str("é🚀"); // é width 1, 🚀 width 2
        assert_eq!(input.cursor_display_col(), 3);
        input.move_cursor_to_head();
        assert_eq!(input.cursor_col(), 0);
        assert_eq!(input.cursor_display_col(), 0);
        input.move_cursor_to_end();
        assert_eq!(input.cursor_col(), "é🚀".len());
        assert_eq!(input.cursor_display_col(), 3);
    }

    #[test]
    fn newline_splits_on_char_boundary() {
        let mut input = SimpleInput::default();
        input.insert_str("aé🚀c");
        input.move_cursor_left(); // between 🚀 and c
        input.insert_newline();
        assert_eq!(input.lines(), &["aé🚀".to_string(), "c".to_string()]);
        assert_eq!(input.cursor(), (1, 0));
    }

    #[test]
    fn move_up_down_clamps_to_char_boundary() {
        let mut input = SimpleInput::default();
        input.insert_str("aaaa");
        input.insert_newline();
        input.insert_str("é"); // row 1, cursor past the é (col 2)
        input.move_cursor_up(); // row 0, col floored to a boundary
        let (row, col) = input.cursor();
        assert_eq!(row, 0);
        assert!(input.lines()[0].is_char_boundary(col));
        input.move_cursor_down();
        let (row, col) = input.cursor();
        assert_eq!(row, 1);
        assert!(input.lines()[1].is_char_boundary(col));
    }

    #[test]
    fn delete_word_on_multibyte_line() {
        let mut input = SimpleInput::default();
        input.insert_str("héllo   ");
        input.delete_word();
        assert_eq!(input.lines(), &["héllo".to_string()]);
        assert_eq!(input.cursor_col(), "héllo".len());
    }

    // Issue #101 cluster 2: editing keys on a fresh (empty `lines`) buffer must
    // not index-out-of-bounds.
    #[test]
    fn empty_buffer_edit_keys_do_not_panic() {
        SimpleInput::default().insert_newline();
        SimpleInput::default().delete_line_by_end();
        SimpleInput::default().delete_line_by_head();
        SimpleInput::default().delete_word();
        SimpleInput::default().delete_char();
        SimpleInput::default().move_cursor_left();
        SimpleInput::default().move_cursor_right();
        SimpleInput::default().move_cursor_up();
        SimpleInput::default().move_cursor_down();
        SimpleInput::default().move_cursor_to_end();
        assert_eq!(SimpleInput::default().current_line_before_cursor(), "");
        assert_eq!(SimpleInput::default().cursor_display_col(), 0);
    }

    #[test]
    fn empty_buffer_newline_then_type() {
        let mut input = SimpleInput::default();
        input.insert_newline();
        assert_eq!(input.cursor(), (1, 0));
        input.insert_char('x');
        assert_eq!(input.lines(), &[String::new(), "x".to_string()]);
    }

    #[test]
    fn delete_char_joins_lines() {
        let mut input = SimpleInput::default();
        input.insert_str("aé");
        input.insert_newline();
        input.insert_str("bc");
        input.move_cursor_to_head();
        input.delete_char(); // join line 1 back onto line 0
        assert_eq!(input.lines(), &["aébc".to_string()]);
        assert_eq!(input.cursor(), (0, "aé".len()));
    }

    #[test]
    fn ctrl_k_and_ctrl_u_on_multibyte() {
        let mut input = SimpleInput::default();
        input.insert_str("aébéc");
        input.move_cursor_left(); // before final c
        input.move_cursor_left(); // between é and c → after "aéb"
        input.delete_line_by_end();
        assert_eq!(input.lines(), &["aéb".to_string()]);

        let mut input = SimpleInput::default();
        input.insert_str("aébéc");
        input.move_cursor_left();
        input.move_cursor_left(); // after "aéb"
        input.delete_line_by_head();
        assert_eq!(input.lines(), &["éc".to_string()]);
        assert_eq!(input.cursor_col(), 0);
    }
}
