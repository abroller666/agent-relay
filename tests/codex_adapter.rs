//! Task 3: the Codex adapter returns the final answer of the last finished
//! turn of the active rollout file, or refuses.
//!
//! Fixtures are sanitized rollouts of test sessions run with Codex 0.160.1
//! (see docs/compatibility.md). Line numbers are 1-based lines of those
//! files; synthetic edits are made here, in the open.

use std::fs;
use std::time::Duration;

use agent_relay::adapters::AnswerAdapter;
use agent_relay::adapters::codex::CodexAdapter;
use agent_relay::config::Config;
use agent_relay::error::HandoffError;
use agent_relay::model::{AgentKind, AnswerSnapshot, PaneBinding, SessionRef};
use serde_json::{Value, json};

/// DELTA-FOUR, then a turn with commentary, `pwd` and a final answer.
const PLAIN_ID: &str = "01a118e3-a4ef-7111-920d-88249b09667d";
const PLAIN: &str = "rollout-2026-10-08T09-22-14-01a118e3-a4ef-7111-920d-88249b09667d.jsonl";
/// CHARLIE-THREE, a tool turn, an interrupted turn, JULIET-ONE, KILO-TWO.
const REWOUND_ID: &str = "01a118e3-a882-7d10-9675-afffb81169d9";
const REWOUND: &str = "rollout-2026-10-08T09-22-15-01a118e3-a882-7d10-9675-afffb81169d9.jsonl";
/// After rewinding KILO-TWO away: LIMA-THREE, then (resumed) NOVEMBER-RESUME.
const SEGMENT: &str = "rollout-2026-10-08T09-26-33-01a118e3-a882-7d10-9675-afffb81169d9_01a118e7-96eb-7db2-a456-fd93d22a289f.jsonl";
/// `codex fork` of PLAIN: MIKE-FORK, MULTI-OK3, MULTI-OK1209.
const FORK_ID: &str = "01a118e9-1aa8-7491-bc3d-f5bfadbddeb7";
const FORK: &str = "rollout-2026-10-08T09-28-12-01a118e9-1aa8-7491-bc3d-f5bfadbddeb7.jsonl";

const FINAL_DELTA: &str = "- `pwd` を実行しました。\n- 現在のディレクトリは `/work` です。\n- 確認が完了しました。FINAL-DELTA";

fn raw_lines(name: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/fixtures/raw/codex/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn join(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn edit(line: &str, f: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(line).unwrap();
    f(&mut v);
    v.to_string()
}

struct Setup {
    root: tempfile::TempDir,
    config: Config,
}

impl Setup {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::defaults(root.path());
        config.codex_roots = vec![root.path().join("sessions")];
        config.limits.retry_delays = vec![Duration::from_millis(10); 2];
        Self { root, config }
    }

    /// Writes `content` as rollout `name` on day `day`.
    fn file(self, day: &str, name: &str, content: &str) -> Self {
        let dir = self.root.path().join("sessions/2026/10").join(day);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), content).unwrap();
        self
    }

    /// The first `n` lines (all when None) of fixture `name`.
    fn fixture(self, name: &str, n: Option<usize>) -> Self {
        let lines = raw_lines(name);
        let n = n.unwrap_or(lines.len());
        self.file("08", name, &join(&lines[..n]))
    }

    fn latest(&self, id: &str) -> Result<AnswerSnapshot, HandoffError> {
        let binding = PaneBinding {
            server_key: "srv".into(),
            pane_id: "w4:p3".into(),
            terminal_id: "term_3".into(),
            tab_id: "w4:t1".into(),
            agent: AgentKind::Codex,
            session: SessionRef {
                kind: "id".into(),
                value: id.into(),
            },
        };
        let adapter = CodexAdapter;
        let session = adapter.resolve(&binding, &self.config)?;
        adapter.latest_completed(&session, &self.config.limits)
    }
}

fn assert_err(r: Result<AnswerSnapshot, HandoffError>, want: fn(&HandoffError) -> bool) {
    match r {
        Ok(a) => panic!("expected an error, got answer {:?}", a.text),
        Err(e) => assert!(want(&e), "{e:?}"),
    }
}

#[test]
fn completed_turn_final_only() {
    let s = Setup::new().fixture(PLAIN, None);
    let answer = s.latest(PLAIN_ID).unwrap();
    assert_eq!(answer.text, FINAL_DELTA);
    assert!(answer.answer_id.starts_with("msg_"));
    assert_eq!(answer.session.native_id, PLAIN_ID);

    let first = Setup::new()
        .fixture(PLAIN, Some(17))
        .latest(PLAIN_ID)
        .unwrap();
    assert_eq!(first.text, "DELTA-FOUR");
    assert_ne!(first.source_fingerprint, answer.source_fingerprint);
}

#[test]
fn commentary_excluded() {
    let answer = Setup::new().fixture(PLAIN, None).latest(PLAIN_ID).unwrap();
    assert!(
        !answer
            .text
            .contains("まず現在の作業ディレクトリを確認します")
    );
}

#[test]
fn duplicate_event_not_duplicated() {
    // Synthetic: the final message recorded twice, first with a shorter
    // (superseded) text, as seen with Codex 0.154.0.
    let mut lines = raw_lines(PLAIN);
    let early = edit(&lines[32], |v| {
        v["payload"]["content"] = json!([{"type": "output_text", "text": "- `pwd` を実行"}])
    });
    lines.insert(32, early);
    lines.insert(34, lines[33].clone());
    let s = Setup::new().file("08", PLAIN, &join(&lines));
    assert_eq!(s.latest(PLAIN_ID).unwrap().text, FINAL_DELTA);
}

#[test]
fn missing_phase_requires_completion() {
    // Synthetic: DELTA-FOUR (line 14) without a phase.
    let mut lines = raw_lines(PLAIN)[..17].to_vec();
    lines[13] = edit(&lines[13], |v| {
        v["payload"].as_object_mut().unwrap().remove("phase");
    });
    let done = Setup::new().file("08", PLAIN, &join(&lines));
    assert_eq!(done.latest(PLAIN_ID).unwrap().text, "DELTA-FOUR");

    let unfinished = Setup::new().file("08", PLAIN, &join(&lines[..14]));
    assert_err(unfinished.latest(PLAIN_ID), |e| {
        matches!(e, HandoffError::CompletionUncertain(_))
    });

    lines[16] = edit(&lines[16], |v| {
        v["payload"]["last_agent_message"] = json!("something else")
    });
    let mismatched = Setup::new().file("08", PLAIN, &join(&lines));
    assert_err(mismatched.latest(PLAIN_ID), |e| {
        matches!(e, HandoffError::CompletionUncertain(_))
    });
}

#[test]
fn final_answer_must_match_the_turn_completion() {
    // Plan mode recorded the plan as final_answer but completed the turn
    // with a commentary message; neither is taken.
    let mut lines = raw_lines(PLAIN);
    lines[35] = edit(&lines[35], |v| {
        v["payload"]["last_agent_message"] = json!("まず現在の作業ディレクトリを確認します。\n")
    });
    let s = Setup::new().file("08", PLAIN, &join(&lines));
    assert_err(s.latest(PLAIN_ID), |e| {
        matches!(e, HandoffError::CompletionUncertain(_))
    });
}

#[test]
fn fork_and_rollback_respected() {
    let base_only = Setup::new().fixture(REWOUND, None);
    assert_eq!(base_only.latest(REWOUND_ID).unwrap().text, "KILO-TWO");

    // The segment written by the rewind supersedes the base file.
    let resumed = Setup::new().fixture(REWOUND, None).fixture(SEGMENT, None);
    assert_eq!(resumed.latest(REWOUND_ID).unwrap().text, "NOVEMBER-RESUME");
    let lima = Setup::new()
        .fixture(REWOUND, None)
        .fixture(SEGMENT, Some(13));
    assert_eq!(lima.latest(REWOUND_ID).unwrap().text, "LIMA-THREE");

    // Right after the rewind: KILO-TWO was rewound away and nothing new
    // was said; the rewound answer must not come back.
    let just_rewound = Setup::new()
        .fixture(REWOUND, None)
        .fixture(SEGMENT, Some(3));
    assert_err(just_rewound.latest(REWOUND_ID), |e| {
        matches!(e, HandoffError::NoCompletedAnswer(_))
    });

    // A fork before its first prompt has no answer of its own.
    let fresh_fork = Setup::new().fixture(PLAIN, None).fixture(FORK, Some(2));
    assert_err(fresh_fork.latest(FORK_ID), |e| {
        matches!(e, HandoffError::NoCompletedAnswer(_))
    });
    let fork = Setup::new().fixture(PLAIN, None).fixture(FORK, None);
    assert_eq!(fork.latest(FORK_ID).unwrap().text, "MULTI-OK1209");
    assert_eq!(fork.latest(PLAIN_ID).unwrap().text, FINAL_DELTA);
}

#[test]
fn unverified_rollback_event_is_unsupported() {
    let mut lines = raw_lines(PLAIN);
    lines.insert(
        34,
        json!({"ordinal": 34, "type": "event_msg",
               "payload": {"type": "thread_rolled_back", "num_turns": 1}})
        .to_string(),
    );
    let s = Setup::new().file("08", PLAIN, &join(&lines));
    assert_err(s.latest(PLAIN_ID), |e| {
        matches!(e, HandoffError::UnsupportedTranscript(_))
    });
}

#[test]
fn interrupted_turn_rejected() {
    let s = Setup::new().fixture(REWOUND, Some(42));
    assert_err(s.latest(REWOUND_ID), |e| {
        matches!(e, HandoffError::NoCompletedAnswer(_))
    });
}

#[test]
fn running_turn_is_uncertain() {
    // 36: a turn started; 49: its answer is written, the turn not ended.
    for n in [36, 49] {
        let s = Setup::new().fixture(REWOUND, Some(n));
        assert_err(s.latest(REWOUND_ID), |e| {
            matches!(e, HandoffError::CompletionUncertain(_))
        });
    }
    let lines = raw_lines(REWOUND);
    let mut content = join(&lines[..51]);
    content.push_str(&lines[51][..30]);
    let partial = Setup::new().file("08", REWOUND, &content);
    assert_err(partial.latest(REWOUND_ID), |e| {
        matches!(e, HandoffError::CompletionUncertain(_))
    });
}

#[test]
fn message_of_another_turn_is_unsupported() {
    let mut lines = raw_lines(PLAIN);
    lines[32] = edit(&lines[32], |v| {
        v["payload"]["internal_chat_message_metadata_passthrough"]["turn_id"] = json!("other")
    });
    let s = Setup::new().file("08", PLAIN, &join(&lines));
    assert_err(s.latest(PLAIN_ID), |e| {
        matches!(e, HandoffError::UnsupportedTranscript(_))
    });
}

#[test]
fn rollout_of_another_session_is_refused() {
    // PLAIN's content stored under REWOUND's id.
    let renamed = PLAIN.replace(PLAIN_ID, REWOUND_ID);
    let s = Setup::new().file("08", &renamed, &join(&raw_lines(PLAIN)));
    assert_err(s.latest(REWOUND_ID), |e| {
        matches!(e, HandoffError::UnsupportedTranscript(_))
    });
}

#[test]
fn two_base_files_are_ambiguous() {
    let content = join(&raw_lines(PLAIN));
    let s = Setup::new().file("08", PLAIN, &content).file(
        "09",
        &PLAIN.replace("09-22-14", "10-00-00"),
        &content,
    );
    assert_err(s.latest(PLAIN_ID), |e| {
        matches!(e, HandoffError::SessionAmbiguous(_))
    });
}

#[test]
fn missing_rollout_is_unavailable() {
    let s = Setup::new().fixture(PLAIN, None);
    assert_err(s.latest(FORK_ID), |e| {
        matches!(e, HandoffError::TranscriptUnavailable(_))
    });
}

impl Setup {
    fn history(&self, id: &str) -> Result<Vec<AnswerSnapshot>, HandoffError> {
        let binding = PaneBinding {
            server_key: "srv".into(),
            pane_id: "w4:p3".into(),
            terminal_id: "term_3".into(),
            tab_id: "w4:t1".into(),
            agent: AgentKind::Codex,
            session: SessionRef {
                kind: "id".into(),
                value: id.into(),
            },
        };
        let adapter = CodexAdapter;
        let session = adapter.resolve(&binding, &self.config)?;
        adapter.completed_answers(&session, &self.config.limits, 50)
    }
}

fn texts(answers: &[AnswerSnapshot]) -> Vec<String> {
    answers
        .iter()
        .map(|a| a.text.lines().next().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn history_follows_the_rewind_into_the_base_file() {
    // KILO-TWO (ordinal 53+) was rewound away; the interrupted turn has
    // no answer.
    let s = Setup::new().fixture(REWOUND, None).fixture(SEGMENT, None);
    let all = s.history(REWOUND_ID).unwrap();
    assert_eq!(
        texts(&all),
        [
            "NOVEMBER-RESUME",
            "LIMA-THREE",
            "JULIET-ONE",
            "## 結果",
            "CHARLIE-THREE"
        ]
    );
    let latest = s.latest(REWOUND_ID).unwrap();
    assert_eq!(all[0].answer_id, latest.answer_id);
    assert_eq!(all[0].source_fingerprint, latest.source_fingerprint);
    assert!(all.iter().all(|a| a.finished_at.is_some()));

    // Right after the rewind, before any new prompt.
    let just_rewound = Setup::new()
        .fixture(REWOUND, None)
        .fixture(SEGMENT, Some(3));
    assert_eq!(
        texts(&just_rewound.history(REWOUND_ID).unwrap()),
        ["JULIET-ONE", "## 結果", "CHARLIE-THREE"]
    );
}

#[test]
fn history_of_a_fork_continues_into_its_parent() {
    let s = Setup::new().fixture(PLAIN, None).fixture(FORK, None);
    assert_eq!(
        texts(&s.history(FORK_ID).unwrap()),
        [
            "MULTI-OK1209",
            "MULTI-OK3",
            "MIKE-FORK",
            "- `pwd` を実行しました。",
            "DELTA-FOUR"
        ]
    );
    let fresh = Setup::new().fixture(PLAIN, None).fixture(FORK, Some(2));
    assert_eq!(
        texts(&fresh.history(FORK_ID).unwrap()),
        ["- `pwd` を実行しました。", "DELTA-FOUR"]
    );
}

#[test]
fn history_while_a_turn_runs_lists_the_earlier_answers() {
    let s = Setup::new().fixture(REWOUND, Some(49));
    assert_eq!(
        texts(&s.history(REWOUND_ID).unwrap()),
        ["## 結果", "CHARLIE-THREE"]
    );
}
