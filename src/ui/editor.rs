//! The instruction being typed: multi-line text with a cursor. Nothing
//! typed here goes anywhere until the popup sends the whole instruction.

use unicode_width::UnicodeWidthChar;

/// Text and a cursor, as a char index into it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editor {
    chars: Vec<char>,
    cursor: usize,
}

impl Editor {
    pub fn new(text: &str, cursor: usize) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = cursor.min(chars.len());
        Self { chars, cursor }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert(&mut self, s: &str) {
        for c in s.chars() {
            // Only text, tabs and line breaks; never control characters.
            if c.is_control() && c != '\n' && c != '\t' {
                continue;
            }
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    fn line_start(&self) -> usize {
        self.chars[..self.cursor]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.chars[self.cursor..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(self.chars.len(), |i| self.cursor + i)
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    pub fn kill_before(&mut self) {
        let start = self.line_start();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn kill_after(&mut self) {
        let end = self.line_end();
        self.chars.drain(self.cursor..end);
    }

    pub fn kill_word(&mut self) {
        let mut start = self.cursor;
        while start > 0 && self.chars[start - 1].is_whitespace() && self.chars[start - 1] != '\n' {
            start -= 1;
        }
        while start > 0 && !self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Moves to the previous line, keeping the column where possible.
    pub fn up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            return;
        }
        let col = self.cursor - start;
        self.cursor = start - 1;
        let prev = self.line_start();
        self.cursor = (prev + col).min(start - 1);
    }

    pub fn down(&mut self) {
        let end = self.line_end();
        if end == self.chars.len() {
            return;
        }
        let col = self.cursor - self.line_start();
        self.cursor = end + 1;
        let next_end = self.line_end();
        self.cursor = (end + 1 + col).min(next_end);
    }

    /// The text as display rows of at most `width` columns, and the row and
    /// column the cursor is at.
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(2);
        let mut rows = vec![String::new()];
        let mut col = 0;
        let mut at = (0, 0);
        for (i, &c) in self.chars.iter().enumerate() {
            if i == self.cursor {
                at = (rows.len() - 1, col);
            }
            if c == '\n' {
                rows.push(String::new());
                col = 0;
                continue;
            }
            let (shown, w) = if c == '\t' {
                (' ', 1)
            } else {
                (c, c.width().unwrap_or(0))
            };
            if col + w > width {
                rows.push(String::new());
                col = 0;
            }
            rows.last_mut().unwrap().push(shown);
            col += w;
        }
        if self.cursor == self.chars.len() {
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            at = (rows.len() - 1, col);
        }
        (rows, at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_lines() {
        let mut e = Editor::default();
        e.insert("ab\ncd");
        e.home();
        e.insert("X");
        assert_eq!(e.text(), "ab\nXcd");
        e.up();
        assert_eq!(e.cursor(), 1);
        e.kill_after();
        assert_eq!(e.text(), "a\nXcd");
        e.down();
        e.end();
        e.kill_before();
        assert_eq!(e.text(), "a\n");
        e.insert("one two");
        e.kill_word();
        assert_eq!(e.text(), "a\none ");
        e.backspace();
        e.left();
        e.delete();
        assert_eq!(e.text(), "a\non");
    }

    #[test]
    fn control_characters_are_not_inserted() {
        let mut e = Editor::default();
        e.insert("a\x1b[31mb\u{7}");
        assert_eq!(e.text(), "a[31mb");
    }

    #[test]
    fn layout_wraps_wide_characters() {
        let e = Editor::new("日本語です", 5);
        let (rows, at) = e.layout(6);
        assert_eq!(rows, ["日本語", "です"]);
        assert_eq!(at, (1, 4));
    }
}
