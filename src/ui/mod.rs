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
use crate::config::Config;
use crate::error::HandoffError;
use crate::handoff::{HandoffService, SendOutcome};
use crate::herdr::{HerdrApi, PaneSummary};
use crate::model::{AgentKind, AnswerSnapshot, PaneBinding};
use crate::state::PopupState;
use editor::Editor;
use keys::{Decoder, Key};
use picker::{Pick, Picker};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Screen {
    Loading,
    Selecting,
    Editing,
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
    /// The other panes of the source's tab, in screen order.
    fn targets(&self, source: &PaneBinding) -> Result<Vec<TargetRow>, HandoffError>;
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
                    .unwrap_or_else(|| "送信元のpaneを特定できません".into()),
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
        if self.state.target.is_some() {
            self.state.screen = Screen::Editing;
        } else {
            self.open_picker();
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
                    text: format!("{e}（Ctrl+Rで再取得）"),
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
                    if key == Key::Quit {
                        return Flow::Quit;
                    }
                }
                Screen::Selecting => {
                    if self.select(&key) == Flow::Quit {
                        return Flow::Quit;
                    }
                }
                Screen::Editing => {
                    if self.edit(key) == Flow::Quit {
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
                        self.state.screen = Screen::Editing;
                        if self.state.answer.is_some() {
                            self.message = None;
                        }
                    }
                    None => {
                        self.message = Some(Message {
                            text: format!(
                                "{}には送れません：{}",
                                row.pane_id,
                                row.unavailable.as_deref().unwrap_or("対象外")
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
            Key::Quit => return Flow::Quit,
            Key::Esc => {}
        }
        Flow::Continue
    }

    fn send(&mut self) {
        let instruction = self.editor.text();
        let (Some(answer), Some(target)) = (self.state.answer.clone(), self.state.target.clone())
        else {
            self.message = Some(Message {
                text: "送る回答がありません（Ctrl+Rで再取得）".into(),
                error: true,
            });
            return;
        };
        if instruction.trim().is_empty() {
            self.message = Some(Message {
                text: "指示を入力してください".into(),
                error: true,
            });
            return;
        }
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
                    self.message = Some(Message {
                        text:
                            "回答が更新されました。新しい回答を確認してからEnterで送信してください"
                                .into(),
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
                    "{}  回答を取得しています…  {}",
                    bold("pane-relay"),
                    dim("C-g: 中止")
                ));
                lines.push(dim(&format!("送信元 {}", self.source_name())));
            }
            Screen::Fatal => {
                lines.push(red(&format!(
                    "pane-relay: {}",
                    self.message().unwrap_or("")
                )));
                lines.push(dim("何かキーを押すと閉じます"));
            }
            Screen::Selecting => self.draw_picker(&mut lines, cols, rows),
            Screen::Editing | Screen::Sending => {
                cursor = self.draw_editor(&mut lines, cols, rows);
            }
            Screen::Sent => {
                lines.push(green(&format!("送信しました → {}", self.target_name())));
                lines.push(dim(
                    "相手が受け付けたことだけを示します。処理の完了は相手のpaneで確認してください。",
                ));
                lines.push(dim("何かキーを押すと閉じます"));
            }
            Screen::DeliveryUnknown => {
                lines.push(yellow(&format!(
                    "送信できたか確認できませんでした → {}",
                    self.target_name()
                )));
                lines.push(
                    "相手のpaneを確認してください。二重送信を避けるため、自動では再送しません。"
                        .into(),
                );
                lines.push(dim("何かキーを押すと閉じます"));
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
        self.state
            .source
            .as_ref()
            .map_or_else(|| "?".into(), pane_name)
    }

    fn target_name(&self) -> String {
        self.state
            .target
            .as_ref()
            .map_or_else(|| "（未選択）".into(), pane_name)
    }

    fn draw_picker(&mut self, lines: &mut Vec<String>, cols: usize, rows: usize) {
        lines.push(format!(
            "{}  {}",
            bold(&format!("送り先を選択（送信元 {}）", self.source_name())),
            dim("↑↓/jk: 移動  1-9/␣: 選択  ⏎: 決定  C-g: 終了")
        ));
        let space = rows.saturating_sub(2).max(1);
        if self.rows.is_empty() {
            lines.push(dim("このタブには他のpaneがありません"));
        }
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
            let text = format!(
                "{mark} {number} {label}{pad}  {:<8} {:<8} {:<7}{reason}",
                row.pane_id, row.agent, row.status
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
        let status = match &self.state.answer {
            Some(a) => green(&format!("回答 {} ✓", size(a.text.len()))),
            None => red("回答なし"),
        };
        lines.push(fit_styled(
            &format!(
                "{} {} → {}  {status}",
                bold("pane-relay"),
                self.source_name(),
                self.target_name()
            ),
            cols,
        ));
        // Footer: the message, or the keys.
        let footer = match (&self.message, self.state.screen) {
            (_, Screen::Sending) => yellow("送信中…"),
            (Some(m), _) if m.error => red(&fit(&m.text, cols)),
            (Some(m), _) => yellow(&fit(&m.text, cols)),
            (None, _) => dim(&fit(
                "⏎: 送信  M-⏎: 改行  C-]: 送り先  C-r: 再取得  C-g: 終了",
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
            lines[top] = format!("{PROMPT}{}", dim("Bへの指示を入力"));
        }
        while lines.len() < top + edit_rows {
            lines.push(String::new());
        }
        lines.push(footer);
        Some((top + cur_row - first, PROMPT.width() + cur_col))
    }
}

/// "Claude Code w1:p4E".
fn pane_name(b: &PaneBinding) -> String {
    format!("{} {}", b.agent.display_name(), b.pane_id)
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

/// Styled text is clipped by the terminal (autowrap is off).
fn fit_styled(s: &str, _width: usize) -> String {
    s.to_string()
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
}

impl LiveService<'_> {
    fn handoff(&self) -> HandoffService<'_> {
        HandoffService::new(self.herdr, self.adapters, self.config)
    }
}

impl PopupService for LiveService<'_> {
    fn prepare(&self, source: &PaneBinding) -> Result<AnswerSnapshot, HandoffError> {
        self.handoff().prepare(source.clone())
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
            "\x1b[999;1H\x1b[2K\x1b[33m送信中…\x1b[0m".as_bytes(),
        );
        let _ = std::io::Write::flush(&mut out);
    }

    fn targets(&self, source: &PaneBinding) -> Result<Vec<TargetRow>, HandoffError> {
        let panes = self.herdr.list_panes()?;
        let places = self
            .herdr
            .layout(&source.pane_id)
            .map(|l| l.panes)
            .unwrap_or_default();
        let mut panes: Vec<&PaneSummary> = panes
            .iter()
            .filter(|p| p.tab_id == source.tab_id && p.pane_id != source.pane_id)
            .collect();
        let place = |p: &PaneSummary| {
            places
                .iter()
                .find(|pl| pl.pane_id == p.pane_id)
                .map(|pl| (pl.rect.y, pl.rect.x))
        };
        // Reading order on screen; panes missing from the layout last.
        panes.sort_by_key(|p| place(p).map_or((1, 0, 0), |(y, x)| (0, y, x)));
        Ok(panes.into_iter().map(|p| self.row(p)).collect())
    }
}

impl LiveService<'_> {
    fn row(&self, p: &PaneSummary) -> TargetRow {
        let agent = p.agent.clone().unwrap_or_default();
        let mut row = TargetRow {
            pane_id: p.pane_id.clone(),
            label: pane_label(p),
            agent: agent.clone(),
            status: p.agent_status.clone().unwrap_or_default(),
            binding: None,
            unavailable: None,
        };
        let reason = if agent.is_empty() {
            Some("AIエージェントなし".to_string())
        } else if AgentKind::from_herdr(&agent).is_none() {
            Some("未対応のエージェント".to_string())
        } else {
            match self.herdr.agent(&p.pane_id) {
                Ok(a) => {
                    row.status = a.agent_status.clone();
                    if a.is_ready() {
                        row.binding = Some(a.binding);
                        None
                    } else {
                        Some(match a.agent_status.as_str() {
                            "working" => "処理中".to_string(),
                            "blocked" => "承認待ち".to_string(),
                            _ if a.launch_pending => "起動中".to_string(),
                            _ => "状態不明".to_string(),
                        })
                    }
                }
                Err(HandoffError::SessionUnavailable(_)) => {
                    Some("セッション未登録（一度発言すると登録されます）".to_string())
                }
                Err(e) => Some(e.to_string()),
            }
        };
        row.unavailable = reason;
        row
    }
}

/// The pane's name, else its terminal title, else its id.
fn pane_label(p: &PaneSummary) -> String {
    [&p.label, &p.terminal_title_stripped]
        .into_iter()
        .flatten()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or(&p.pane_id)
        .to_string()
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
    fn preview_text_cannot_drive_the_terminal() {
        assert_eq!(sanitize("a\x1b[2Jb\tc"), "a\u{fffd}[2Jb c");
    }
}
