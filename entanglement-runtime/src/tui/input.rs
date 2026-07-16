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

    /// Word-jump left (Ctrl+Left): skip any trailing whitespace to the left of
    /// the cursor, then skip exactly one run of non-whitespace chars. Stays on
    /// the current line (emacs-`M-b`-style, line-local). Char-boundary-safe.
    pub fn move_word_left(&mut self) {
        let line = self.line_str();
        let mut col = floor_char_boundary(line, self.cursor_col);
        while col > 0 {
            let Some(prev) = line[..col].chars().next_back() else {
                break;
            };
            if prev.is_whitespace() {
                col -= prev.len_utf8();
            } else {
                break;
            }
        }
        while col > 0 {
            let Some(prev) = line[..col].chars().next_back() else {
                break;
            };
            if !prev.is_whitespace() {
                col -= prev.len_utf8();
            } else {
                break;
            }
        }
        self.cursor_col = col;
    }

    /// Word-jump right (Ctrl+Right): skip the rest of the current word, then
    /// cross the following whitespace — landing at the **start of the next
    /// word** (the convention VS Code / most editors use). Char-boundary-safe.
    pub fn move_word_right(&mut self) {
        let line = self.line_str();
        let mut col = floor_char_boundary(line, self.cursor_col);
        // Skip the remainder of the current word (non-whitespace)…
        while col < line.len() {
            let Some(next) = line[col..].chars().next() else {
                break;
            };
            if !next.is_whitespace() {
                col += next.len_utf8();
            } else {
                break;
            }
        }
        // …then the following whitespace, to reach the next word's start.
        while col < line.len() {
            let Some(next) = line[col..].chars().next() else {
                break;
            };
            if next.is_whitespace() {
                col += next.len_utf8();
            } else {
                break;
            }
        }
        self.cursor_col = col;
    }

    /// Jump to the start of the document (Ctrl+Home): first row, column 0.
    pub fn move_to_doc_home(&mut self) {
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Jump to the end of the document (Ctrl+End): last row, end of its line.
    /// Floored to a char boundary so a clamped column never splits a glyph.
    pub fn move_to_doc_end(&mut self) {
        self.cursor_row = self.lines.len().saturating_sub(1);
        let line = self.line_str();
        self.cursor_col = floor_char_boundary(line, line.len());
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
mod tests;
