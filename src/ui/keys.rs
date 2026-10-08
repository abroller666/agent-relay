//! Raw bytes from the popup's terminal to keys.
//!
//! Text arrives as UTF-8 that may be split across reads (an IME commits a
//! whole word at once; a long paste comes in pieces), so an incomplete
//! character or escape sequence is held until the next read. Pastes come
//! wrapped in bracketed-paste markers, which the popup turns on; their line
//! breaks are text. Without the markers, a line break that shares its read
//! with more input is taken as pasted text too: only an Enter that arrives
//! on its own submits.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Text(String),
    /// Enter. `alone` when it was the whole read, i.e. a key press rather
    /// than part of pasted or fast-repeated input.
    Enter {
        alone: bool,
    },
    /// Alt+Enter.
    AltEnter,
    Paste(String),
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    /// Ctrl+A / Home.
    Home,
    /// Ctrl+E / End.
    End,
    /// Ctrl+U: delete to the start of the line.
    KillBefore,
    /// Ctrl+K: delete to the end of the line.
    KillAfter,
    /// Ctrl+W: delete the word before the cursor.
    KillWord,
    /// Ctrl+P.
    Prev,
    /// Ctrl+N.
    Next,
    /// Ctrl+]: choose the target again.
    Pick,
    /// Ctrl+R: read the answer again.
    Reload,
    /// Ctrl+O: choose among the source's earlier answers.
    Answers,
    /// Ctrl+G / Ctrl+Q.
    Quit,
    Esc,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Default)]
pub struct Decoder {
    /// Bytes held from the last read: an incomplete UTF-8 character or
    /// escape sequence.
    pending: Vec<u8>,
    /// Inside a bracketed paste: what was pasted so far.
    paste: Option<Vec<u8>>,
}

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        let alone = self.pending.is_empty() && self.paste.is_none() && is_enter(bytes);
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut text = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if let Some(paste) = self.paste.as_mut() {
                match find(&buf[i..], PASTE_END) {
                    Some(end) => {
                        paste.extend_from_slice(&buf[i..i + end]);
                        let pasted = self.paste.take().unwrap_or_default();
                        keys.push(Key::Paste(normalize_breaks(&String::from_utf8_lossy(
                            &pasted,
                        ))));
                        i += end + PASTE_END.len();
                    }
                    None => {
                        // Keep a possible start of the end marker for the next read.
                        let keep = partial_suffix(&buf[i..], PASTE_END);
                        paste.extend_from_slice(&buf[i..buf.len() - keep]);
                        self.pending = buf[buf.len() - keep..].to_vec();
                        i = buf.len();
                    }
                }
                continue;
            }
            let b = buf[i];
            let single = match b {
                b'\r' | b'\n' => {
                    // \r\n is one line break.
                    if b == b'\r' && buf.get(i + 1) == Some(&b'\n') {
                        i += 1;
                    }
                    Some(Key::Enter { alone })
                }
                0x7f | 0x08 => Some(Key::Backspace),
                0x01 => Some(Key::Home),
                0x05 => Some(Key::End),
                0x02 => Some(Key::Left),
                0x06 => Some(Key::Right),
                0x15 => Some(Key::KillBefore),
                0x0b => Some(Key::KillAfter),
                0x17 => Some(Key::KillWord),
                0x10 => Some(Key::Prev),
                0x0e => Some(Key::Next),
                0x1d => Some(Key::Pick),
                0x12 => Some(Key::Reload),
                0x0f => Some(Key::Answers),
                0x07 | 0x11 => Some(Key::Quit),
                b'\t' => Some(Key::Text("\t".into())),
                0x1b => None,
                c if c < 0x20 => {
                    i += 1;
                    continue; // other control keys do nothing
                }
                _ => None,
            };
            if let Some(key) = single {
                flush_text(&mut text, &mut keys);
                keys.push(key);
                i += 1;
                continue;
            }
            if b == 0x1b {
                flush_text(&mut text, &mut keys);
                match escape(&buf[i..]) {
                    Escape::Incomplete => {
                        self.pending = buf[i..].to_vec();
                        break;
                    }
                    Escape::PasteStart => {
                        self.paste = Some(Vec::new());
                        i += PASTE_START.len();
                    }
                    Escape::Key(len, key) => {
                        if let Some(key) = key {
                            keys.push(key);
                        }
                        i += len;
                    }
                }
                continue;
            }
            // UTF-8 text.
            let len = utf8_len(b);
            if i + len > buf.len() {
                self.pending = buf[i..].to_vec();
                break;
            }
            text.extend_from_slice(&buf[i..i + len]);
            i += len;
        }
        flush_text(&mut text, &mut keys);
        // A line break inside a read with other input is text, not a submit.
        if !alone {
            for key in &mut keys {
                if matches!(key, Key::Enter { .. }) {
                    *key = Key::Enter { alone: false };
                }
            }
        }
        keys
    }

    /// Whether bytes are held waiting for the rest of a sequence.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty() && self.paste.is_none()
    }

    /// Settles held bytes when no more came: a lone Esc is Esc, anything
    /// else is dropped.
    pub fn expire(&mut self) -> Vec<Key> {
        if self.paste.is_some() {
            return Vec::new();
        }
        let held = std::mem::take(&mut self.pending);
        if held == b"\x1b" {
            vec![Key::Esc]
        } else {
            Vec::new()
        }
    }
}

fn is_enter(bytes: &[u8]) -> bool {
    matches!(bytes, b"\r" | b"\n" | b"\r\n")
}

fn flush_text(text: &mut Vec<u8>, keys: &mut Vec<Key>) {
    if !text.is_empty() {
        keys.push(Key::Text(String::from_utf8_lossy(text).into_owned()));
        text.clear();
    }
}

fn normalize_breaks(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

enum Escape {
    Incomplete,
    PasteStart,
    /// A sequence of this length, and the key it is (None: ignored).
    Key(usize, Option<Key>),
}

fn escape(bytes: &[u8]) -> Escape {
    match bytes.get(1) {
        None => Escape::Incomplete,
        Some(b'[') => {
            let Some(end) = bytes.iter().skip(2).position(|b| (0x40..=0x7e).contains(b)) else {
                return Escape::Incomplete;
            };
            let len = end + 3;
            let seq = &bytes[..len];
            if seq == PASTE_START {
                return Escape::PasteStart;
            }
            let key = match seq {
                b"\x1b[A" => Some(Key::Up),
                b"\x1b[B" => Some(Key::Down),
                b"\x1b[C" => Some(Key::Right),
                b"\x1b[D" => Some(Key::Left),
                b"\x1b[H" | b"\x1b[1~" => Some(Key::Home),
                b"\x1b[F" | b"\x1b[4~" => Some(Key::End),
                b"\x1b[3~" => Some(Key::Delete),
                // Alt+Enter as some terminals encode it (CSI u / modifyOtherKeys).
                b"\x1b[13;3u" | b"\x1b[27;3;13~" => Some(Key::AltEnter),
                _ => None,
            };
            Escape::Key(len, key)
        }
        Some(b'O') => match bytes.get(2) {
            None => Escape::Incomplete,
            Some(c) => {
                let key = match c {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    b'H' => Some(Key::Home),
                    b'F' => Some(Key::End),
                    _ => None,
                };
                Escape::Key(3, key)
            }
        },
        Some(b'\r' | b'\n') => Escape::Key(2, Some(Key::AltEnter)),
        Some(0x1b) => Escape::Key(1, Some(Key::Esc)),
        // Alt+key: ignored, whole.
        Some(&c) => Escape::Key(1 + utf8_len(c), None),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// How many bytes at the end of `bytes` could start `marker`.
fn partial_suffix(bytes: &[u8], marker: &[u8]) -> usize {
    (1..marker.len().min(bytes.len() + 1))
        .rev()
        .find(|&n| bytes.ends_with(&marker[..n]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(chunks: &[&[u8]]) -> Vec<Key> {
        let mut d = Decoder::default();
        chunks.iter().flat_map(|c| d.feed(c)).collect()
    }

    #[test]
    fn enter_alone_submits_and_enter_with_text_does_not() {
        assert_eq!(keys(&[b"\r"]), [Key::Enter { alone: true }]);
        assert_eq!(
            keys(&[b"a\r"]),
            [Key::Text("a".into()), Key::Enter { alone: false }]
        );
        assert_eq!(
            keys(&[b"\r\r"]),
            [Key::Enter { alone: false }, Key::Enter { alone: false }]
        );
    }

    #[test]
    fn split_utf8_and_escape_sequences_are_joined() {
        let ja = "日本".as_bytes();
        assert_eq!(keys(&[&ja[..2], &ja[2..]]), [Key::Text("日本".into())]);
        assert_eq!(keys(&[b"\x1b[", b"A"]), [Key::Up]);
    }

    #[test]
    fn paste_end_marker_split_across_reads() {
        assert_eq!(
            keys(&[b"\x1b[200~a\rb\x1b[2", b"01~"]),
            [Key::Paste("a\nb".into())]
        );
    }

    #[test]
    fn lone_escape_waits_then_expires() {
        let mut d = Decoder::default();
        assert!(d.feed(b"\x1b").is_empty());
        assert!(d.has_pending());
        assert_eq!(d.expire(), [Key::Esc]);
    }

    #[test]
    fn alt_keys_and_unknown_sequences_are_dropped_whole() {
        assert_eq!(keys(&[b"\x1bx\x1b[1;5Cy"]), [Key::Text("y".into())]);
        assert_eq!(keys(&[b"\x1b\r"]), [Key::AltEnter]);
    }
}
