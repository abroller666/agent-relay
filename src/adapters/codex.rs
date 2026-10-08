//! Codex rollouts: `<root>/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`
//! and, after a rewind, `…-<thread id>_<segment>.jsonl`.
//!
//! Verified on 0.160.1 (docs/compatibility.md). The latest answer comes
//! from the active file only (the newest segment, else the base file): a
//! rewind or fork starts a file whose `history_base` points into older
//! history, and until the first new turn in it there is no latest answer.
//! The history of answers follows `history_base` into the older files. A turn runs from
//! `task_started` to `task_complete` (or `turn_aborted`); its answer is the
//! assistant message with `phase: "final_answer"`, which must also be the
//! turn's `last_agent_message`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{AnswerAdapter, fingerprint, with_retries};
use crate::config::{Config, ReadLimits};
use crate::error::HandoffError;
use crate::model::{AgentKind, AnswerSnapshot, PaneBinding, ResolvedSession};
use crate::session::find_codex_rollouts;
use crate::transcript::{name, read_at, read_records};

pub struct CodexAdapter;

impl AnswerAdapter for CodexAdapter {
    fn resolve(
        &self,
        binding: &PaneBinding,
        config: &Config,
    ) -> Result<ResolvedSession, HandoffError> {
        if binding.agent != AgentKind::Codex {
            return Err(HandoffError::UnsupportedAgent(
                binding.agent.display_name().into(),
            ));
        }
        let rollouts =
            find_codex_rollouts(&config.codex_roots, &binding.session.value, &config.limits)?;
        Ok(ResolvedSession {
            binding: binding.clone(),
            native_id: binding.session.value.clone(),
            transcript_path: rollouts.active().to_path_buf(),
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
}

/// An assistant message of a turn, to be read again if chosen.
struct Message {
    id: String,
    offset: u64,
    len: usize,
}

enum End {
    Complete {
        last_agent_message: Option<String>,
        at: Option<String>,
    },
    Aborted,
}

struct Turn {
    id: String,
    finals: Vec<Message>,
    unphased: Vec<Message>,
    end: Option<End>,
}

/// What one rollout file holds.
struct Scanned {
    /// The turns, oldest first.
    turns: Vec<Turn>,
    /// Where the history before this file is: (thread id, first ordinal
    /// not taken from it).
    base: Option<(String, u64)>,
}

/// The turns of rollout `path` of thread `thread`, keeping only records
/// with an ordinal below `limit` when one is given.
fn scan(
    path: &Path,
    thread: &str,
    limit: Option<u64>,
    limits: &ReadLimits,
) -> Result<Scanned, HandoffError> {
    let unsupported =
        |what: &str| HandoffError::UnsupportedTranscript(format!("{}: {what}", name(path)));
    let mut records = read_records(path, limits)?;

    let first = match records.next() {
        Some(r) => r?,
        None if records.partial_tail() => {
            return Err(HandoffError::CompletionUncertain(
                "the transcript is still being written".into(),
            ));
        }
        None => return Err(unsupported("empty file")),
    };
    let meta = &first.value["payload"];
    if first.value["type"] != "session_meta" || meta["id"].as_str() != Some(thread) {
        return Err(unsupported(
            "session_meta id does not match the Herdr session",
        ));
    }
    let is_segment = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(&format!("{thread}_")));
    if is_segment && meta["history_base"]["thread_id"].as_str() != Some(thread) {
        return Err(unsupported("the segment belongs to another thread"));
    }
    let base = match (
        meta["history_base"]["thread_id"].as_str(),
        meta["history_base"]["end_ordinal_exclusive"].as_u64(),
    ) {
        (Some(t), Some(end)) => Some((t.to_string(), end)),
        _ => None,
    };

    let mut turns: Vec<Turn> = Vec::new();
    for record in records.by_ref() {
        let record = record?;
        let r = &record.value;
        if let Some(limit) = limit {
            match r["ordinal"].as_u64() {
                Some(o) if o >= limit => break,
                Some(_) => {}
                None => return Err(unsupported("a record has no ordinal")),
            }
        }
        let p = &r["payload"];
        match (r["type"].as_str(), p["type"].as_str()) {
            (Some("event_msg"), Some("task_started")) => {
                turns.push(Turn {
                    id: p["turn_id"].as_str().unwrap_or_default().to_string(),
                    finals: Vec::new(),
                    unphased: Vec::new(),
                    end: None,
                });
            }
            (Some("event_msg"), Some(t)) if t.contains("roll") && t.contains("back") => {
                return Err(unsupported("unverified rollback record"));
            }
            (Some("event_msg"), Some(t @ ("task_complete" | "turn_aborted"))) => {
                let Some(turn) = turns.last_mut().filter(|turn| turn.end.is_none()) else {
                    continue;
                };
                if p["turn_id"].as_str() != Some(&turn.id) {
                    continue;
                }
                turn.end = Some(if t == "task_complete" {
                    End::Complete {
                        last_agent_message: p["last_agent_message"].as_str().map(str::to_string),
                        at: r["timestamp"].as_str().map(str::to_string),
                    }
                } else {
                    End::Aborted
                });
            }
            (Some("response_item"), Some("message")) if p["role"] == "assistant" => {
                let Some(turn) = turns.last_mut().filter(|turn| turn.end.is_none()) else {
                    continue;
                };
                let owner = &p["internal_chat_message_metadata_passthrough"]["turn_id"];
                if !owner.is_null() && owner.as_str() != Some(&turn.id) {
                    return Err(unsupported("a message of another turn is mixed in"));
                }
                let message = Message {
                    id: p["id"].as_str().unwrap_or_default().to_string(),
                    offset: record.offset,
                    len: record.len,
                };
                match p["phase"].as_str() {
                    Some("final_answer") => turn.finals.push(message),
                    None => turn.unphased.push(message),
                    Some(_) => {} // commentary: progress, not the answer
                }
            }
            _ => {}
        }
    }
    if limit.is_none() && records.partial_tail() {
        return Err(HandoffError::CompletionUncertain(
            "the transcript is still being written".into(),
        ));
    }
    Ok(Scanned { turns, base })
}

/// The answer of `turn` (in rollout `path`), or why it has none.
fn turn_answer(
    session: &ResolvedSession,
    path: &Path,
    turn: &Turn,
) -> Result<AnswerSnapshot, HandoffError> {
    let (last_agent_message, at) = match &turn.end {
        None => {
            return Err(HandoffError::CompletionUncertain(
                "the latest turn has not finished".into(),
            ));
        }
        Some(End::Aborted) => {
            return Err(HandoffError::NoCompletedAnswer(
                "the latest turn was interrupted".into(),
            ));
        }
        Some(End::Complete {
            last_agent_message,
            at,
        }) => (last_agent_message, at),
    };
    // The same message can be recorded more than once; the last record of
    // the last final message is the answer.
    let (chosen, phased) = match (turn.finals.last(), turn.unphased.last()) {
        (Some(m), _) => (m, true),
        (None, Some(m)) => (m, false),
        (None, None) => {
            return Err(HandoffError::NoCompletedAnswer(
                "the latest turn has no final answer".into(),
            ));
        }
    };
    let text = message_text(&read_at(path, chosen.offset, chosen.len)?);
    let matches_completion = last_agent_message.as_deref() == Some(text.as_str());
    if !matches_completion && (!phased || last_agent_message.is_some()) {
        return Err(HandoffError::CompletionUncertain(
            "the final answer does not match the turn completion record".into(),
        ));
    }
    if text.trim().is_empty() {
        return Err(HandoffError::NoCompletedAnswer(
            "the answer is empty".into(),
        ));
    }
    Ok(AnswerSnapshot {
        source_fingerprint: fingerprint(&[&session.native_id, &turn.id, &chosen.id, &text]),
        session: session.clone(),
        answer_id: chosen.id.clone(),
        text,
        finished_at: at.clone(),
        chosen: false,
    })
}

fn read_latest(
    session: &ResolvedSession,
    limits: &ReadLimits,
) -> Result<AnswerSnapshot, HandoffError> {
    let path = &session.transcript_path;
    let scanned = scan(path, &session.native_id, None, limits)?;
    let Some(turn) = scanned.turns.last() else {
        return Err(HandoffError::NoCompletedAnswer(
            "this session has no answer yet (after a rewind or fork, send one prompt first)".into(),
        ));
    };
    turn_answer(session, path, turn)
}

/// How many files back a history may reach through rewinds and forks.
const MAX_HISTORY_FILES: usize = 16;

/// The finished answers of the thread's current history, newest first:
/// the active file, then, through each file's `history_base`, the part of
/// the older history it continues from.
fn read_history(
    session: &ResolvedSession,
    limits: &ReadLimits,
    max: usize,
) -> Result<Vec<AnswerSnapshot>, HandoffError> {
    let mut answers = Vec::new();
    let mut path = session.transcript_path.clone();
    let mut thread = session.native_id.clone();
    let mut limit: Option<u64> = None;
    for _ in 0..MAX_HISTORY_FILES {
        let scanned = scan(&path, &thread, limit, limits)?;
        for turn in scanned.turns.iter().rev() {
            if answers.len() >= max {
                return Ok(answers);
            }
            if let Ok(answer) = turn_answer(session, &path, turn) {
                answers.push(answer);
            }
        }
        let Some((base_thread, end)) = scanned.base else {
            break;
        };
        let end = limit.map_or(end, |l| l.min(end));
        match older_file(&path, &base_thread, end, limits)? {
            Some(older) => {
                path = older;
                thread = base_thread;
                limit = Some(end);
            }
            None => break,
        }
    }
    Ok(answers)
}

/// The newest rollout of `thread`, other than `current`, that holds
/// records before ordinal `end`. Rollouts of a thread share one root.
fn older_file(
    current: &Path,
    thread: &str,
    end: u64,
    limits: &ReadLimits,
) -> Result<Option<PathBuf>, HandoffError> {
    // <root>/YYYY/MM/DD/<file>
    let Some(root) = current.ancestors().nth(4) else {
        return Ok(None);
    };
    let rollouts = match find_codex_rollouts(&[root.to_path_buf()], thread, limits) {
        Ok(r) => r,
        Err(HandoffError::TranscriptUnavailable(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let candidates = rollouts
        .segments
        .iter()
        .rev()
        .map(|(_, p)| p.clone())
        .chain(rollouts.base.clone());
    for candidate in candidates {
        if candidate == current {
            continue;
        }
        if first_ordinal(&candidate, limits)?.is_some_and(|o| o < end) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn first_ordinal(path: &Path, limits: &ReadLimits) -> Result<Option<u64>, HandoffError> {
    match read_records(path, limits)?.next() {
        Some(r) => Ok(r?.value["ordinal"].as_u64()),
        None => Ok(None),
    }
}

/// The `output_text` parts of a message, in order.
fn message_text(r: &Value) -> String {
    r["payload"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["type"] == "output_text")
        .filter_map(|c| c["text"].as_str())
        .collect()
}
