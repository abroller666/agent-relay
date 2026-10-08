//! Task 2: the Claude Code adapter returns the final text of the last
//! finished turn on the active branch, or refuses.
//!
//! Fixtures are sanitized transcripts of test sessions run with Claude Code
//! 2.1.293 (see docs/compatibility.md). Line numbers below are 1-based
//! lines of those files; synthetic edits are made here, in the open.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use agent_relay::adapters::AnswerAdapter;
use agent_relay::adapters::claude::ClaudeAdapter;
use agent_relay::config::Config;
use agent_relay::error::HandoffError;
use agent_relay::model::{AgentKind, AnswerSnapshot, PaneBinding, SessionRef};
use serde_json::{Value, json};

/// Short answers, a tool turn, an interrupted turn, then GOLF-ONE,
/// HOTEL-TWO, a rewind to before HOTEL-TWO and INDIA-THREE, then an
/// away_summary written while idle.
const MAIN: &str = "c07c63dd-285d-4412-bb32-5654dd417828";
/// A progress sentence, a Bash call, then the final answer.
const PROGRESS: &str = "a5498f9c-a941-4f63-99a7-c81327440d7a";
/// A fork (`--resume --fork-session`) of PROGRESS.
const FORK: &str = "fc5bdcc2-78cf-4599-b9c3-5fdb87959b2a";

const TOOL_ANSWER: &str = "## 結果\n\nカレントディレクトリにはファイルがありませんでした。Glob ツールはこのセッションで利用できないため、代わりに Bash の `ls` で確認しました。\n\n```\nTOOL-DONE\n```";
const FINAL_BRAVO: &str = "- 作業ディレクトリは `/work` です。\n- `pwd` の出力と一致しており、これが現在のシェルの場所です。\n- 今回の操作は読み取りのみで、ファイルの変更は行っていません。 FINAL-BRAVO";

fn raw_lines(id: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/fixtures/raw/claude/{id}.jsonl",
        env!("CARGO_MANIFEST_DIR")
    );
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

struct Setup {
    _root: tempfile::TempDir,
    config: Config,
    path: PathBuf,
    id: String,
}

impl Setup {
    /// A Claude root holding `content` as the transcript of `id`.
    fn new(id: &str, content: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("projects").join("-work");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, content).unwrap();
        let mut config = Config::defaults(root.path());
        config.claude_roots = vec![root.path().join("projects")];
        config.limits.retry_delays = vec![Duration::from_millis(20); 3];
        Self {
            _root: root,
            config,
            path,
            id: id.to_string(),
        }
    }

    /// The first `n` lines of fixture `id`.
    fn prefix(id: &str, n: usize) -> Self {
        Self::new(id, &join(&raw_lines(id)[..n]))
    }

    fn binding(&self) -> PaneBinding {
        PaneBinding {
            server_key: "srv".into(),
            pane_id: "w4:p1".into(),
            terminal_id: "term_1".into(),
            tab_id: "w4:t1".into(),
            agent: AgentKind::Claude,
            session: SessionRef {
                kind: "id".into(),
                value: self.id.clone(),
            },
        }
    }

    fn latest(&self) -> Result<AnswerSnapshot, HandoffError> {
        let adapter = ClaudeAdapter;
        let session = adapter.resolve(&self.binding(), &self.config)?;
        adapter.latest_completed(&session, &self.config.limits)
    }
}

fn join(lines: &[String]) -> String {
    lines.iter().map(|l| format!("{l}\n")).collect()
}

fn edit(line: &str, f: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(line).unwrap();
    f(&mut v);
    v.to_string()
}

#[test]
fn final_text_only() {
    let s = Setup::prefix(PROGRESS, 45);
    let answer = s.latest().unwrap();
    assert_eq!(answer.text, FINAL_BRAVO);
    assert!(!answer.text.contains("I'll check"));
    assert_eq!(answer.session.transcript_path, s.path);
    assert_eq!(answer.session.native_id, PROGRESS);
}

#[test]
fn tool_and_thinking_excluded() {
    let s = Setup::prefix(MAIN, 50);
    let answer = s.latest().unwrap();
    assert_eq!(answer.text, TOOL_ANSWER);
    assert!(!answer.text.contains("Glob tool isn't available"));
    assert!(answer.answer_id.starts_with("msg_"));
}

#[test]
fn partial_tail_pending() {
    // turn_duration is only half written.
    let lines = raw_lines(MAIN);
    let mut content = join(&lines[..49]);
    content.push_str(&lines[49][..40]);
    let s = Setup::new(MAIN, &content);
    let err = s.latest().unwrap_err();
    assert!(
        matches!(err, HandoffError::CompletionUncertain(_)),
        "{err:?}"
    );
}

#[test]
fn partial_tail_completed_while_waiting() {
    let lines = raw_lines(MAIN);
    let mut content = join(&lines[..49]);
    content.push_str(&lines[49][..40]);
    let mut s = Setup::new(MAIN, &content);
    s.config.limits.retry_delays = vec![Duration::from_millis(100); 5];
    let path = s.path.clone();
    let rest = format!("{}\n", &lines[49][40..]);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        std::io::Write::write_all(&mut f, rest.as_bytes()).unwrap();
    });
    let answer = s.latest().unwrap();
    writer.join().unwrap();
    assert_eq!(answer.text, TOOL_ANSWER);
}

#[test]
fn stale_answer_not_returned() {
    // 48: the answer is written but the turn's end is not yet.
    // 52: a new prompt was submitted. 55: its answer is streaming.
    for n in [48, 52, 55] {
        let err = Setup::prefix(MAIN, n).latest().unwrap_err();
        assert!(
            matches!(err, HandoffError::CompletionUncertain(_)),
            "prefix {n}: {err:?}"
        );
    }
}

#[test]
fn interrupted_turn_is_not_replaced_by_an_older_answer() {
    let err = Setup::prefix(MAIN, 56).latest().unwrap_err();
    assert!(matches!(err, HandoffError::NoCompletedAnswer(_)), "{err:?}");
}

#[test]
fn active_branch_only() {
    // HOTEL-TWO was rewound away; INDIA-THREE continues from GOLF-ONE.
    let all = Setup::prefix(MAIN, 81).latest().unwrap();
    assert_eq!(all.text, "INDIA-THREE");
    let before_rewind = Setup::prefix(MAIN, 74).latest().unwrap();
    assert_eq!(before_rewind.text, "HOTEL-TWO");
    assert_ne!(all.source_fingerprint, before_rewind.source_fingerprint);
}

#[test]
fn records_written_while_idle_keep_the_answer() {
    // Line 81 is an away_summary appended after the turn ended.
    let with = Setup::prefix(MAIN, 81).latest().unwrap();
    let without = Setup::prefix(MAIN, 80).latest().unwrap();
    assert_eq!(with.text, "INDIA-THREE");
    assert_eq!(with.answer_id, without.answer_id);
    assert_eq!(with.source_fingerprint, without.source_fingerprint);
}

#[test]
fn fork_reads_its_own_transcript() {
    let answer = Setup::prefix(FORK, 66).latest().unwrap();
    assert_eq!(answer.text, "MULTI-OK 4784");
}

#[test]
fn text_blocks_of_one_message_are_joined_in_order() {
    // Synthetic: a second text block of the ALPHA-ONE message (line 27).
    let mut lines = raw_lines(MAIN)[..35].to_vec();
    let second = edit(&lines[26], |v| {
        v["uuid"] = json!("aaaaaaaa-0000-0000-0000-000000000001");
        v["message"]["content"] = json!([{"type": "text", "text": "SECOND"}]);
    });
    let real_uuid = serde_json::from_str::<Value>(&lines[26]).unwrap()["uuid"].clone();
    let second = edit(&second, |v| v["parentUuid"] = real_uuid);
    lines[27] = edit(&lines[27], |v| {
        v["parentUuid"] = json!("aaaaaaaa-0000-0000-0000-000000000001")
    });
    lines.insert(27, second);
    let answer = Setup::new(MAIN, &join(&lines)).latest().unwrap();
    assert_eq!(answer.text, "ALPHA-ONE\n\nSECOND");
}

#[test]
fn duplicated_record_is_counted_once() {
    let mut lines = raw_lines(MAIN)[..35].to_vec();
    lines.insert(27, lines[26].clone());
    let answer = Setup::new(MAIN, &join(&lines)).latest().unwrap();
    assert_eq!(answer.text, "ALPHA-ONE");
}

#[test]
fn api_error_is_no_answer() {
    let mut lines = raw_lines(MAIN)[..35].to_vec();
    lines[26] = edit(&lines[26], |v| v["isApiErrorMessage"] = json!(true));
    let err = Setup::new(MAIN, &join(&lines)).latest().unwrap_err();
    assert!(matches!(err, HandoffError::NoCompletedAnswer(_)), "{err:?}");
}

#[test]
fn records_of_another_session_are_refused() {
    let lines = raw_lines(MAIN)[..35].to_vec();
    let err = Setup::new(PROGRESS, &join(&lines)).latest().unwrap_err();
    assert!(
        matches!(err, HandoffError::UnsupportedTranscript(_)),
        "{err:?}"
    );
}

#[test]
fn corrupt_finished_line_is_an_error() {
    let mut lines = raw_lines(MAIN)[..35].to_vec();
    lines[10] = "{not json".into();
    let err = Setup::new(MAIN, &join(&lines)).latest().unwrap_err();
    assert!(matches!(err, HandoffError::TranscriptCorrupt(_)), "{err:?}");
}

#[test]
fn over_long_line_is_a_limit_error() {
    let mut s = Setup::prefix(MAIN, 35);
    s.config.limits.max_line_bytes = 200;
    let err = s.latest().unwrap_err();
    assert!(matches!(err, HandoffError::ReadLimitExceeded(_)), "{err:?}");
}

#[test]
fn over_large_file_is_a_limit_error() {
    let mut s = Setup::prefix(MAIN, 35);
    s.config.limits.max_file_bytes = 1000;
    let err = s.latest().unwrap_err();
    assert!(matches!(err, HandoffError::ReadLimitExceeded(_)), "{err:?}");
}

#[test]
fn empty_session_has_no_answer() {
    let err = Setup::prefix(MAIN, 4).latest().unwrap_err();
    assert!(matches!(err, HandoffError::NoCompletedAnswer(_)), "{err:?}");
}

fn texts(answers: &[AnswerSnapshot]) -> Vec<&str> {
    answers.iter().map(|a| a.text.as_str()).collect()
}

impl Setup {
    fn history(&self) -> Result<Vec<AnswerSnapshot>, HandoffError> {
        let adapter = ClaudeAdapter;
        let session = adapter.resolve(&self.binding(), &self.config)?;
        adapter.completed_answers(&session, &self.config.limits, 50)
    }
}

#[test]
fn history_lists_finished_answers_of_the_active_branch_newest_first() {
    // HOTEL-TWO was rewound away; the interrupted turn has no answer.
    let all = Setup::prefix(MAIN, 81).history().unwrap();
    assert_eq!(
        texts(&all),
        ["INDIA-THREE", "GOLF-ONE", TOOL_ANSWER, "ALPHA-ONE"]
    );
    // The newest entry is the latest answer, with the same identity.
    let latest = Setup::prefix(MAIN, 81).latest().unwrap();
    assert_eq!(all[0].answer_id, latest.answer_id);
    assert_eq!(all[0].source_fingerprint, latest.source_fingerprint);
    assert!(all.iter().all(|a| a.finished_at.is_some()));
}

#[test]
fn history_survives_an_interrupted_latest_turn() {
    let s = Setup::prefix(MAIN, 56);
    assert!(s.latest().is_err());
    assert_eq!(texts(&s.history().unwrap()), [TOOL_ANSWER, "ALPHA-ONE"]);
}

#[test]
fn history_while_a_turn_runs_lists_the_earlier_answers() {
    // 52: a new prompt was submitted and has no answer yet.
    let s = Setup::prefix(MAIN, 52);
    assert_eq!(texts(&s.history().unwrap()), [TOOL_ANSWER, "ALPHA-ONE"]);
}

#[test]
fn history_is_limited() {
    let s = Setup::prefix(MAIN, 81);
    let adapter = ClaudeAdapter;
    let session = adapter.resolve(&s.binding(), &s.config).unwrap();
    let two = adapter
        .completed_answers(&session, &s.config.limits, 2)
        .unwrap();
    assert_eq!(texts(&two), ["INDIA-THREE", "GOLF-ONE"]);
}
