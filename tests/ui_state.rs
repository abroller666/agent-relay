//! Task 5: what typing in the popup does, and how popup state is kept.
//! The service is a fake that counts sends; nothing reaches Herdr.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use pane_relay::error::HandoffError;
use pane_relay::handoff::SendOutcome;
use pane_relay::model::{AgentKind, AnswerSnapshot, PaneBinding, ResolvedSession, SessionRef};
use pane_relay::state::{self, PopupState};
use pane_relay::ui::{Flow, Popup, PopupService, Screen, TargetRow};

fn binding(pane: &str, agent: AgentKind, session: &str) -> PaneBinding {
    PaneBinding {
        server_key: "/tmp/herdr.sock".into(),
        pane_id: pane.into(),
        terminal_id: format!("term-{pane}"),
        tab_id: "w1:t1".into(),
        agent,
        session: SessionRef {
            kind: "id".into(),
            value: session.into(),
        },
    }
}

fn source() -> PaneBinding {
    binding(
        "w1:pA",
        AgentKind::Claude,
        "c07c63dd-285d-4412-bb32-5654dd417828",
    )
}

fn answer(id: &str, text: &str) -> AnswerSnapshot {
    AnswerSnapshot {
        session: ResolvedSession {
            binding: source(),
            native_id: "c07c63dd-285d-4412-bb32-5654dd417828".into(),
            transcript_path: PathBuf::from("/fake.jsonl"),
        },
        answer_id: id.into(),
        text: text.into(),
        source_fingerprint: format!("fp-{id}"),
    }
}

struct FakeService {
    answer: RefCell<Result<AnswerSnapshot, HandoffError>>,
    sends: RefCell<Vec<(String, String)>>,
    send_result: RefCell<Result<SendOutcome, HandoffError>>,
    prepares: RefCell<usize>,
}

impl FakeService {
    fn new() -> Self {
        Self {
            answer: RefCell::new(Ok(answer("msg_1", "## 回答\n本文"))),
            sends: RefCell::new(Vec::new()),
            send_result: RefCell::new(Ok(SendOutcome::Accepted)),
            prepares: RefCell::new(0),
        }
    }
    fn sent(&self) -> usize {
        self.sends.borrow().len()
    }
}

impl PopupService for FakeService {
    fn prepare(&self, _source: &PaneBinding) -> Result<AnswerSnapshot, HandoffError> {
        *self.prepares.borrow_mut() += 1;
        self.answer.borrow().clone()
    }
    fn send(
        &self,
        _answer: &AnswerSnapshot,
        target: &PaneBinding,
        instruction: &str,
    ) -> Result<SendOutcome, HandoffError> {
        self.sends
            .borrow_mut()
            .push((target.pane_id.clone(), instruction.to_string()));
        self.send_result.borrow().clone()
    }
    fn targets(&self, _source: &PaneBinding) -> Result<Vec<TargetRow>, HandoffError> {
        let row = |pane: &str, agent, ok: bool| TargetRow {
            pane_id: pane.into(),
            label: format!("title of {}", &pane[3..]),
            agent: "codex".into(),
            status: "idle".into(),
            binding: ok.then(|| binding(pane, agent, "01a118e3-a882-7d10-9675-afffb81169d9")),
            unavailable: (!ok).then(|| "セッション未登録".to_string()),
        };
        Ok(vec![
            row("w1:pB", AgentKind::Codex, true),
            row("w1:pC", AgentKind::Codex, false),
            row("w1:pD", AgentKind::Claude, true),
        ])
    }
}

/// A popup that has loaded the answer and chosen w1:pB as target.
fn editing(svc: &FakeService) -> Popup<'_> {
    let mut p = Popup::new(PopupState::new("op1", Some(source())), svc);
    p.load();
    assert_eq!(p.screen(), Screen::Selecting);
    p.feed(b"1");
    p.feed(b"\r");
    assert_eq!(p.screen(), Screen::Editing);
    p
}

#[test]
fn typing_does_not_send() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"abc");
    let ja = "日本語".as_bytes();
    p.feed(&ja[..4]); // a character split across reads
    p.feed(&ja[4..]);
    p.feed(b"\x1b\r"); // Alt+Enter
    p.feed(b"x");
    assert_eq!(svc.sent(), 0);
    assert_eq!(p.state().instruction, "abc日本語\nx");
}

#[test]
fn enter_sends_once() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"review this");
    assert_eq!(p.feed(b"\r"), Flow::Continue);
    assert_eq!(svc.sent(), 1);
    assert_eq!(
        svc.sends.borrow()[0],
        ("w1:pB".to_string(), "review this".to_string())
    );
    assert_eq!(p.screen(), Screen::Sent);
    // More Enter presses after sending only close the popup.
    assert_eq!(p.feed(b"\r"), Flow::Quit);
    assert_eq!(svc.sent(), 1);
}

#[test]
fn empty_instruction_is_not_sent() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"\r");
    assert_eq!(svc.sent(), 0);
    assert_eq!(p.screen(), Screen::Editing);
}

#[test]
fn delivery_unknown_is_never_resent() {
    let svc = FakeService::new();
    *svc.send_result.borrow_mut() = Ok(SendOutcome::DeliveryUnknown);
    let mut p = editing(&svc);
    p.feed(b"go");
    p.feed(b"\r");
    assert_eq!(p.screen(), Screen::DeliveryUnknown);
    assert_eq!(p.feed(b"\r"), Flow::Quit);
    assert_eq!(svc.sent(), 1);
}

#[test]
fn failed_send_keeps_the_instruction_for_a_manual_retry() {
    let svc = FakeService::new();
    *svc.send_result.borrow_mut() = Err(HandoffError::AgentNotReady("w1:pB is working".into()));
    let mut p = editing(&svc);
    p.feed(b"go");
    p.feed(b"\r");
    assert_eq!(svc.sent(), 1);
    assert_eq!(p.screen(), Screen::Editing);
    assert_eq!(p.state().instruction, "go");
    assert!(p.message().unwrap().contains("受付可能"));
    *svc.send_result.borrow_mut() = Ok(SendOutcome::Accepted);
    p.feed(b"\r");
    assert_eq!(svc.sent(), 2);
    assert_eq!(p.screen(), Screen::Sent);
}

#[test]
fn updated_answer_is_reloaded_and_not_sent_automatically() {
    let svc = FakeService::new();
    *svc.send_result.borrow_mut() = Err(HandoffError::SourceChanged("new".into()));
    let mut p = editing(&svc);
    p.feed(b"go");
    *svc.answer.borrow_mut() = Ok(answer("msg_2", "newer"));
    p.feed(b"\r");
    assert_eq!(svc.sent(), 1);
    assert_eq!(p.screen(), Screen::Editing);
    assert_eq!(p.state().answer.as_ref().unwrap().answer_id, "msg_2");
    assert_eq!(p.state().instruction, "go");
    assert!(p.message().unwrap().contains("回答が更新されました"));
}

#[test]
fn paste_newline_does_not_send() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"\x1b[200~line1\rline2\r\n");
    p.feed(b"line3\r\x1b[201~"); // a paste split across reads
    assert_eq!(svc.sent(), 0);
    assert_eq!(p.state().instruction, "line1\nline2\nline3\n");
    // Without bracketed paste, a line break followed by more text in the
    // same read is part of a paste too.
    p.feed(b"a\rb");
    assert_eq!(svc.sent(), 0);
    assert_eq!(p.state().instruction, "line1\nline2\nline3\na\nb");
}

#[test]
fn picker_preserves_instruction() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"keep me");
    p.feed(b"\x1d"); // Ctrl+]
    assert_eq!(p.screen(), Screen::Selecting);
    p.feed(b"2"); // unavailable: no session
    p.feed(b"\r");
    assert_eq!(p.screen(), Screen::Selecting);
    p.feed(b"3");
    p.feed(b"\r");
    assert_eq!(p.screen(), Screen::Editing);
    assert_eq!(p.state().instruction, "keep me");
    assert_eq!(p.state().target.as_ref().unwrap().pane_id, "w1:pD");
    assert_eq!(svc.sent(), 0);
}

#[test]
fn quit_keys_close_without_sending() {
    for key in [b"\x07", b"\x11"] {
        let svc = FakeService::new();
        let mut p = editing(&svc);
        p.feed(b"go");
        assert_eq!(p.feed(key), Flow::Quit);
        assert_eq!(svc.sent(), 0);
    }
}

#[test]
fn failed_load_keeps_the_instruction_and_can_retry() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"draft");
    *svc.answer.borrow_mut() = Err(HandoffError::CompletionUncertain("x".into()));
    p.feed(b"\x12"); // Ctrl+R: read the answer again
    assert_eq!(p.screen(), Screen::Editing);
    assert!(p.state().answer.is_none());
    assert_eq!(p.state().instruction, "draft");
    p.feed(b"\r");
    assert_eq!(svc.sent(), 0, "no answer, nothing to send");
    *svc.answer.borrow_mut() = Ok(answer("msg_3", "ok"));
    p.feed(b"\x12");
    assert_eq!(p.state().answer.as_ref().unwrap().answer_id, "msg_3");
}

#[test]
fn popup_state_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let states = dir.path().join("popups");
    let a = state::op_key("/tmp/herdr-1.sock", "term-1");
    let b = state::op_key("/tmp/herdr-2.sock", "term-1");
    assert_ne!(a, b);
    assert_ne!(
        a,
        state::op_key("/tmp/herdr-1.sock", "term-1"),
        "a fresh id per launch"
    );

    let mut sa = PopupState::new(&a, Some(source()));
    sa.instruction = "for A".into();
    state::save(&states, &sa).unwrap();
    let sb = PopupState::new(&b, None);
    state::save(&states, &sb).unwrap();

    assert_eq!(
        state::load(&states, &a).unwrap().unwrap().instruction,
        "for A"
    );
    assert_eq!(state::load(&states, &b).unwrap().unwrap().instruction, "");
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&states), 0o700);
    for entry in std::fs::read_dir(&states).unwrap() {
        assert_eq!(mode(&entry.unwrap().path()), 0o600);
    }

    state::clear(&states, &a);
    assert!(state::load(&states, &a).unwrap().is_none());
    assert!(state::load(&states, &b).unwrap().is_some());
    assert!(state::load(&states, "../escape").is_err());
}

#[test]
fn stale_state_files_are_swept() {
    let dir = tempfile::tempdir().unwrap();
    let states = dir.path().join("popups");
    let old = state::op_key("/s", "t");
    let fresh = state::op_key("/s", "t");
    state::save(&states, &PopupState::new(&old, None)).unwrap();
    state::save(&states, &PopupState::new(&fresh, None)).unwrap();
    std::fs::write(states.join("unrelated.txt"), "keep").unwrap();
    let day_ago = SystemTime::now() - Duration::from_secs(25 * 3600);
    let f = std::fs::File::options()
        .write(true)
        .open(state::path(&states, &old).unwrap())
        .unwrap();
    f.set_modified(day_ago).unwrap();
    let u = std::fs::File::options()
        .write(true)
        .open(states.join("unrelated.txt"))
        .unwrap();
    u.set_modified(day_ago).unwrap();

    state::sweep(&states, Duration::from_secs(24 * 3600));
    assert!(state::load(&states, &old).unwrap().is_none());
    assert!(state::load(&states, &fresh).unwrap().is_some());
    assert!(states.join("unrelated.txt").exists());
}

#[test]
fn picker_rows_show_pane_ids() {
    let svc = FakeService::new();
    let mut p = Popup::new(PopupState::new("op1", Some(source())), &svc);
    p.load();
    let screen = p.render(100, 20);
    for pane in ["w1:pB", "w1:pC", "w1:pD"] {
        assert!(
            screen.lines().any(|l| l.contains(pane) && l.contains("codex") || l.contains(pane) && l.contains("claude")),
            "{pane} missing from:\n{screen}"
        );
    }
}
