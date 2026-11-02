//! Devin CLI sessions: `<root>/sessions.db`, an SQLite database (WAL) that
//! holds every session. It is opened read-only.
//!
//! Verified on 3000.11.234 (docs/compatibility.md). `message_nodes` holds a
//! tree of messages per session (`node_id`, `parent_node_id`, the message
//! as JSON in `chat_message`); `sessions.main_chain_id` names the newest
//! node of the current conversation, so `/revert` moves it to another
//! branch at once. A message is written whole, once it is finished. A turn
//! has ended normally when the chain ends, past system messages, with an
//! assistant message that has `finish_reason: "stop"` and no tool calls.
//! An interrupt ends the chain with a system message
//! `[Response interrupted by user]` (after the partial message) or with a
//! tool result failed as `Canceled`. A compaction ends the chain with a
//! system message whose node `metadata.summarized_from` names the newest
//! node before it; following that keeps the answers from before it.
//!
//! Herdr's screen rules do not recognize Devin's approval prompt and report
//! it as idle, so `accepts_prompt` refuses a session whose chain ends in a
//! pending tool call: Enter would approve it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};

use super::{AnswerAdapter, fingerprint, with_retries};
use crate::config::{Config, ReadLimits};
use crate::error::HandoffError;
use crate::model::{AgentKind, AnswerSnapshot, PaneBinding, ResolvedSession};
use crate::session::{unique, validate_devin_session_id};

pub struct DevinAdapter;

impl AnswerAdapter for DevinAdapter {
    fn resolve(
        &self,
        binding: &PaneBinding,
        config: &Config,
    ) -> Result<ResolvedSession, HandoffError> {
        if binding.agent != AgentKind::Devin {
            return Err(HandoffError::UnsupportedAgent(
                binding.agent.display_name().into(),
            ));
        }
        let id = &binding.session.value;
        let path = find_devin_database(&config.devin_roots, id)?;
        Ok(ResolvedSession {
            binding: binding.clone(),
            native_id: id.clone(),
            transcript_path: path,
        })
    }

    fn latest_completed(
        &self,
        session: &ResolvedSession,
        limits: &ReadLimits,
    ) -> Result<AnswerSnapshot, HandoffError> {
        with_retries(&limits.retry_delays, || read_latest(session, limits))
    }

    fn completed_answers(
        &self,
        session: &ResolvedSession,
        limits: &ReadLimits,
        max: usize,
    ) -> Result<Vec<AnswerSnapshot>, HandoffError> {
        with_retries(&limits.retry_delays, || read_history(session, limits, max))
    }

    fn accepts_prompt(&self, binding: &PaneBinding, config: &Config) -> Result<(), HandoffError> {
        let session = match self.resolve(binding, config) {
            Ok(session) => session,
            // Devin writes a session at its first prompt: nothing can be
            // pending before that.
            Err(HandoffError::TranscriptUnavailable(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut db = Db::open(&session.transcript_path, &session.native_id)?;
        let Some(end) = db.last_message()? else {
            return Ok(());
        };
        match end.kind {
            // Its turn is running: a tool call waits for approval or runs,
            // or the model is answering a prompt or a tool result.
            Kind::Assistant {
                tool_calls: true, ..
            }
            | Kind::User
            | Kind::Tool { canceled: false } => Err(HandoffError::AgentNotReady(
                "Devin is working or waiting for approval".into(),
            )),
            _ => Ok(()),
        }
    }
}

/// The `sessions.db` of `roots` that holds session `id`; exactly one must.
pub fn find_devin_database(roots: &[PathBuf], id: &str) -> Result<PathBuf, HandoffError> {
    validate_devin_session_id(id)?;
    let mut found = Vec::new();
    for root in roots {
        let path = root.join("sessions.db");
        if !path.is_file() {
            continue;
        }
        let conn = connect(&path)?;
        let known = conn
            .query_row("SELECT 1 FROM sessions WHERE id = ?1", [id], |_| Ok(()))
            .optional()
            .map_err(db_error)?;
        if known.is_some() {
            found.push(path);
        }
    }
    unique(found, id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Assistant {
        finish_reason: Option<String>,
        tool_calls: bool,
    },
    /// What the user typed (one or more queued prompts start a turn).
    User,
    Tool {
        canceled: bool,
    },
    Interrupted,
    /// Other system messages: instructions, skill listings, compaction
    /// summaries. Never part of an answer and never the end of a turn.
    Aside,
}

impl Kind {
    /// Whether this message is the answer that ended its turn.
    fn is_final_answer(&self) -> bool {
        matches!(self, Kind::Assistant { finish_reason: Some(r), tool_calls: false } if r == "stop")
    }
}

/// What is kept of a message: its links and kind, but not its text.
#[derive(Debug)]
struct Message {
    node_id: i64,
    /// The node before it: `summarized_from` where a compaction set it,
    /// else the parent.
    previous: Option<i64>,
    kind: Kind,
    message_id: String,
    created_at: Option<String>,
}

const INTERRUPTED: &str = "[Response interrupted by user]";

/// One read transaction on the database, for one session.
struct Db {
    conn: Connection,
    session: String,
    /// Nodes visited, to stop at a cycle of parent links.
    seen: HashSet<i64>,
}

impl Db {
    fn open(path: &Path, session: &str) -> Result<Self, HandoffError> {
        let conn = connect(path)?;
        // One snapshot for every query, so that Devin writing meanwhile
        // cannot mix two states of the conversation.
        conn.execute_batch("BEGIN").map_err(db_error)?;
        Ok(Self {
            conn,
            session: session.to_string(),
            seen: HashSet::new(),
        })
    }

    /// The newest node of the current conversation, if any.
    fn main_chain(&self) -> Result<Option<i64>, HandoffError> {
        let row: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT main_chain_id FROM sessions WHERE id = ?1",
                [&self.session],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        match row {
            Some(main) => Ok(main),
            None => Err(HandoffError::TranscriptUnavailable(format!(
                "{} is no longer in the Devin database",
                self.session
            ))),
        }
    }

    fn message(&mut self, node_id: i64) -> Result<Message, HandoffError> {
        if !self.seen.insert(node_id) {
            return Err(HandoffError::TranscriptCorrupt(
                "parent links form a cycle".into(),
            ));
        }
        let row = self
            .conn
            .query_row(
                "SELECT parent_node_id,
                        json_extract(metadata, '$.summarized_from'),
                        json_extract(chat_message, '$.role'),
                        json_extract(chat_message, '$.message_id'),
                        json_extract(chat_message, '$.metadata.finish_reason'),
                        json_array_length(chat_message, '$.tool_calls'),
                        json_extract(chat_message, '$.metadata.created_at'),
                        json_extract(chat_message,
                            '$.metadata.extensions.\"chisel/tool_failure\".reason'),
                        CASE WHEN json_extract(chat_message, '$.role') = 'system'
                             THEN substr(json_extract(chat_message, '$.content'), 1, 64)
                        END
                 FROM message_nodes WHERE session_id = ?1 AND node_id = ?2",
                params![self.session, node_id],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        let Some((parent, summarized_from, role, message_id, finish, calls, at, failure, head)) =
            row
        else {
            return Err(HandoffError::UnsupportedTranscript(
                "a parent message is missing".into(),
            ));
        };
        let kind = match role.as_deref() {
            Some("assistant") => Kind::Assistant {
                finish_reason: finish,
                tool_calls: calls.unwrap_or(0) > 0,
            },
            // Typed prompts carry `is_user_input`; any other user message
            // (not seen on the verified version) is also taken to start a
            // turn, so that it is never mistaken for the end of one.
            Some("user") => Kind::User,
            Some("tool") => Kind::Tool {
                canceled: failure.as_deref() == Some("Canceled"),
            },
            Some("system") if head.as_deref().is_some_and(|h| h.starts_with(INTERRUPTED)) => {
                Kind::Interrupted
            }
            Some("system") => Kind::Aside,
            other => {
                return Err(HandoffError::UnsupportedTranscript(format!(
                    "message role {}",
                    other.unwrap_or("missing")
                )));
            }
        };
        Ok(Message {
            node_id,
            previous: summarized_from.or(parent),
            kind,
            message_id: message_id.unwrap_or_default(),
            created_at: at,
        })
    }

    fn previous(&mut self, m: &Message) -> Result<Option<Message>, HandoffError> {
        m.previous.map(|p| self.message(p)).transpose()
    }

    /// The newest message of the current conversation that is not an
    /// aside, if there is one.
    fn last_message(&mut self) -> Result<Option<Message>, HandoffError> {
        let mut cur = match self.main_chain()? {
            Some(n) => Some(self.message(n)?),
            None => None,
        };
        while let Some(m) = cur.take_if(|m| m.kind == Kind::Aside) {
            cur = self.previous(&m)?;
        }
        Ok(cur)
    }

    /// The text of the message at `node_id`, refused past `max_bytes`.
    fn content(&self, node_id: i64, max_bytes: usize) -> Result<String, HandoffError> {
        let (len, text): (Option<i64>, Option<String>) = self
            .conn
            .query_row(
                "SELECT length(CAST(c AS BLOB)), CASE WHEN length(CAST(c AS BLOB)) <= ?3 THEN c END
                 FROM (SELECT json_extract(chat_message, '$.content') AS c,
                              json_type(chat_message, '$.content') AS t
                       FROM message_nodes WHERE session_id = ?1 AND node_id = ?2)
                 WHERE t IS NULL OR t = 'text'",
                params![self.session, node_id, max_bytes as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| {
                HandoffError::UnsupportedTranscript("the answer is not plain text".into())
            })?;
        match (len, text) {
            (_, Some(text)) => Ok(text),
            (None, None) => Ok(String::new()),
            (Some(len), None) => Err(HandoffError::ReadLimitExceeded(format!(
                "the answer is {len} bytes (limit {max_bytes})"
            ))),
        }
    }
}

fn read_latest(
    session: &ResolvedSession,
    limits: &ReadLimits,
) -> Result<AnswerSnapshot, HandoffError> {
    let mut db = Db::open(&session.transcript_path, &session.native_id)?;
    let Some(end) = db.last_message()? else {
        return Err(HandoffError::NoCompletedAnswer(
            "this session has no answer yet".into(),
        ));
    };
    match &end.kind {
        k if k.is_final_answer() => snapshot(&db, session, &end, limits)?
            .ok_or_else(|| HandoffError::NoCompletedAnswer("the answer is empty".into())),
        Kind::Interrupted | Kind::Tool { canceled: true } => Err(HandoffError::NoCompletedAnswer(
            "the latest turn was interrupted".into(),
        )),
        Kind::Assistant {
            finish_reason: Some(r),
            tool_calls: false,
        } if r != "tool_calls" => Err(HandoffError::NoCompletedAnswer(format!(
            "the latest answer did not end normally ({r})"
        ))),
        _ => Err(HandoffError::CompletionUncertain(
            "the latest turn has not finished".into(),
        )),
    }
}

/// The finished answers of the current conversation, newest first, at
/// most `max`. An answer counts when the next message (past asides) is a
/// prompt, or when it ends the conversation.
fn read_history(
    session: &ResolvedSession,
    limits: &ReadLimits,
    max: usize,
) -> Result<Vec<AnswerSnapshot>, HandoffError> {
    let mut db = Db::open(&session.transcript_path, &session.native_id)?;
    let mut answers = Vec::new();
    let mut newer: Option<Kind> = None;
    let mut cur = db.last_message()?;
    while let Some(m) = cur {
        if answers.len() >= max {
            break;
        }
        if m.kind != Kind::Aside {
            let ends_turn = matches!(newer, None | Some(Kind::User));
            if m.kind.is_final_answer() && ends_turn {
                if let Some(answer) = snapshot(&db, session, &m, limits)? {
                    answers.push(answer);
                }
            }
            newer = Some(m.kind.clone());
        }
        cur = db.previous(&m)?;
    }
    Ok(answers)
}

/// The answer `m`; None when it has no text.
fn snapshot(
    db: &Db,
    session: &ResolvedSession,
    m: &Message,
    limits: &ReadLimits,
) -> Result<Option<AnswerSnapshot>, HandoffError> {
    let text = db.content(m.node_id, limits.max_line_bytes)?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    // Node ids change when Devin rewrites the conversation (after
    // `/revert`); the message id stays.
    let answer_id = if m.message_id.is_empty() {
        format!("node-{}", m.node_id)
    } else {
        m.message_id.clone()
    };
    Ok(Some(AnswerSnapshot {
        source_fingerprint: fingerprint(&[&session.native_id, &answer_id, &text]),
        session: session.clone(),
        answer_id,
        text,
        finished_at: m.created_at.clone(),
        chosen: false,
    }))
}

fn connect(path: &Path) -> Result<Connection, HandoffError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| HandoffError::TranscriptUnavailable(format!("{}: {e}", path.display())))?;
    conn.busy_timeout(Duration::from_millis(500))
        .map_err(db_error)?;
    Ok(conn)
}

fn db_error(e: rusqlite::Error) -> HandoffError {
    match e.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            HandoffError::CompletionUncertain("the Devin database is busy".into())
        }
        _ => {
            let text = e.to_string();
            if text.contains("malformed JSON") {
                HandoffError::TranscriptCorrupt("a message is not valid JSON".into())
            } else if text.contains("no such") {
                HandoffError::UnsupportedTranscript(format!("Devin database: {text}"))
            } else {
                HandoffError::TranscriptUnavailable(format!("Devin database: {text}"))
            }
        }
    }
}
