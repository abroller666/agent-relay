//! Task 4: checks before sending, the quoted prompt, and sending exactly
//! once. Herdr and the adapters are fakes; nothing reaches a real pane.

use std::cell::RefCell;
use std::path::PathBuf;

use agent_relay::adapters::{AdapterRegistry, AnswerAdapter};
use agent_relay::config::{Config, ReadLimits};
use agent_relay::error::HandoffError;
use agent_relay::handoff::{HandoffService, SendOutcome};
use agent_relay::herdr::{HerdrApi, Layout, PaneSummary};
use agent_relay::model::{AgentKind, AnswerSnapshot, PaneBinding, ResolvedSession};
use agent_relay::prompt::build_prompt;
use agent_relay::session::binding_from_agent;
use serde_json::{Value, json};

const SRC_SESSION: &str = "c07c63dd-285d-4412-bb32-5654dd417828";
const DST_SESSION: &str = "01a118e3-a882-7d10-9675-afffb81169d9";

fn info(pane: &str, agent: &str, session: &str, status: &str) -> Value {
    json!({
        "agent": agent, "agent_status": status, "pane_id": pane, "tab_id": "w1:t1",
        "workspace_id": "w1", "terminal_id": format!("term-{pane}"), "focused": false,
        "revision": 1, "state_change_seq": 10,
        "agent_session": {"source": format!("herdr:{agent}"), "agent": agent,
                          "kind": "id", "value": session},
    })
}

struct FakeHerdr {
    agents: RefCell<Vec<Value>>,
    prompts: RefCell<Vec<(String, String)>>,
    prompt_result: RefCell<Result<(), HandoffError>>,
}

impl FakeHerdr {
    fn new() -> Self {
        Self {
            agents: RefCell::new(vec![
                info("w1:pA", "claude", SRC_SESSION, "idle"),
                info("w1:pB", "codex", DST_SESSION, "done"),
            ]),
            prompts: RefCell::new(Vec::new()),
            prompt_result: RefCell::new(Ok(())),
        }
    }

    fn set(&self, pane: &str, f: impl FnOnce(&mut Value)) {
        let mut agents = self.agents.borrow_mut();
        f(agents.iter_mut().find(|a| a["pane_id"] == pane).unwrap());
    }

    fn binding(&self, pane: &str) -> PaneBinding {
        binding_from_agent(&self.agent_info(pane).unwrap(), &self.server_key()).unwrap()
    }

    fn sent(&self) -> usize {
        self.prompts.borrow().len()
    }
}

impl HerdrApi for FakeHerdr {
    fn server_key(&self) -> String {
        "/tmp/herdr.sock".into()
    }
    fn agent_info(&self, pane_id: &str) -> Result<Value, HandoffError> {
        self.agents
            .borrow()
            .iter()
            .find(|a| a["pane_id"] == pane_id)
            .cloned()
            .ok_or_else(|| HandoffError::UnsupportedAgent(pane_id.into()))
    }
    fn prompt(&self, pane_id: &str, text: &str) -> Result<(), HandoffError> {
        self.prompts
            .borrow_mut()
            .push((pane_id.to_string(), text.to_string()));
        self.prompt_result.borrow().clone()
    }
    fn list_panes(&self) -> Result<Vec<PaneSummary>, HandoffError> {
        Ok(vec![PaneSummary {
            pane_id: "w1:pA".into(),
            tab_id: "w1:t1".into(),
            agent: Some("claude".into()),
            cwd: Some("/srv/app".into()),
            ..Default::default()
        }])
    }
    fn layout(&self, _pane_id: &str) -> Result<Layout, HandoffError> {
        Err(HandoffError::Herdr("no layout".into()))
    }
    fn foreground_processes(
        &self,
        pane_id: &str,
    ) -> Result<Vec<agent_relay::herdr::PaneProcess>, HandoffError> {
        let agent = self.agent_info(pane_id)?["agent"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        Ok(vec![agent_relay::herdr::PaneProcess {
            pid: 7,
            argv: vec![agent],
        }])
    }
}

/// An adapter whose "transcript" is whatever the test puts in `answer`.
struct FakeAdapter {
    answer: RefCell<Result<(String, String), HandoffError>>,
    /// The transcript `resolve` finds (a Codex rewind starts a new file).
    path: RefCell<PathBuf>,
    /// Older answers, newest first, below the latest one.
    older: RefCell<Vec<(String, String)>>,
}

fn snap(session: &ResolvedSession, id: &str, text: &str) -> AnswerSnapshot {
    AnswerSnapshot {
        session: session.clone(),
        source_fingerprint: format!("fp-{id}"),
        answer_id: id.into(),
        text: text.into(),
        finished_at: None,
        chosen: false,
    }
}

impl AnswerAdapter for FakeAdapter {
    fn resolve(&self, binding: &PaneBinding, _: &Config) -> Result<ResolvedSession, HandoffError> {
        Ok(ResolvedSession {
            binding: binding.clone(),
            native_id: binding.session.value.clone(),
            transcript_path: self.path.borrow().clone(),
        })
    }
    fn latest_completed(
        &self,
        session: &ResolvedSession,
        _: &ReadLimits,
    ) -> Result<AnswerSnapshot, HandoffError> {
        let (id, text) = self.answer.borrow().clone()?;
        Ok(snap(session, &id, &text))
    }
    fn completed_answers(
        &self,
        session: &ResolvedSession,
        _: &ReadLimits,
        max: usize,
    ) -> Result<Vec<AnswerSnapshot>, HandoffError> {
        let latest = self.answer.borrow().clone().ok();
        Ok(latest
            .into_iter()
            .chain(self.older.borrow().iter().cloned())
            .map(|(id, text)| snap(session, &id, &text))
            .take(max)
            .collect())
    }
}

struct FakeAdapters(FakeAdapter);

impl AdapterRegistry for FakeAdapters {
    fn adapter(&self, _: AgentKind) -> Option<&dyn AnswerAdapter> {
        Some(&self.0)
    }
}

struct World {
    herdr: FakeHerdr,
    adapters: FakeAdapters,
    config: Config,
}

impl World {
    fn new() -> Self {
        Self {
            herdr: FakeHerdr::new(),
            adapters: FakeAdapters(FakeAdapter {
                answer: RefCell::new(Ok((
                    "msg_1".into(),
                    "## 回答\n\n```rust\nfn main() {}\n```\n日本語".into(),
                ))),
                path: RefCell::new(PathBuf::from("/fake.jsonl")),
                older: RefCell::new(vec![("msg_0".into(), "an older answer".into())]),
            }),
            config: Config::defaults(std::path::Path::new("/home/user")),
        }
    }

    fn service(&self) -> HandoffService<'_> {
        HandoffService::new(&self.herdr, &self.adapters, &self.config)
    }

    fn set_answer(&self, id: &str, text: &str) {
        *self.adapters.0.answer.borrow_mut() = Ok((id.into(), text.into()));
    }
}

#[test]
fn send_once() {
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    let target = w.herdr.binding("w1:pB");
    let outcome = w
        .service()
        .send(&answer, &target, "これをレビューして")
        .unwrap();
    assert_eq!(outcome, SendOutcome::Accepted);
    let prompts = w.herdr.prompts.borrow();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].0, "w1:pB");
    assert!(prompts[0].1.starts_with("これをレビューして"));
    assert!(prompts[0].1.contains(&answer.text));
}

#[test]
fn changed_source_blocks_send() {
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    w.set_answer("msg_2", "a newer answer");
    let err = w
        .service()
        .send(&answer, &w.herdr.binding("w1:pB"), "go")
        .unwrap_err();
    assert!(matches!(err, HandoffError::SourceChanged(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 0);
}

#[test]
fn replaced_source_blocks_send() {
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    w.herdr.set("w1:pA", |a| {
        a["agent_session"]["value"] = json!("a5498f9c-a941-4f63-99a7-c81327440d7a")
    });
    let err = w
        .service()
        .send(&answer, &w.herdr.binding("w1:pB"), "go")
        .unwrap_err();
    assert!(matches!(err, HandoffError::SourceChanged(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 0);
}

#[test]
fn replaced_target_blocks_send() {
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    let target = w.herdr.binding("w1:pB");
    // A new process in the same pane.
    w.herdr
        .set("w1:pB", |a| a["terminal_id"] = json!("term-new"));
    let err = w.service().send(&answer, &target, "go").unwrap_err();
    assert!(matches!(err, HandoffError::TargetChanged(_)), "{err:?}");
    // A different session in the same terminal.
    w.herdr.set("w1:pB", |a| {
        a["terminal_id"] = json!("term-w1:pB");
        a["agent_session"]["value"] = json!("01a118e9-1aa8-7491-bc3d-f5bfadbddeb7");
    });
    let err = w.service().send(&answer, &target, "go").unwrap_err();
    assert!(matches!(err, HandoffError::TargetChanged(_)), "{err:?}");
    // The pane's agent exited.
    w.herdr
        .agents
        .borrow_mut()
        .retain(|a| a["pane_id"] != "w1:pB");
    let err = w.service().send(&answer, &target, "go").unwrap_err();
    assert!(matches!(err, HandoffError::TargetChanged(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 0);
}

#[test]
fn working_or_blocked_rejected() {
    for status in ["working", "blocked", "unknown"] {
        let w = World::new();
        let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
        let target = w.herdr.binding("w1:pB");
        w.herdr.set("w1:pB", |a| a["agent_status"] = json!(status));
        let err = w.service().send(&answer, &target, "go").unwrap_err();
        assert!(
            matches!(err, HandoffError::AgentNotReady(_)),
            "{status}: {err:?}"
        );

        w.herdr.set("w1:pB", |a| a["agent_status"] = json!("idle"));
        w.herdr.set("w1:pA", |a| a["agent_status"] = json!(status));
        let err = w.service().send(&answer, &target, "go").unwrap_err();
        assert!(
            matches!(err, HandoffError::AgentNotReady(_)),
            "{status}: {err:?}"
        );
        assert_eq!(w.herdr.sent(), 0);
    }
}

#[test]
fn source_must_be_ready_to_prepare() {
    let w = World::new();
    w.herdr
        .set("w1:pA", |a| a["agent_status"] = json!("working"));
    let err = w.service().prepare(w.herdr.binding("w1:pA")).unwrap_err();
    assert!(matches!(err, HandoffError::AgentNotReady(_)), "{err:?}");
}

#[test]
fn starting_managed_target_rejected() {
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    let target = w.herdr.binding("w1:pB");
    w.herdr.set("w1:pB", |a| a["launch_pending"] = json!(true));
    let err = w.service().send(&answer, &target, "go").unwrap_err();
    assert!(matches!(err, HandoffError::AgentNotReady(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 0);
}

#[test]
fn same_pane_rejected() {
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    let err = w
        .service()
        .send(&answer, &w.herdr.binding("w1:pA"), "go")
        .unwrap_err();
    assert!(matches!(err, HandoffError::TargetChanged(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 0);
}

#[test]
fn oversize_rejected() {
    let mut w = World::new();
    w.config.max_payload_bytes = 200;
    w.set_answer("msg_1", &"長".repeat(100));
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    let err = w
        .service()
        .send(&answer, &w.herdr.binding("w1:pB"), "go")
        .unwrap_err();
    assert!(matches!(err, HandoffError::PayloadTooLarge(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 0);
}

#[test]
fn timeout_never_retries() {
    let w = World::new();
    *w.herdr.prompt_result.borrow_mut() = Err(HandoffError::DeliveryUnknown("timed out".into()));
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    let outcome = w
        .service()
        .send(&answer, &w.herdr.binding("w1:pB"), "go")
        .unwrap();
    assert_eq!(outcome, SendOutcome::DeliveryUnknown);
    assert_eq!(w.herdr.sent(), 1);
}

#[test]
fn rejection_before_writing_is_an_error_not_unknown() {
    let w = World::new();
    *w.herdr.prompt_result.borrow_mut() = Err(HandoffError::AgentNotReady("blocked".into()));
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    let err = w
        .service()
        .send(&answer, &w.herdr.binding("w1:pB"), "go")
        .unwrap_err();
    assert!(matches!(err, HandoffError::AgentNotReady(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 1);
}

#[test]
fn unreadable_source_blocks_send() {
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    *w.adapters.0.answer.borrow_mut() = Err(HandoffError::CompletionUncertain("x".into()));
    let err = w
        .service()
        .send(&answer, &w.herdr.binding("w1:pB"), "go")
        .unwrap_err();
    assert!(
        matches!(err, HandoffError::CompletionUncertain(_)),
        "{err:?}"
    );
    assert_eq!(w.herdr.sent(), 0);
}

fn snapshot(text: &str) -> AnswerSnapshot {
    let w = World::new();
    w.set_answer("msg_1", text);
    w.service().prepare(w.herdr.binding("w1:pA")).unwrap()
}

#[test]
fn prompt_puts_instruction_first_and_quotes_the_answer_verbatim() {
    let answer = snapshot("line 1\n=====\n```\ncode\n```\n");
    let prompt = build_prompt("要約して", &answer, "Claude Code ~/work").unwrap();
    assert!(prompt.starts_with("要約して\n"));
    assert!(
        prompt.contains("以下は別のAIの回答を引用した参考資料です（送信元：Claude Code ~/work）")
    );
    assert!(prompt.contains(&answer.text));
    // The fence around the quote does not occur in the quote.
    let fence = prompt
        .lines()
        .find(|l| l.starts_with("=====") && !answer.text.lines().any(|a| a == *l))
        .expect("a fence line");
    assert_eq!(prompt.matches(fence).count(), 2);
    assert!(!answer.text.contains(fence));
}

#[test]
fn prompt_refuses_terminal_control_characters() {
    for text in ["a\x1b[201~b", "a\x07b", "a\u{9b}b"] {
        let answer = snapshot(text);
        assert!(build_prompt("go", &answer, "x").is_err(), "{text:?}");
    }
    let answer = snapshot("tab\there\r\nnext");
    assert!(build_prompt("go", &answer, "x").is_ok());
    assert!(build_prompt("bad\x1b", &snapshot("ok"), "x").is_err());
}

#[test]
fn source_transcript_switch_blocks_send() {
    // After a Codex rewind, A's history continues in a new file; the old
    // file still holds the rewound answer.
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    *w.adapters.0.path.borrow_mut() = PathBuf::from("/fake_segment.jsonl");
    let err = w
        .service()
        .send(&answer, &w.herdr.binding("w1:pB"), "go")
        .unwrap_err();
    assert!(matches!(err, HandoffError::SourceChanged(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 0);
}

#[test]
fn prompt_label_adds_the_agent_to_a_pane_name() {
    let answer = snapshot("text");
    let prompt = build_prompt("go", &answer, "api server").unwrap();
    assert!(
        prompt.contains("（送信元：api server（Claude Code））"),
        "{prompt}"
    );
}

#[test]
fn sent_prompt_names_the_source_without_pane_ids() {
    let w = World::new();
    let answer = w.service().prepare(w.herdr.binding("w1:pA")).unwrap();
    w.service()
        .send(&answer, &w.herdr.binding("w1:pB"), "go")
        .unwrap();
    let prompts = w.herdr.prompts.borrow();
    assert!(
        prompts[0].1.contains("（送信元：Claude Code /srv/app）"),
        "{}",
        prompts[0].1
    );
    assert!(!prompts[0].1.contains("w1:p"), "{}", prompts[0].1);
}

#[test]
fn empty_instruction_sends_label_and_answer() {
    let answer = snapshot("the answer");
    let prompt = build_prompt("  \n", &answer, "Claude Code ~/work").unwrap();
    assert!(
        prompt.starts_with("以下は別のAIの回答を引用した参考資料です"),
        "{prompt}"
    );
    assert!(prompt.contains("the answer"));
}

#[test]
fn answers_lists_the_history_of_a_ready_source() {
    let w = World::new();
    let answers = w.service().answers(w.herdr.binding("w1:pA")).unwrap();
    let ids: Vec<&str> = answers.iter().map(|a| a.answer_id.as_str()).collect();
    assert_eq!(ids, ["msg_1", "msg_0"]);
    w.herdr
        .set("w1:pA", |a| a["agent_status"] = json!("working"));
    let err = w.service().answers(w.herdr.binding("w1:pA")).unwrap_err();
    assert!(matches!(err, HandoffError::AgentNotReady(_)), "{err:?}");
}

#[test]
fn chosen_older_answer_is_sent_after_a_newer_one_appears() {
    let w = World::new();
    let mut older = w
        .service()
        .answers(w.herdr.binding("w1:pA"))
        .unwrap()
        .remove(1);
    older.chosen = true;
    // A newer answer arrives; the chosen one is still in the conversation.
    w.set_answer("msg_2", "the newest");
    w.adapters
        .0
        .older
        .borrow_mut()
        .insert(0, ("msg_1".into(), "previous".into()));
    let outcome = w
        .service()
        .send(&older, &w.herdr.binding("w1:pB"), "go")
        .unwrap();
    assert_eq!(outcome, SendOutcome::Accepted);
    assert!(w.herdr.prompts.borrow()[0].1.contains("an older answer"));
}

#[test]
fn chosen_answer_no_longer_in_the_conversation_blocks_send() {
    let w = World::new();
    let mut older = w
        .service()
        .answers(w.herdr.binding("w1:pA"))
        .unwrap()
        .remove(1);
    older.chosen = true;
    w.adapters.0.older.borrow_mut().clear(); // rewound away
    let err = w
        .service()
        .send(&older, &w.herdr.binding("w1:pB"), "go")
        .unwrap_err();
    assert!(matches!(err, HandoffError::SourceChanged(_)), "{err:?}");
    assert_eq!(w.herdr.sent(), 0);
}

struct OneThreadDaemon;

impl agent_relay::codex_daemon::CodexDaemon for OneThreadDaemon {
    fn loaded_threads(
        &self,
    ) -> Result<Vec<agent_relay::codex_daemon::DaemonThread>, agent_relay::codex_daemon::DaemonError>
    {
        Ok(vec![agent_relay::codex_daemon::DaemonThread {
            id: DST_SESSION.into(),
            name: Some("Review".into()),
            cwd: Some("/home/user/app".into()),
        }])
    }
}

#[test]
fn codex_on_the_shared_daemon_is_reached_through_its_title() {
    let w = World::new();
    // Herdr has no session for the Codex pane; its title names the thread.
    w.herdr.set("w1:pB", |a| {
        a.as_object_mut().unwrap().remove("agent_session");
        a["terminal_title_stripped"] = json!("Review | app");
        a["foreground_cwd"] = json!("/home/user/app");
    });
    let daemon = OneThreadDaemon;
    let service = w.service().with_codex_daemon(&daemon);
    let answer = service.prepare(w.herdr.binding("w1:pA")).unwrap();
    let target = service.agent("w1:pB").unwrap().binding;
    assert_eq!(target.session.value, DST_SESSION);
    assert_eq!(
        service.send(&answer, &target, "go").unwrap(),
        SendOutcome::Accepted
    );
    // Without the daemon the pane cannot be identified.
    let err = w.service().agent("w1:pB").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}
