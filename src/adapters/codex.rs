//! Codex rollouts: `<root>/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`
//! and, after a rewind, `…-<thread id>_<segment>.jsonl`.
//!
//! Verified on 0.160.1 (docs/compatibility.md). Only the active file (the
//! newest segment, else the base file) is read: a rewind or fork starts a
//! file whose `history_base` points into older history, and until the
//! first new turn in it there is no answer to hand off. A turn runs from
//! `task_started` to `task_complete` (or `turn_aborted`); its answer is the
//! assistant message with `phase: "final_answer"`, which must also be the
//! turn's `last_agent_message`.

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
}

/// An assistant message of the current turn, to be read again if chosen.
struct Message {
    id: String,
    offset: u64,
    len: usize,
}

enum End {
    Complete { last_agent_message: Option<String> },
    Aborted,
}

struct Turn {
    id: String,
    finals: Vec<Message>,
    unphased: Vec<Message>,
    end: Option<End>,
}

fn read_latest(
    session: &ResolvedSession,
    limits: &ReadLimits,
) -> Result<AnswerSnapshot, HandoffError> {
    let path = &session.transcript_path;
    let id = session.native_id.as_str();
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
    if first.value["type"] != "session_meta" || meta["id"].as_str() != Some(id) {
        return Err(unsupported(
            "session_meta id does not match the Herdr session",
        ));
    }
    let is_segment = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(&format!("{id}_")));
    if is_segment && meta["history_base"]["thread_id"].as_str() != Some(id) {
        return Err(unsupported("the segment belongs to another thread"));
    }

    let mut turn: Option<Turn> = None;
    for record in records.by_ref() {
        let record = record?;
        let r = &record.value;
        let p = &r["payload"];
        match (r["type"].as_str(), p["type"].as_str()) {
            (Some("event_msg"), Some("task_started")) => {
                turn = Some(Turn {
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
                let Some(turn) = turn.as_mut().filter(|turn| turn.end.is_none()) else {
                    continue;
                };
                if p["turn_id"].as_str() != Some(&turn.id) {
                    continue;
                }
                turn.end = Some(if t == "task_complete" {
                    End::Complete {
                        last_agent_message: p["last_agent_message"].as_str().map(str::to_string),
                    }
                } else {
                    End::Aborted
                });
            }
            (Some("response_item"), Some("message")) if p["role"] == "assistant" => {
                let Some(turn) = turn.as_mut().filter(|turn| turn.end.is_none()) else {
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
    if records.partial_tail() {
        return Err(HandoffError::CompletionUncertain(
            "the transcript is still being written".into(),
        ));
    }

    let Some(turn) = turn else {
        return Err(HandoffError::NoCompletedAnswer(
            "this session has no answer yet (after a rewind or fork, send one prompt first)".into(),
        ));
    };
    let last_agent_message = match turn.end {
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
        Some(End::Complete { last_agent_message }) => last_agent_message,
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
        source_fingerprint: fingerprint(&[id, &name(path), &turn.id, &chosen.id, &text]),
        session: session.clone(),
        answer_id: chosen.id.clone(),
        text,
    })
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
