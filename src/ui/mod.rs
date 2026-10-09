//! The popup: read A's answer, pick B, type the instruction, send once.
//!
//! `Popup` is a state machine fed with raw terminal bytes; it draws to a
//! string and talks to the world only through `PopupService`, so tests can
//! drive it with a fake. Typed text stays in the popup until Enter sends
//! the whole instruction.

pub mod editor;
pub mod keys;
pub mod picker;

use serde::{Deserialize, Serialize};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::adapters::AdapterRegistry;
use crate::codex_daemon::CodexDaemon;
use crate::config::Config;
use crate::error::HandoffError;
use crate::handoff::{HandoffService, SendOutcome};
use crate::herdr::{HerdrApi, PaneSummary};
use crate::model::{AgentKind, AnswerSnapshot, PaneBinding};
use crate::names::summary_name;
use crate::state::PopupState;
use editor::Editor;
use keys::{Decoder, Key};
use picker::{Pick, Picker};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Screen {
    Loading,
    Selecting,
    Editing,
    /// Choosing which of the source's answers to send.
    Answers,
    Sending,
    Sent,
    DeliveryUnknown,
    /// Nothing can be done; any key closes.
    Fatal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

/// A pane of the tab, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRow {
    pub pane_id: String,
    pub label: String,
    /// How the pane is named in headers: its Herdr name, else the agent
    /// and working directory.
    pub name: String,
    /// The workspace the pane is in, by its label.
    pub space: String,
    pub agent: String,
    pub status: String,
    /// Present when the pane can receive the prompt.
    pub binding: Option<PaneBinding>,
    /// Why it cannot, otherwise.
    pub unavailable: Option<String>,
}

pub trait PopupService {
    fn prepare(&self, source: &PaneBinding) -> Result<AnswerSnapshot, HandoffError>;
    fn send(
        &self,
        answer: &AnswerSnapshot,
        target: &PaneBinding,
        instruction: &str,
    ) -> Result<SendOutcome, HandoffError>;
    /// The source's finished answers, newest first.
    fn answers(&self, source: &PaneBinding) -> Result<Vec<AnswerSnapshot>, HandoffError>;
    /// The other panes of the source's tab, in screen order.
    fn targets(&self, source: &PaneBinding) -> Result<Vec<TargetRow>, HandoffError>;
    /// How to name `pane` on screen (see `display_name`), if known.
    fn display_name(&self, _pane: &PaneBinding) -> Option<String> {
        None
    }
    /// Called right before a send, which can take a while.
    fn sending(&self) {}
}

pub struct Popup<'a> {
    state: PopupState,
    svc: &'a dyn PopupService,
    decoder: Decoder,
    editor: Editor,
    picker: Picker,
    rows: Vec<TargetRow>,
    /// The answers offered by Ctrl+O, newest first.
    history: Vec<AnswerSnapshot>,
    answer_picker: Picker,
    /// How the source and the target are named on screen.
    source_display: Option<String>,
    target_display: Option<String>,
    message: Option<Message>,
    /// Input typed while sending is dropped; set when the caller should
    /// also discard what is still buffered in the terminal.
    discard_input: bool,
    /// Set by a send attempt, for the rest of the read to be dropped.
    just_sent: bool,
}

#[derive(Debug, Clone)]
struct Message {
    text: String,
    error: bool,
}

impl<'a> Popup<'a> {
    pub fn new(mut state: PopupState, svc: &'a dyn PopupService) -> Self {
        let editor = Editor::new(&state.instruction, state.cursor);
        let mut message = None;
        if state.source.is_none() {
            state.screen = Screen::Fatal;
            message = Some(Message {
                text: state
                    .source_error
                    .clone()
                    .unwrap_or_else(|| "cannot identify the source pane".into()),
                error: true,
            });
        }
        Self {
            state,
            svc,
            decoder: Decoder::default(),
            editor,
            picker: Picker::default(),
            rows: Vec::new(),
            history: Vec::new(),
            answer_picker: Picker::default(),
            source_display: None,
            target_display: None,
            message,
            discard_input: false,
            just_sent: false,
        }
    }

    pub fn state(&self) -> &PopupState {
        &self.state
    }

    pub fn screen(&self) -> Screen {
        self.state.screen
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_ref().map(|m| m.text.as_str())
    }

    /// Whether input buffered in the terminal should be thrown away (it
    /// was typed while the prompt was being sent).
    pub fn take_discard_input(&mut self) -> bool {
        std::mem::take(&mut self.discard_input)
    }

    pub fn has_pending_input(&self) -> bool {
        self.decoder.has_pending()
    }

    /// Reads the answer (unless already read), then shows the picker or,
    /// with a target already chosen, the editor.
    pub fn load(&mut self) {
        if self.state.screen == Screen::Fatal {
            return;
        }
        if self.state.answer.is_none() {
            self.reload_answer();
        }
        self.name_source();
        if self.state.target.is_some() {
            self.state.screen = Screen::Editing;
        } else {
            self.open_picker();
        }
    }

    /// Takes the result of a `prepare` run elsewhere (the caller may read
    /// the answer on another thread to keep the popup responsive).
    pub fn loaded(&mut self, result: Result<AnswerSnapshot, HandoffError>) {
        self.apply_answer(result);
        self.name_source();
        if self.state.target.is_some() {
            self.state.screen = Screen::Editing;
        } else {
            self.open_picker();
        }
    }

    fn name_source(&mut self) {
        if let Some(source) = &self.state.source {
            self.source_display = self.svc.display_name(source);
        }
    }

    pub fn source(&self) -> Option<&PaneBinding> {
        self.state.source.as_ref()
    }

    fn reload_answer(&mut self) {
        let Some(source) = self.state.source.clone() else {
            return;
        };
        let result = self.svc.prepare(&source);
        self.apply_answer(result);
    }

    fn apply_answer(&mut self, result: Result<AnswerSnapshot, HandoffError>) {
        match result {
            Ok(answer) => {
                self.state.answer = Some(answer);
                self.message = None;
            }
            Err(e) => {
                self.state.answer = None;
                self.message = Some(Message {
                    text: format!("{e} (C-r: read again)"),
                    error: true,
                });
            }
        }
    }

    fn open_picker(&mut self) {
        let Some(source) = self.state.source.clone() else {
            return;
        };
        match self.svc.targets(&source) {
            Ok(rows) => self.rows = rows,
            Err(e) => {
                self.rows.clear();
                self.message = Some(Message {
                    text: e.to_string(),
                    error: true,
                });
            }
        }
        let current = self
            .state
            .target
            .as_ref()
            .and_then(|t| self.rows.iter().position(|r| r.pane_id == t.pane_id));
        let first_ok = self.rows.iter().position(|r| r.binding.is_some());
        self.picker = Picker::at(current.or(first_ok).unwrap_or(0));
        self.state.screen = Screen::Selecting;
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Flow {
        let keys = self.decoder.feed(bytes);
        self.handle_keys(keys)
    }

    /// Settles a lone Esc once no more bytes followed it.
    pub fn expire(&mut self) -> Flow {
        let keys = self.decoder.expire();
        self.handle_keys(keys)
    }

    fn handle_keys(&mut self, keys: Vec<Key>) -> Flow {
        for key in keys {
            match self.state.screen {
                Screen::Sent | Screen::DeliveryUnknown | Screen::Fatal => return Flow::Quit,
                Screen::Loading | Screen::Sending => {
                    if matches!(key, Key::Quit | Key::Esc) {
                        return Flow::Quit;
                    }
                }
                Screen::Selecting => {
                    if self.select(&key) == Flow::Quit {
                        return Flow::Quit;
                    }
                }
                Screen::Answers => {
                    if self.choose(&key) == Flow::Quit {
                        return Flow::Quit;
                    }
                }
                Screen::Editing => {
                    // An accepted send closes the popup at once.
                    if self.edit(key) == Flow::Quit || self.state.screen == Screen::Sent {
                        return Flow::Quit;
                    }
                    // After a send attempt, the rest of this read was typed
                    // while sending: drop it.
                    if std::mem::take(&mut self.just_sent) || self.state.screen != Screen::Editing {
                        break;
                    }
                }
            }
        }
        self.state.instruction = self.editor.text();
        self.state.cursor = self.editor.cursor();
        Flow::Continue
    }

    fn open_answers(&mut self) {
        let Some(source) = self.state.source.clone() else {
            return;
        };
        match self.svc.answers(&source) {
            Ok(list) if !list.is_empty() => {
                let current = self
                    .state
                    .answer
                    .as_ref()
                    .and_then(|a| list.iter().position(|h| h.answer_id == a.answer_id));
                self.history = list;
                self.answer_picker = Picker::at(current.unwrap_or(0));
                self.state.screen = Screen::Answers;
                self.message = None;
            }
            Ok(_) => {
                self.message = Some(Message {
                    text: "no finished answers yet".into(),
                    error: true,
                });
            }
            Err(e) => {
                self.message = Some(Message {
                    text: e.to_string(),
                    error: true,
                });
            }
        }
    }

    fn choose(&mut self, key: &Key) -> Flow {
        if *key == Key::Answers {
            self.state.screen = Screen::Editing;
            return Flow::Continue;
        }
        match self.answer_picker.handle(key, self.history.len()) {
            Pick::Stay => {}
            Pick::Quit => return Flow::Quit,
            Pick::Back => self.state.screen = Screen::Editing,
            Pick::Confirm(i) => {
                let mut answer = self.history[i].clone();
                answer.chosen = true;
                self.state.answer = Some(answer);
                self.state.screen = Screen::Editing;
                self.message = None;
            }
        }
        Flow::Continue
    }

    fn select(&mut self, key: &Key) -> Flow {
        match self.picker.handle(key, self.rows.len()) {
            Pick::Stay => {}
            Pick::Quit => return Flow::Quit,
            Pick::Back => {
                if self.state.target.is_some() {
                    self.state.screen = Screen::Editing;
                    self.message = None;
                }
            }
            Pick::Confirm(i) => {
                let row = &self.rows[i];
                match &row.binding {
                    Some(binding) => {
                        self.state.target = Some(binding.clone());
                        self.target_display = Some(row.name.clone());
                        self.state.screen = Screen::Editing;
                        if self.state.answer.is_some() {
                            self.message = None;
                        }
                    }
                    None => {
                        self.message = Some(Message {
                            text: format!(
                                "cannot send to {}: {}",
                                row.name,
                                row.unavailable.as_deref().unwrap_or("not a target")
                            ),
                            error: true,
                        });
                    }
                }
            }
        }
        Flow::Continue
    }

    fn edit(&mut self, key: Key) -> Flow {
        let e = &mut self.editor;
        match key {
            Key::Text(t) | Key::Paste(t) => e.insert(&t),
            Key::Enter { alone: false } | Key::AltEnter => e.insert("\n"),
            Key::Enter { alone: true } => self.send(),
            Key::Backspace => e.backspace(),
            Key::Delete => e.delete(),
            Key::Left => e.left(),
            Key::Right => e.right(),
            Key::Up | Key::Prev => e.up(),
            Key::Down | Key::Next => e.down(),
            Key::Home => e.home(),
            Key::End => e.end(),
            Key::KillBefore => e.kill_before(),
            Key::KillAfter => e.kill_after(),
            Key::KillWord => e.kill_word(),
            Key::Pick => {
                self.state.instruction = self.editor.text();
                self.open_picker();
            }
            Key::Reload => self.reload_answer(),
            Key::Answers => self.open_answers(),
            Key::Quit => return Flow::Quit,
            Key::Esc => return Flow::Quit,
        }
        Flow::Continue
    }

    fn send(&mut self) {
        let instruction = self.editor.text();
        let (Some(answer), Some(target)) = (self.state.answer.clone(), self.state.target.clone())
        else {
            self.message = Some(Message {
                text: "no answer to send (C-r: read again)".into(),
                error: true,
            });
            return;
        };
        self.state.screen = Screen::Sending;
        self.discard_input = true;
        self.just_sent = true;
        self.svc.sending();
        match self.svc.send(&answer, &target, &instruction) {
            Ok(SendOutcome::Accepted) => self.state.screen = Screen::Sent,
            Ok(SendOutcome::DeliveryUnknown) => self.state.screen = Screen::DeliveryUnknown,
            Err(HandoffError::SourceChanged(_)) => {
                self.state.screen = Screen::Editing;
                self.reload_answer();
                if self.state.answer.is_some() {
                    let text = if answer.chosen {
                        "The chosen answer is no longer in the conversation; showing the latest. Review it, then press Enter to send"
                    } else {
                        "The answer was updated. Review the new answer, then press Enter to send"
                    };
                    self.message = Some(Message {
                        text: text.into(),
                        error: false,
                    });
                }
            }
            Err(e) => {
                self.state.screen = Screen::Editing;
                self.message = Some(Message {
                    text: e.to_string(),
                    error: true,
                });
            }
        }
    }

    /// The whole popup, drawn for a `cols` × `rows` terminal.
    pub fn render(&mut self, cols: usize, rows: usize) -> String {
        let cols = cols.max(20);
        let rows = rows.max(4);
        // Autowrap off: long lines are clipped instead of scrolling.
        let mut out = String::from("\x1b[?7l\x1b[H\x1b[2J");
        let mut cursor: Option<(usize, usize)> = None;
        let mut lines: Vec<String> = Vec::new();
        match self.state.screen {
            Screen::Loading => {
                lines.push(format!(
                    "{}  reading the answer…  {}",
                    bold("agent-relay"),
                    dim("Esc/C-g: cancel")
                ));
                lines.push(dim(&format!("from {}", self.source_name())));
            }
            Screen::Fatal => {
                lines.push(red(&format!(
                    "agent-relay: {}",
                    self.message().unwrap_or("")
                )));
                lines.push(dim("press any key to close"));
            }
            Screen::Selecting => self.draw_picker(&mut lines, cols, rows),
            Screen::Answers => self.draw_answers(&mut lines, cols, rows),
            Screen::Editing | Screen::Sending => {
                cursor = self.draw_editor(&mut lines, cols, rows);
            }
            Screen::Sent => {
                lines.push(green(&format!("Sent → {}", self.target_name())));
                lines.push(dim(
                    "This only means the target accepted the prompt; check its pane for the result.",
                ));
                lines.push(dim("press any key to close"));
            }
            Screen::DeliveryUnknown => {
                lines.push(yellow(&format!(
                    "Could not confirm delivery → {}",
                    self.target_name()
                )));
                lines.push(
                    "Check the target pane. Nothing is resent automatically, to avoid sending twice."
                        .into(),
                );
                lines.push(dim("press any key to close"));
            }
        }
        for (i, line) in lines.iter().take(rows).enumerate() {
            if i > 0 {
                out.push_str("\r\n");
            }
            out.push_str(line);
        }
        match cursor {
            Some((r, c)) => out.push_str(&format!("\x1b[{};{}H\x1b[?25h", r + 1, c + 1)),
            None => out.push_str("\x1b[?25l"),
        }
        out
    }

    fn source_name(&self) -> String {
        match (&self.source_display, &self.state.source) {
            (Some(name), _) => name.clone(),
            (None, Some(b)) => b.agent.display_name().into(),
            (None, None) => "?".into(),
        }
    }

    fn target_name(&self) -> String {
        match (&self.target_display, &self.state.target) {
            (Some(name), Some(_)) => name.clone(),
            (_, Some(b)) => b.agent.display_name().into(),
            (_, None) => "(none)".into(),
        }
    }

    fn draw_answers(&mut self, lines: &mut Vec<String>, cols: usize, rows: usize) {
        lines.push(format!(
            "{}  {}",
            bold(&format!(
                "Choose the answer (from {})",
                fit_name(
                    &self.source_name(),
                    cols.saturating_sub("Choose the answer (from )".width())
                )
            )),
            dim("↑↓/jk: move  1-9/␣: pick  ⏎: choose  C-o: back  Esc/C-g: quit")
        ));
        let space = rows.saturating_sub(1).max(1);
        let shown = self.answer_picker.window(self.history.len(), space);
        for i in shown {
            let a = &self.history[i];
            let number = if i < 9 {
                (i + 1).to_string()
            } else {
                " ".into()
            };
            let mark = if i == self.answer_picker.cursor {
                "●"
            } else {
                "○"
            };
            let when = a
                .finished_at
                .as_deref()
                .and_then(local_time)
                .unwrap_or_else(|| " ".repeat(11));
            let first = a
                .text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or_default();
            let tail = format!(
                "  {}{}",
                size(a.text.len()),
                if i == 0 { "  (latest)" } else { "" }
            );
            let head = format!("{mark} {number} {when}  ");
            let room = cols.saturating_sub(head.width() + tail.width());
            let text = format!("{head}{}{tail}", fit(&sanitize(first), room));
            lines.push(if i == self.answer_picker.cursor {
                format!("\x1b[7m{text}\x1b[0m")
            } else {
                text
            });
        }
    }

    fn draw_picker(&mut self, lines: &mut Vec<String>, cols: usize, rows: usize) {
        lines.push(format!(
            "{}  {}",
            bold(&format!(
                "Choose the target (from {})",
                fit_name(
                    &self.source_name(),
                    cols.saturating_sub("Choose the target (from )".width())
                )
            )),
            dim("↑↓/jk: move  1-9/␣: pick  ⏎: choose  Esc/C-g: quit")
        ));
        let space = rows.saturating_sub(2).max(1);
        if self.rows.is_empty() {
            lines.push(dim("no other panes run an AI agent"));
        }
        let many_spaces = self.rows.iter().any(|r| r.space != self.rows[0].space);
        let shown = self.picker.window(self.rows.len(), space);
        for i in shown {
            let row = &self.rows[i];
            let number = if i < 9 {
                (i + 1).to_string()
            } else {
                " ".into()
            };
            let mark = if i == self.picker.cursor {
                "●"
            } else {
                "○"
            };
            let label = fit(&row.label, 16);
            let pad = " ".repeat(16usize.saturating_sub(label.width()));
            let reason = row
                .unavailable
                .as_deref()
                .map(|r| format!("  ✕ {r}"))
                .unwrap_or_default();
            // The workspace column only when the list spans several.
            let space = if many_spaces {
                let s = fit(&row.space, 12);
                format!("{s}{}  ", " ".repeat(12usize.saturating_sub(s.width())))
            } else {
                String::new()
            };
            let text = format!(
                "{mark} {number} {space}{label}{pad}  {:<8} {:<7}{reason}",
                row.agent, row.status
            );
            let text = fit(&text, cols);
            lines.push(match (i == self.picker.cursor, row.binding.is_some()) {
                (true, _) => format!("\x1b[7m{text}\x1b[0m"),
                (false, true) => text,
                (false, false) => dim(&text),
            });
        }
        if let Some(m) = &self.message {
            lines.truncate(rows.saturating_sub(1));
            lines.push(red(&fit(&m.text, cols)));
        }
    }

    fn draw_editor(
        &mut self,
        lines: &mut Vec<String>,
        cols: usize,
        rows: usize,
    ) -> Option<(usize, usize)> {
        let (status, styled) = match &self.state.answer {
            Some(a) => {
                // A hand-picked answer shows when it was written.
                let when = match a.finished_at.as_deref().and_then(local_time) {
                    Some(t) if a.chosen => format!("{t} "),
                    _ => String::new(),
                };
                let s = format!("answer {when}{} ✓", size(a.text.len()));
                (s.clone(), green(&s))
            }
            None => ("no answer".to_string(), red("no answer")),
        };
        // Long names are cut at the front, so the ends of paths, the target
        // and the answer state stay on screen.
        let fixed = " → ".width() + 2 + status.width();
        let (from, to) = share(
            &self.source_name(),
            &self.target_name(),
            cols.saturating_sub(fixed),
        );
        lines.push(format!("{} → {}  {styled}", bold(&from), bold(&to)));
        // Footer: the message, or the keys.
        let footer = match (&self.message, self.state.screen) {
            (_, Screen::Sending) => yellow("sending…"),
            (Some(m), _) if m.error => red(&fit(&m.text, cols)),
            (Some(m), _) => yellow(&fit(&m.text, cols)),
            (None, _) => dim(&fit(
                "⏎: send  M-⏎: newline  C-o: answers  C-]: target  C-r: reload  Esc/C-g: quit",
                cols,
            )),
        };
        let body = rows.saturating_sub(3); // header, separator, footer
        let preview_rows = match &self.state.answer {
            Some(_) => (body / 3).clamp(1, 6),
            None => 0,
        };
        if let Some(answer) = &self.state.answer {
            let mut shown: Vec<&str> = answer.text.lines().collect();
            let more = shown.len() > preview_rows;
            shown.truncate(preview_rows);
            for (i, l) in shown.iter().enumerate() {
                let mut l = format!("│ {}", sanitize(l));
                if more && i + 1 == preview_rows {
                    l.push_str(" …");
                }
                lines.push(dim(&fit(&l, cols)));
            }
        }
        lines.push(dim(&"─".repeat(cols)));
        let edit_rows = body.saturating_sub(preview_rows).max(1);
        const PROMPT: &str = "› ";
        let (text_rows, (cur_row, cur_col)) = self.editor.layout(cols.saturating_sub(3));
        let first = cur_row.saturating_sub(edit_rows - 1);
        let top = lines.len();
        for (i, r) in text_rows.iter().enumerate().skip(first).take(edit_rows) {
            let lead = if i == 0 { PROMPT } else { "  " };
            lines.push(format!("{lead}{r}"));
        }
        if self.editor.text().is_empty() {
            lines[top] = format!("{PROMPT}{}", dim("instruction for the target"));
        }
        while lines.len() < top + edit_rows {
            lines.push(String::new());
        }
        lines.push(footer);
        Some((top + cur_row - first, PROMPT.width() + cur_col))
    }
}

/// An RFC 3339 UTC timestamp (as transcripts write them) in local time,
/// "MM-DD HH:MM".
fn local_time(rfc3339: &str) -> Option<String> {
    let secs = unix_seconds(rfc3339)?;
    // time_t is 64-bit here but not on every platform.
    #[allow(clippy::useless_conversion)]
    let t: nix::libc::time_t = secs.try_into().ok()?;
    let mut tm: nix::libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only reads `t` and writes the tm we pass.
    if unsafe { nix::libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(format!(
        "{:02}-{:02} {:02}:{:02}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    ))
}

/// Seconds since the epoch of "YYYY-MM-DDTHH:MM:SS[.fff]Z".
fn unix_seconds(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    if s.get(4..5) != Some("-") || s.get(10..11) != Some("T") || !s.ends_with('Z') {
        return None;
    }
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else {
        format!("{:.1}KiB", bytes as f64 / 1024.0)
    }
}

/// Text safe to print: control characters shown as `�`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\t' => ' ',
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect()
}

/// `s` cut to `width` columns, ending in "…" when cut.
fn fit(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// `s` cut to `width` columns from the front, starting with "…" when cut.
fn fit_left(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        used += w;
        kept.push(c);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

/// A name cut to `width` columns. In "Agent path" names the agent stays
/// and the path is cut at the front; other names are cut at the front.
fn fit_name(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if let Some(at) = s.find(" /").or_else(|| s.find(" ~")) {
        let (agent, path) = (&s[..=at], &s[at + 1..]);
        // Keep at least a few columns of the path.
        if agent.width() + 6 <= width {
            return format!("{agent}{}", fit_left(path, width - agent.width()));
        }
        // No room for the path: the agent alone says more than a path tail.
        if agent.trim_end().width() <= width {
            return agent.trim_end().to_string();
        }
    }
    fit_left(s, width)
}

/// `a` and `b` cut from the front to share `width` columns: each gets half,
/// and one that needs less leaves the rest to the other.
fn share(a: &str, b: &str, width: usize) -> (String, String) {
    let (wa, wb) = (a.width(), b.width());
    if wa + wb <= width {
        return (a.to_string(), b.to_string());
    }
    let half = width / 2;
    let (ka, kb) = if wa <= half {
        (wa, width - wa)
    } else if wb <= half {
        (width - wb, wb)
    } else {
        (half, width - half)
    };
    (fit_name(a, ka), fit_name(b, kb))
}

fn bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[0m")
}
fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}
fn red(s: &str) -> String {
    format!("\x1b[31m{s}\x1b[0m")
}
fn green(s: &str) -> String {
    format!("\x1b[32m{s}\x1b[0m")
}
fn yellow(s: &str) -> String {
    format!("\x1b[33m{s}\x1b[0m")
}

/// The service the real popup uses: Herdr plus the agent adapters.
pub struct LiveService<'a> {
    pub herdr: &'a dyn HerdrApi,
    pub adapters: &'a dyn AdapterRegistry,
    pub config: &'a Config,
    /// For Codex panes on the shared app-server daemon.
    pub daemon: Option<&'a dyn CodexDaemon>,
}

impl LiveService<'_> {
    fn handoff(&self) -> HandoffService<'_> {
        let service = HandoffService::new(self.herdr, self.adapters, self.config);
        match self.daemon {
            Some(daemon) => service.with_codex_daemon(daemon),
            None => service,
        }
    }
}

impl PopupService for LiveService<'_> {
    fn prepare(&self, source: &PaneBinding) -> Result<AnswerSnapshot, HandoffError> {
        self.handoff().prepare(source.clone())
    }

    fn answers(&self, source: &PaneBinding) -> Result<Vec<AnswerSnapshot>, HandoffError> {
        self.handoff().answers(source.clone())
    }

    fn send(
        &self,
        answer: &AnswerSnapshot,
        target: &PaneBinding,
        instruction: &str,
    ) -> Result<SendOutcome, HandoffError> {
        self.handoff().send(answer, target, instruction)
    }

    fn sending(&self) {
        // Bottom row: the footer of the editor.
        let mut out = std::io::stdout().lock();
        let _ = std::io::Write::write_all(
            &mut out,
            "\x1b[999;1H\x1b[2K\x1b[33msending…\x1b[0m".as_bytes(),
        );
        let _ = std::io::Write::flush(&mut out);
    }

    fn display_name(&self, pane: &PaneBinding) -> Option<String> {
        let panes = self.herdr.list_panes().ok()?;
        let p = panes.into_iter().find(|p| p.pane_id == pane.pane_id)?;
        Some(summary_name(&p))
    }

    fn targets(&self, source: &PaneBinding) -> Result<Vec<TargetRow>, HandoffError> {
        let panes = self.herdr.list_panes()?;
        let places = self
            .herdr
            .layout(&source.pane_id)
            .map(|l| l.panes)
            .unwrap_or_default();
        let spaces = self.herdr.workspace_labels().unwrap_or_default();
        let source_space = panes
            .iter()
            .find(|p| p.pane_id == source.pane_id)
            .map(|p| p.workspace_id.clone())
            .unwrap_or_default();
        // Every pane running an agent but the source, in every workspace.
        let mut panes: Vec<(usize, &PaneSummary)> = panes
            .iter()
            .filter(|p| p.pane_id != source.pane_id)
            .filter(|p| p.agent.as_deref().is_some_and(|a| !a.trim().is_empty()))
            .enumerate()
            .collect();
        let place = |p: &PaneSummary| {
            places
                .iter()
                .find(|pl| pl.pane_id == p.pane_id)
                .map(|pl| (pl.rect.y, pl.rect.x))
        };
        let space_order = |p: &PaneSummary| {
            spaces
                .iter()
                .position(|(id, _)| *id == p.workspace_id)
                .unwrap_or(usize::MAX)
        };
        // The source's tab first, in reading order on screen; then the rest
        // of its workspace; then the other workspaces in Herdr's order.
        panes.sort_by_key(|(i, p)| {
            let group = if p.tab_id == source.tab_id {
                0
            } else if p.workspace_id == source_space {
                1
            } else {
                2
            };
            let (y, x) = place(p).unwrap_or((u32::MAX, u32::MAX));
            (group, if group == 2 { space_order(p) } else { 0 }, y, x, *i)
        });
        Ok(panes
            .into_iter()
            .map(|(_, p)| {
                let mut row = self.row(p);
                row.space = spaces
                    .iter()
                    .find(|(id, _)| *id == p.workspace_id)
                    .map_or_else(|| p.workspace_id.clone(), |(_, label)| label.clone());
                row
            })
            .collect())
    }
}

impl LiveService<'_> {
    fn row(&self, p: &PaneSummary) -> TargetRow {
        let agent = p.agent.clone().unwrap_or_default();
        let mut row = TargetRow {
            pane_id: p.pane_id.clone(),
            label: pane_label(p),
            name: summary_name(p),
            space: String::new(),
            agent: agent.clone(),
            status: p.agent_status.clone().unwrap_or_default(),
            binding: None,
            unavailable: None,
        };
        let reason = if AgentKind::from_herdr(&agent).is_none() {
            Some("unsupported agent".to_string())
        } else {
            match self.handoff().target(&p.pane_id) {
                Ok(a) => {
                    row.status = a.agent_status.clone();
                    if a.is_ready() {
                        row.binding = Some(a.binding);
                        None
                    } else {
                        Some(match a.agent_status.as_str() {
                            "working" => "working".to_string(),
                            "blocked" => "waiting for approval".to_string(),
                            _ if a.launch_pending => "starting".to_string(),
                            _ => "unknown state".to_string(),
                        })
                    }
                }
                Err(e) => Some(unavailable_reason(&e)),
            }
        };
        row.unavailable = reason;
        row
    }
}

/// Why a pane cannot be picked as the target, short enough for its row.
pub fn unavailable_reason(e: &HandoffError) -> String {
    match e {
        HandoffError::SessionUnavailable(_) => "session unknown (send it one prompt first)".into(),
        HandoffError::ThreadNameShared(_) => "same name as another Codex: /rename one".into(),
        e => e.to_string(),
    }
}

/// The pane's name, else its terminal title, else its working directory.
fn pane_label(p: &PaneSummary) -> String {
    [&p.label, &p.terminal_title_stripped]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .map_or_else(|| summary_name(p), str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_cuts_by_columns() {
        assert_eq!(fit("abcdef", 6), "abcdef");
        assert_eq!(fit("abcdefg", 6), "abcde…");
        assert_eq!(fit("日本語です", 6), "日本…");
    }

    #[test]
    fn transcript_timestamps_parse() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            unix_seconds("2026-10-08T00:25:27.875Z"),
            Some(1_791_419_127)
        );
        assert_eq!(
            unix_seconds("2024-02-29T12:00:00.000Z"),
            Some(1_709_208_000)
        );
        assert_eq!(unix_seconds("garbage"), None);
    }

    #[test]
    fn names_are_cut_at_the_front() {
        assert_eq!(fit_left("~/dev/app", 9), "~/dev/app");
        assert_eq!(fit_left("~/dev/app", 6), "…v/app");
        assert_eq!(share("aaaa", "bb", 6), ("aaaa".into(), "bb".into()));
        assert_eq!(share("aaaaaaaa", "bb", 6), ("…aaa".into(), "bb".into()));
        assert_eq!(share("aaaaaa", "bbbbbb", 6), ("…aa".into(), "…bb".into()));
        assert_eq!(fit_name("Codex ~/dev/long/app", 14), "Codex …ong/app");
        assert_eq!(fit_name("Codex /a/b", 4), "…a/b");
        assert_eq!(fit_name("Codex ~/dev/long/app", 9), "Codex");
    }

    #[test]
    fn preview_text_cannot_drive_the_terminal() {
        assert_eq!(sanitize("a\x1b[2Jb\tc"), "a\u{fffd}[2Jb c");
    }
}
