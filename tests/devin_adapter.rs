//! The Devin CLI adapter returns the final text of the last finished turn
//! of the current conversation, or refuses; and refuses a target that is
//! waiting for approval although Herdr reports it idle.
//!
//! Fixtures are sanitized sessions of a test run with Devin CLI
//! 3000.11.234 (see docs/compatibility.md), loaded here into a database
//! with Devin's schema. Node ids below are those of the fixtures; synthetic
//! edits are made here, in the open.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_relay::adapters::AnswerAdapter;
use agent_relay::adapters::devin::DevinAdapter;
use agent_relay::config::Config;
use agent_relay::error::HandoffError;
use agent_relay::model::{AgentKind, AnswerSnapshot, PaneBinding, SessionRef};
use rusqlite::{Connection, params};
use serde_json::Value;

/// ANSWER-ONE (an `exec` call, then the answer), a prompt interrupted at
/// its tool's approval, three queued prompts answered by "Got it.", a
/// `/revert` to after ANSWER-ONE, a prompt interrupted while the answer
/// streamed, ANSWER-THREE, `/compact`, then a `/btw` side question.
const MAIN: &str = "thundering-myrtle";
/// After `/clear` in the same pane: AFTER-CLEAR, a prompt whose approval
/// was canceled, then ANSWER-FIVE.
const CLEARED: &str = "emerald-wolfsbane";
/// `/fork` of MAIN, made after its compaction.
const FORK: &str = "vine-hygienic";

const ANSWER_ONE: &str = "ANSWER-ONE line1\nline2 日本語";

/// Nodes of MAIN.
const ONE_FIRST: i64 = 26; // ANSWER-ONE, before the revert
const SLEEP_PROMPT: i64 = 27; // the prompt whose tool was canceled
const SLEEP_CALL: i64 = 29; // its pending `exec` call
const SLEEP_CANCELED: i64 = 30; // the canceled tool result
const GOT_IT: i64 = 35; // answer to the queued prompts
const ONE_REVERTED: i64 = 38; // ANSWER-ONE, rewritten by `/revert`
const STREAM_INTERRUPTED: i64 = 75; // `[Response interrupted by user]`
const THREE: i64 = 78; // ANSWER-THREE

/// The tables and columns of Devin's `sessions.db` the adapter reads.
const SCHEMA: &str = "
CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  working_directory TEXT NOT NULL,
  backend_type TEXT NOT NULL,
  model TEXT NOT NULL,
  agent_mode TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  last_activity_at INTEGER NOT NULL, title TEXT, main_chain_id INTEGER,
  shell_last_seen_index INTEGER DEFAULT 0, cogs_json TEXT, workspace_dirs TEXT,
  hidden INTEGER NOT NULL DEFAULT 0, metadata TEXT);
CREATE TABLE message_nodes (
  row_id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id TEXT NOT NULL,
  node_id INTEGER NOT NULL,
  parent_node_id INTEGER,
  chat_message TEXT NOT NULL,
  created_at INTEGER NOT NULL, metadata TEXT,
  FOREIGN KEY (session_id) REFERENCES sessions(id),
  UNIQUE(session_id, node_id)
);
";

fn fixture(id: &str) -> Vec<Value> {
    let path = format!(
        "{}/tests/fixtures/raw/devin/{id}.jsonl",
        env!("CARGO_MANIFEST_DIR")
    );
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Creates `<dir>/sessions.db` holding the fixture sessions `ids`.
fn write_db(dir: &Path, ids: &[&str]) -> Connection {
    fs::create_dir_all(dir).unwrap();
    let conn = Connection::open(dir.join("sessions.db")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    for id in ids {
        let rows = fixture(id);
        let main = &rows[0]["session"];
        conn.execute(
            "INSERT INTO sessions (id, working_directory, backend_type, model, agent_mode,
                                   created_at, last_activity_at, main_chain_id)
             VALUES (?1, '/work', 'Windsurf', 'swe-1-6-slow', 'normal', 0, 0, ?2)",
            params![main["id"].as_str().unwrap(), main["main_chain_id"].as_i64()],
        )
        .unwrap();
        for row in &rows[1..] {
            insert_node(
                &conn,
                id,
                row["node_id"].as_i64().unwrap(),
                row["parent_node_id"].as_i64(),
                &row["chat_message"].to_string(),
                &row["metadata"],
            );
        }
    }
    conn
}

fn insert_node(
    conn: &Connection,
    session: &str,
    node: i64,
    parent: Option<i64>,
    message: &str,
    metadata: &Value,
) {
    let metadata = (!metadata.is_null()).then(|| metadata.to_string());
    conn.execute(
        "INSERT INTO message_nodes (session_id, node_id, parent_node_id, chat_message,
                                    created_at, metadata)
         VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        params![session, node, parent, message, metadata],
    )
    .unwrap();
}

struct Setup {
    _root: tempfile::TempDir,
    config: Config,
    db: Connection,
    path: PathBuf,
}

impl Setup {
    /// A Devin root whose database holds all three fixture sessions.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("devin");
        let db = write_db(&dir, &[MAIN, CLEARED, FORK]);
        let mut config = Config::defaults(root.path());
        config.devin_roots = vec![dir.clone()];
        config.limits.retry_delays = vec![Duration::from_millis(20); 3];
        Self {
            _root: root,
            config,
            db,
            path: dir.join("sessions.db"),
        }
    }

    /// MAIN as it was when its conversation ended at `node`.
    fn at(node: i64) -> Self {
        let s = Self::new();
        s.set_main(MAIN, Some(node));
        s
    }

    fn set_main(&self, id: &str, node: Option<i64>) {
        self.db
            .execute(
                "UPDATE sessions SET main_chain_id = ?2 WHERE id = ?1",
                params![id, node],
            )
            .unwrap();
    }

    fn sql(&self, sql: &str) {
        self.db.execute_batch(sql).unwrap();
    }

    fn binding(id: &str) -> PaneBinding {
        PaneBinding {
            server_key: "srv".into(),
            pane_id: "w2:p1".into(),
            terminal_id: "term_1".into(),
            tab_id: "w2:t1".into(),
            agent: AgentKind::Devin,
            session: SessionRef {
                kind: "id".into(),
                value: id.into(),
            },
        }
    }

    fn latest_of(&self, id: &str) -> Result<AnswerSnapshot, HandoffError> {
        let session = DevinAdapter.resolve(&Self::binding(id), &self.config)?;
        DevinAdapter.latest_completed(&session, &self.config.limits)
    }

    fn latest(&self) -> Result<AnswerSnapshot, HandoffError> {
        self.latest_of(MAIN)
    }

    fn history_of(&self, id: &str) -> Vec<String> {
        let session = DevinAdapter
            .resolve(&Self::binding(id), &self.config)
            .unwrap();
        DevinAdapter
            .completed_answers(&session, &self.config.limits, 50)
            .unwrap()
            .into_iter()
            .map(|a| a.text)
            .collect()
    }

    fn accepts(&self, id: &str) -> Result<(), HandoffError> {
        DevinAdapter.accepts_prompt(&Self::binding(id), &self.config)
    }
}

#[test]
fn after_compaction_the_answer_before_it_is_the_latest() {
    let s = Setup::new();
    let answer = s.latest().unwrap();
    assert_eq!(answer.text, "ANSWER-THREE");
    assert_eq!(answer.session.transcript_path, s.path);
    assert_eq!(answer.session.native_id, MAIN);
    assert!(answer.finished_at.is_some());
}

#[test]
fn history_follows_the_current_branch_through_the_compaction() {
    let s = Setup::new();
    // "Got it." was on the branch `/revert` left; the interrupted stream
    // and the side question are not answers of the conversation.
    assert_eq!(s.history_of(MAIN), ["ANSWER-THREE", ANSWER_ONE]);
}

#[test]
fn final_text_only() {
    // ANSWER-ONE follows an `exec` call and its output.
    let s = Setup::at(ONE_FIRST);
    let answer = s.latest().unwrap();
    assert_eq!(answer.text, ANSWER_ONE);
    assert!(!answer.text.contains("redacted"));
}

#[test]
fn revert_takes_effect_at_once() {
    // Before the revert, the latest answer was "Got it.".
    let before = Setup::at(GOT_IT);
    assert!(before.latest().unwrap().text.starts_with("Got it."));
    assert_eq!(before.history_of(MAIN).len(), 2);
    // `/revert` moves the main chain before any new prompt.
    let after = Setup::at(ONE_REVERTED);
    assert_eq!(after.latest().unwrap().text, ANSWER_ONE);
    assert_eq!(after.history_of(MAIN), [ANSWER_ONE]);
}

#[test]
fn rewritten_nodes_keep_the_answer_identity() {
    let first = Setup::at(ONE_FIRST).latest().unwrap();
    let rewritten = Setup::at(ONE_REVERTED).latest().unwrap();
    assert_eq!(first.answer_id, rewritten.answer_id);
    assert_eq!(first.source_fingerprint, rewritten.source_fingerprint);
}

#[test]
fn interrupted_turns_have_no_answer() {
    for node in [SLEEP_CANCELED, STREAM_INTERRUPTED] {
        let s = Setup::at(node);
        assert!(
            matches!(s.latest(), Err(HandoffError::NoCompletedAnswer(_))),
            "{node}"
        );
        // Older answers can still be picked.
        assert_eq!(s.history_of(MAIN), [ANSWER_ONE], "{node}");
        assert_eq!(s.accepts(MAIN), Ok(()), "{node}");
    }
}

#[test]
fn a_running_turn_is_uncertain_and_not_a_target() {
    for node in [SLEEP_PROMPT, SLEEP_CALL] {
        let s = Setup::at(node);
        assert!(
            matches!(s.latest(), Err(HandoffError::CompletionUncertain(_))),
            "{node}"
        );
        assert!(
            matches!(s.accepts(MAIN), Err(HandoffError::AgentNotReady(_))),
            "{node}"
        );
    }
}

#[test]
fn a_finished_session_takes_prompts() {
    let s = Setup::new();
    assert_eq!(s.accepts(MAIN), Ok(()));
    assert_eq!(s.accepts(CLEARED), Ok(()));
    // A session Devin has not written yet (no first prompt) cannot be
    // waiting for anything.
    assert_eq!(s.accepts("fresh-session"), Ok(()));
}

#[test]
fn sessions_of_one_database_stay_apart() {
    let s = Setup::new();
    assert_eq!(s.latest_of(CLEARED).unwrap().text, "ANSWER-FIVE");
    assert_eq!(s.history_of(CLEARED), ["ANSWER-FIVE", "AFTER-CLEAR"]);
    let fork = s.latest_of(FORK).unwrap();
    assert_eq!(fork.text, "ANSWER-THREE");
    assert_eq!(fork.session.native_id, FORK);
}

#[test]
fn a_session_without_messages_has_no_answer() {
    let s = Setup::new();
    s.set_main(MAIN, None);
    assert!(matches!(
        s.latest(),
        Err(HandoffError::NoCompletedAnswer(_))
    ));
    assert!(s.history_of(MAIN).is_empty());
}

#[test]
fn an_answer_cut_short_is_not_finished() {
    let s = Setup::at(ONE_FIRST);
    s.sql(
        "UPDATE message_nodes
         SET chat_message = json_set(chat_message, '$.metadata.finish_reason', 'length')
         WHERE session_id = 'thundering-myrtle' AND node_id = 26",
    );
    assert!(matches!(
        s.latest(),
        Err(HandoffError::NoCompletedAnswer(_))
    ));
}

#[test]
fn unknown_and_duplicate_sessions_are_refused() {
    let s = Setup::new();
    assert!(matches!(
        s.latest_of("no-such-session"),
        Err(HandoffError::TranscriptUnavailable(_))
    ));

    let root = tempfile::tempdir().unwrap();
    let mut config = s.config.clone();
    config.devin_roots = vec![root.path().join("a"), root.path().join("b")];
    write_db(&config.devin_roots[0], &[MAIN]);
    write_db(&config.devin_roots[1], &[MAIN]);
    assert!(matches!(
        DevinAdapter.resolve(&Setup::binding(MAIN), &config),
        Err(HandoffError::SessionAmbiguous(_))
    ));
}

#[test]
fn other_agents_are_not_resolved() {
    let s = Setup::new();
    let mut binding = Setup::binding(MAIN);
    binding.agent = AgentKind::Claude;
    assert!(matches!(
        DevinAdapter.resolve(&binding, &s.config),
        Err(HandoffError::UnsupportedAgent(_))
    ));
}

#[test]
fn a_parent_cycle_is_corrupt() {
    let s = Setup::at(THREE);
    s.sql(
        "UPDATE message_nodes SET parent_node_id = 78
         WHERE session_id = 'thundering-myrtle' AND node_id = 76",
    );
    let session = DevinAdapter
        .resolve(&Setup::binding(MAIN), &s.config)
        .unwrap();
    assert!(matches!(
        DevinAdapter.completed_answers(&session, &s.config.limits, 50),
        Err(HandoffError::TranscriptCorrupt(_))
    ));
}

#[test]
fn a_missing_parent_is_unsupported() {
    let s = Setup::at(THREE);
    s.sql("DELETE FROM message_nodes WHERE session_id = 'thundering-myrtle' AND node_id = 76");
    let session = DevinAdapter
        .resolve(&Setup::binding(MAIN), &s.config)
        .unwrap();
    assert!(matches!(
        DevinAdapter.completed_answers(&session, &s.config.limits, 50),
        Err(HandoffError::UnsupportedTranscript(_))
    ));
}

#[test]
fn malformed_messages_are_corrupt() {
    let s = Setup::at(THREE);
    s.sql(
        "UPDATE message_nodes SET chat_message = '{\"role\": \"assistant\", '
         WHERE session_id = 'thundering-myrtle' AND node_id = 78",
    );
    assert!(matches!(
        s.latest(),
        Err(HandoffError::TranscriptCorrupt(_))
    ));
}

#[test]
fn an_answer_over_the_line_limit_is_refused() {
    let mut s = Setup::at(ONE_FIRST);
    s.config.limits.max_line_bytes = 10;
    assert!(matches!(
        s.latest(),
        Err(HandoffError::ReadLimitExceeded(_))
    ));
}

#[test]
fn history_stops_at_max() {
    let s = Setup::new();
    let session = DevinAdapter
        .resolve(&Setup::binding(MAIN), &s.config)
        .unwrap();
    let answers = DevinAdapter
        .completed_answers(&session, &s.config.limits, 1)
        .unwrap();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].text, "ANSWER-THREE");
}
