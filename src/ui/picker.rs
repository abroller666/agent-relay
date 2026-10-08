//! Choosing the one target pane. Keys follow broadcast-pane's picker:
//! ↑↓ / Ctrl+P Ctrl+N / k j move, Space or 1-9 pick a row, Enter confirms,
//! Ctrl+] goes back to the instruction, Esc / Ctrl+G / Ctrl+Q close.

use std::ops::Range;

use super::keys::Key;

#[derive(Debug, PartialEq, Eq)]
pub enum Pick {
    Stay,
    /// Enter on this row.
    Confirm(usize),
    /// Back to the instruction without changing the target.
    Back,
    Quit,
}

#[derive(Debug, Default)]
pub struct Picker {
    /// The highlighted row, which Enter confirms.
    pub cursor: usize,
    scroll: usize,
}

impl Picker {
    pub fn at(cursor: usize) -> Self {
        Self { cursor, scroll: 0 }
    }

    pub fn handle(&mut self, key: &Key, rows: usize) -> Pick {
        match key {
            Key::Up | Key::Prev => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Next => {
                if self.cursor + 1 < rows {
                    self.cursor += 1;
                }
            }
            Key::Text(t) | Key::Paste(t) => {
                for c in t.chars() {
                    match c {
                        'k' => self.cursor = self.cursor.saturating_sub(1),
                        'j' if self.cursor + 1 < rows => self.cursor += 1,
                        '1'..='9' => {
                            let n = c as usize - '1' as usize;
                            if n < rows {
                                self.cursor = n;
                            }
                        }
                        _ => {} // Space keeps the highlighted row picked
                    }
                }
            }
            Key::Enter { .. } if rows > 0 => return Pick::Confirm(self.cursor),
            Key::Pick => return Pick::Back,
            Key::Quit | Key::Esc => return Pick::Quit,
            _ => {}
        }
        Pick::Stay
    }

    /// The rows of a `len`-row list to show in `space` lines, scrolled so
    /// the cursor is visible.
    pub fn window(&mut self, len: usize, space: usize) -> Range<usize> {
        let space = space.max(1);
        self.cursor = self.cursor.min(len.saturating_sub(1));
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + space {
            self.scroll = self.cursor + 1 - space;
        }
        self.scroll = self.scroll.min(len.saturating_sub(space));
        self.scroll..len.min(self.scroll + space)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_keys_and_arrows_move_the_pick() {
        let mut p = Picker::default();
        assert_eq!(p.handle(&Key::Text("3".into()), 4), Pick::Stay);
        assert_eq!(p.cursor, 2);
        p.handle(&Key::Text("9".into()), 4); // no such row
        assert_eq!(p.cursor, 2);
        p.handle(&Key::Down, 4);
        p.handle(&Key::Down, 4);
        assert_eq!(p.cursor, 3);
        p.handle(&Key::Text("kk".into()), 4);
        assert_eq!(p.cursor, 1);
        assert_eq!(p.handle(&Key::Enter { alone: true }, 4), Pick::Confirm(1));
        assert_eq!(p.handle(&Key::Enter { alone: true }, 0), Pick::Stay);
        assert_eq!(p.handle(&Key::Pick, 4), Pick::Back);
        assert_eq!(p.handle(&Key::Quit, 4), Pick::Quit);
        assert_eq!(p.handle(&Key::Esc, 4), Pick::Quit);
    }

    #[test]
    fn window_follows_the_cursor() {
        let mut p = Picker::at(4);
        assert_eq!(p.window(10, 3), 2..5);
        p.cursor = 0;
        assert_eq!(p.window(10, 3), 0..3);
    }
}
