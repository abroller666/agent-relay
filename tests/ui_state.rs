//! Task 5: what typing in the popup does, and how popup state is kept.
//! The service is a fake that counts sends; nothing reaches Herdr.

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use agent_relay::error::HandoffError;
use agent_relay::handoff::SendOutcome;
use agent_relay::model::{AgentKind, AnswerSnapshot, PaneBinding, ResolvedSession, SessionRef};
use agent_relay::state::{self, PopupState};
use agent_relay::ui::{Flow, Popup, PopupService, Screen, TargetRow};

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
        finished_at: None,
        chosen: false,
    }
}

struct FakeService {
    answer: RefCell<Result<AnswerSnapshot, HandoffError>>,
    sends: RefCell<Vec<(String, String)>>,
    /// The answer id of each send, and whether it was picked by hand.
    sent_answers: RefCell<Vec<(String, bool)>>,
    send_result: RefCell<Result<SendOutcome, HandoffError>>,
    prepares: RefCell<usize>,
    source_name: RefCell<Option<String>>,
}

impl FakeService {
    fn new() -> Self {
        Self {
            answer: RefCell::new(Ok(answer("msg_1", "## 回答\n本文"))),
            sends: RefCell::new(Vec::new()),
            sent_answers: RefCell::new(Vec::new()),
            send_result: RefCell::new(Ok(SendOutcome::Accepted)),
            prepares: RefCell::new(0),
            source_name: RefCell::new(None),
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
        answer: &AnswerSnapshot,
        target: &PaneBinding,
        instruction: &str,
    ) -> Result<SendOutcome, HandoffError> {
        self.sent_answers
            .borrow_mut()
            .push((answer.answer_id.clone(), answer.chosen));
        self.sends
            .borrow_mut()
            .push((target.pane_id.clone(), instruction.to_string()));
        self.send_result.borrow().clone()
    }
    fn answers(&self, _source: &PaneBinding) -> Result<Vec<AnswerSnapshot>, HandoffError> {
        let latest = self.answer.borrow().clone()?;
        let mut older = answer("msg_0", "an older answer\nsecond line");
        older.finished_at = Some("2026-10-08T00:25:27.875Z".into());
        Ok(vec![latest, older])
    }
    fn display_name(&self, _pane: &PaneBinding) -> Option<String> {
        self.source_name.borrow().clone()
    }
    fn targets(&self, _source: &PaneBinding) -> Result<Vec<TargetRow>, HandoffError> {
        let row = |pane: &str, agent, ok: bool| TargetRow {
            pane_id: pane.into(),
            label: format!("title of {}", &pane[3..]),
            name: format!("Codex ~/dir-{}", &pane[3..]),
            space: "develop".into(),
            agent: "codex".into(),
            status: "idle".into(),
            binding: ok.then(|| binding(pane, agent, "01a118e3-a882-7d10-9675-afffb81169d9")),
            unavailable: (!ok).then(|| "no session yet".to_string()),
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
    // An accepted send closes the popup at once.
    assert_eq!(p.feed(b"\r"), Flow::Quit);
    assert_eq!(svc.sent(), 1);
    assert_eq!(
        svc.sends.borrow()[0],
        ("w1:pB".to_string(), "review this".to_string())
    );
    assert_eq!(p.screen(), Screen::Sent);
    // Enter pressed again in the same read is not a second send.
    let mut p = editing(&svc);
    assert_eq!(p.feed(b"\r\r"), Flow::Continue); // a line break, not a send
    assert_eq!(svc.sent(), 1);
}

#[test]
fn empty_instruction_sends_the_answer_alone() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"\r");
    assert_eq!(svc.sent(), 1);
    assert_eq!(svc.sends.borrow()[0].1, "");
    assert_eq!(p.screen(), Screen::Sent);
}

#[test]
fn delivery_unknown_is_never_resent() {
    let svc = FakeService::new();
    *svc.send_result.borrow_mut() = Ok(SendOutcome::DeliveryUnknown);
    let mut p = editing(&svc);
    p.feed(b"go");
    // An unconfirmed send stays on screen for its warning.
    assert_eq!(p.feed(b"\r"), Flow::Continue);
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
    assert!(p.message().unwrap().contains("not ready"));
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
    assert!(p.message().unwrap().contains("answer was updated"));
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
fn esc_closes_from_the_editor_and_the_picker() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"go");
    // A lone Esc is settled once no more bytes follow it.
    assert_eq!(p.feed(b"\x1b"), Flow::Continue);
    assert_eq!(p.expire(), Flow::Quit);
    assert_eq!(svc.sent(), 0);

    let mut p = editing(&svc);
    p.feed(b"\x1d");
    assert_eq!(p.screen(), Screen::Selecting);
    p.feed(b"\x1b");
    assert_eq!(p.expire(), Flow::Quit);

    // Escape sequences (arrows, Alt+key) are not Esc.
    let mut p = editing(&svc);
    assert_eq!(p.feed(b"\x1b[D"), Flow::Continue);
    assert_eq!(p.feed(b"\x1bx"), Flow::Continue);
    assert_eq!(svc.sent(), 0);
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
fn screens_do_not_show_pane_ids() {
    let svc = FakeService::new();
    *svc.source_name.borrow_mut() = Some("Claude Code ~/work".into());
    let mut p = Popup::new(PopupState::new("op1", Some(source())), &svc);
    let mut screens = vec![p.render(120, 20)];
    p.load();
    screens.push(p.render(120, 20));
    p.feed(b"2");
    p.feed(b"\r"); // refused, with a message naming the pane
    screens.push(p.render(120, 20));
    p.feed(b"1");
    p.feed(b"\r");
    let editor = p.render(120, 20);
    assert!(
        plain(&editor).contains("Claude Code ~/work → Codex ~/dir-pB"),
        "{editor}"
    );
    screens.push(editor);
    p.feed(b"go");
    p.feed(b"\r");
    screens.push(p.render(120, 20));
    for screen in screens {
        assert!(!screen.contains("w1:p"), "{screen}");
    }
}

fn has_japanese(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\u{3040}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}'))
}

#[test]
fn menus_are_english() {
    let svc = FakeService::new();
    *svc.answer.borrow_mut() = Ok(answer("msg_1", "plain answer"));
    let mut p = Popup::new(PopupState::new("op1", Some(source())), &svc);
    let mut screens = vec![p.render(100, 20)];
    p.load();
    screens.push(p.render(100, 20)); // picker
    p.feed(b"2");
    p.feed(b"\r"); // refused: no session
    screens.push(p.render(100, 20));
    p.feed(b"1");
    p.feed(b"\r");
    screens.push(p.render(100, 20)); // editor, empty
    p.feed(b"\x0f");
    screens.push(p.render(100, 20)); // answers
    p.feed(b"\x0f");
    *svc.answer.borrow_mut() = Err(HandoffError::CompletionUncertain("x".into()));
    p.feed(b"\x12");
    screens.push(p.render(100, 20)); // answer missing
    *svc.answer.borrow_mut() = Ok(answer("msg_2", "plain answer"));
    p.feed(b"\x12");
    p.feed(b"go");
    *svc.send_result.borrow_mut() = Ok(SendOutcome::DeliveryUnknown);
    p.feed(b"\r");
    screens.push(p.render(100, 20));
    let mut fatal = Popup::new(PopupState::new("op2", None), &svc);
    screens.push(fatal.render(100, 20));
    for screen in screens {
        assert!(!has_japanese(&screen), "{screen}");
    }
}

#[test]
fn error_messages_are_english() {
    let errors = [
        HandoffError::UnsupportedAgent(String::new()),
        HandoffError::SessionUnavailable(String::new()),
        HandoffError::SessionAmbiguous(String::new()),
        HandoffError::TranscriptUnavailable(String::new()),
        HandoffError::UnsupportedTranscript(String::new()),
        HandoffError::CompletionUncertain(String::new()),
        HandoffError::NoCompletedAnswer(String::new()),
        HandoffError::TranscriptCorrupt(String::new()),
        HandoffError::ReadLimitExceeded(String::new()),
        HandoffError::SourceChanged(String::new()),
        HandoffError::TargetChanged(String::new()),
        HandoffError::AgentNotReady(String::new()),
        HandoffError::PayloadTooLarge(String::new()),
        HandoffError::InvalidInstruction(String::new()),
        HandoffError::DeliveryUnknown(String::new()),
        HandoffError::Herdr(String::new()),
    ];
    for e in errors {
        assert!(!has_japanese(&e.to_string()), "{e}");
    }
}

#[test]
fn picker_names_the_source() {
    let svc = FakeService::new();
    let mut p = Popup::new(PopupState::new("op1", Some(source())), &svc);
    p.load();
    let header = p.render(120, 20);
    assert!(header.contains("(from Claude Code)"), "{header}");

    *svc.source_name.borrow_mut() = Some("api server".into());
    let mut p = Popup::new(PopupState::new("op1", Some(source())), &svc);
    p.load();
    let header = p.render(120, 20);
    assert!(header.contains("(from api server)"), "{header}");
}

/// The visible text of a screen line, without escape sequences.
fn plain(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() || c == '~' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn long_names_keep_both_ends_of_the_header_visible() {
    let svc = FakeService::new();
    *svc.source_name.borrow_mut() =
        Some("Claude Code ~/very/long/path/aaaaaaaaaaaa/bbbbbbbbbbbb/project-a".into());
    let mut p = editing(&svc);
    let screen = p.render(70, 20);
    let header = plain(screen.lines().next().unwrap());
    let width = unicode_width::UnicodeWidthStr::width(header.as_str());
    assert!(width <= 70, "{width}: {header}");
    // The agent stays; the path is cut at the front.
    assert!(header.contains("Claude Code …"), "{header}");
    assert!(header.contains("project-a"), "{header}");
    assert!(header.contains("→ Codex ~/dir-pB"), "{header}");
    assert!(header.contains("answer"), "{header}");

    p.feed(b"\x1d");
    let screen = p.render(50, 20);
    let header = plain(screen.lines().next().unwrap());
    assert!(header.contains("(from Claude Code …"), "{header}");
    assert!(header.contains("project-a)"), "{header}");
}

mod live_targets {
    use super::*;
    use agent_relay::adapters::DefaultAdapters;
    use agent_relay::config::Config;
    use agent_relay::herdr::{HerdrApi, Layout, PaneSummary};
    use agent_relay::ui::LiveService;
    use serde_json::{Value, json};

    /// Panes in two tabs of workspace w1 and in workspace w2.
    struct Herdr;

    fn pane(id: &str, tab: &str, agent: Option<&str>) -> PaneSummary {
        PaneSummary {
            pane_id: id.into(),
            tab_id: tab.into(),
            workspace_id: tab.split(':').next().unwrap().into(),
            agent: agent.map(str::to_string),
            agent_status: Some("idle".into()),
            cwd: Some(format!("/srv/{}", &id[3..])),
            ..Default::default()
        }
    }

    impl HerdrApi for Herdr {
        fn server_key(&self) -> String {
            "/tmp/herdr.sock".into()
        }
        fn agent_info(&self, pane_id: &str) -> Result<Value, HandoffError> {
            let tab = if pane_id == "w2:pD" { "w2:t1" } else { "w1:t1" };
            Ok(json!({
                "agent": "codex", "agent_status": "idle", "pane_id": pane_id,
                "tab_id": tab, "terminal_id": format!("term-{pane_id}"),
                "agent_session": {"source": "herdr:codex", "agent": "codex", "kind": "id",
                                  "value": "01a118e3-a882-7d10-9675-afffb81169d9"},
            }))
        }
        fn prompt(&self, _: &str, _: &str) -> Result<(), HandoffError> {
            unreachable!()
        }
        fn list_panes(&self) -> Result<Vec<PaneSummary>, HandoffError> {
            Ok(vec![
                pane("w2:pD", "w2:t1", Some("codex")),
                pane("w1:pC", "w1:t2", Some("codex")),
                pane("w1:pA", "w1:t1", Some("claude")),
                pane("w1:pB", "w1:t1", Some("codex")),
                pane("w1:pS", "w1:t1", None), // a shell, no agent
            ])
        }
        fn layout(&self, _: &str) -> Result<Layout, HandoffError> {
            Err(HandoffError::Herdr("no layout".into()))
        }
        fn workspace_labels(&self) -> Result<Vec<(String, String)>, HandoffError> {
            Ok(vec![
                ("w1".into(), "develop".into()),
                ("w2".into(), "review".into()),
            ])
        }
    }

    #[test]
    fn every_other_pane_of_every_workspace_is_a_target() {
        let config = Config::defaults(std::path::Path::new("/home/user"));
        let svc = LiveService {
            herdr: &Herdr,
            adapters: &DefaultAdapters,
            config: &config,
        };
        let rows = svc.targets(&source()).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.pane_id.as_str()).collect();
        // The source's tab first, then its workspace, then the others;
        // panes without an agent are left out.
        assert_eq!(ids, ["w1:pB", "w1:pC", "w2:pD"]);
        assert_eq!(rows[2].space, "review");
        assert!(rows.iter().all(|r| r.binding.is_some()));

        let mut p = Popup::new(PopupState::new("op1", Some(source())), &svc);
        p.load();
        let screen = p.render(120, 20);
        assert!(screen.contains("review"), "{screen}");
        assert!(screen.contains("develop"), "{screen}");
    }
}

#[test]
fn an_older_answer_can_be_chosen_and_sent() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"keep");
    p.feed(b"\x0f"); // Ctrl+O: the answers
    assert_eq!(p.screen(), Screen::Answers);
    let list = plain(&p.render(100, 20));
    assert!(list.contains("an older answer"), "{list}");
    assert!(!list.contains("second line"), "{list}");
    p.feed(b"2");
    p.feed(b"\r");
    assert_eq!(p.screen(), Screen::Editing);
    assert_eq!(p.state().instruction, "keep");
    let chosen = p.state().answer.as_ref().unwrap();
    assert_eq!(chosen.answer_id, "msg_0");
    assert!(chosen.chosen);
    assert_eq!(svc.sent(), 0);
    assert_eq!(p.feed(b"\r"), Flow::Quit);
    assert_eq!(svc.sent_answers.borrow()[0], ("msg_0".to_string(), true));
}

#[test]
fn the_answer_list_can_be_left_or_closed() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"\x0f");
    p.feed(b"2");
    p.feed(b"\x0f"); // back without choosing
    assert_eq!(p.screen(), Screen::Editing);
    assert_eq!(p.state().answer.as_ref().unwrap().answer_id, "msg_1");
    assert!(!p.state().answer.as_ref().unwrap().chosen);
    p.feed(b"\x0f");
    p.feed(b"\x1b");
    assert_eq!(p.expire(), Flow::Quit);
    assert_eq!(svc.sent(), 0);
}

#[test]
fn reload_goes_back_to_the_latest_answer() {
    let svc = FakeService::new();
    let mut p = editing(&svc);
    p.feed(b"\x0f");
    p.feed(b"2");
    p.feed(b"\r");
    p.feed(b"\x12"); // Ctrl+R
    let a = p.state().answer.as_ref().unwrap();
    assert_eq!(a.answer_id, "msg_1");
    assert!(!a.chosen);
}
