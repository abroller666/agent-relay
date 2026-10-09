//! Claude Code transcripts: `<root>/<project>/<session id>.jsonl`.
//!
//! Verified on 2.1.293 (docs/compatibility.md). Conversation records form a
//! tree through `uuid` / `parentUuid`; the active branch is the chain from
//! the last record written. An assistant message is one record per content
//! block, sharing `message.id`. A turn has ended normally when its chain
//! reads, from the newest record back: `system` `turn_duration`, then
//! (past attachments and other system records) the assistant message with
//! `stop_reason: "end_turn"`. A compaction starts a new chain, linked back
//! to the old one through the boundary's `logicalParentUuid`.

use std::collections::HashMap;

use serde_json::Value;

use super::{AnswerAdapter, fingerprint, with_retries};
use crate::config::{Config, ReadLimits};
use crate::error::HandoffError;
use crate::model::{AgentKind, AnswerSnapshot, PaneBinding, ResolvedSession};
use crate::session::find_claude_transcript;
use crate::transcript::{read_at, read_records};

pub struct ClaudeAdapter;

impl AnswerAdapter for ClaudeAdapter {
    fn resolve(
        &self,
        binding: &PaneBinding,
        config: &Config,
    ) -> Result<ResolvedSession, HandoffError> {
        if binding.agent != AgentKind::Claude {
            return Err(HandoffError::UnsupportedAgent(
                binding.agent.display_name().into(),
            ));
        }
        let path =
            find_claude_transcript(&config.claude_roots, &binding.session.value, &config.limits)?;
        Ok(ResolvedSession {
            binding: binding.clone(),
            native_id: binding.session.value.clone(),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Assistant {
        message_id: String,
        stop_reason: Option<String>,
        api_error: bool,
    },
    TurnDuration,
    /// Other system records and attachments: never part of an answer, and
    /// written around a turn's end (stop hooks, away summaries).
    Aside,
    User {
        interrupted: bool,
    },
    Other,
}

/// What is kept of a conversation record: its links and kind, and where to
/// read it again, but not its text.
#[derive(Debug)]
struct Node {
    parent: Option<String>,
    kind: Kind,
    same_session: bool,
    timestamp: Option<String>,
    offset: u64,
    len: usize,
}

fn kind_of(r: &Value) -> Kind {
    match r["type"].as_str() {
        Some("assistant") => Kind::Assistant {
            message_id: r["message"]["id"].as_str().unwrap_or_default().to_string(),
            stop_reason: r["message"]["stop_reason"].as_str().map(str::to_string),
            api_error: r["isApiErrorMessage"].as_bool().unwrap_or(false),
        },
        Some("system") if r["subtype"] == "turn_duration" => Kind::TurnDuration,
        Some("system" | "attachment") => Kind::Aside,
        Some("user") if written_by_compaction(r) => Kind::Aside,
        Some("user") => Kind::User {
            interrupted: user_text(r)
                .is_some_and(|t| t.starts_with("[Request interrupted by user")),
        },
        _ => Kind::Other,
    }
}

/// Whether a user record is one that `/compact` writes after the boundary:
/// the summary, the command's caveat (`isMeta`), the command and its
/// output. None of them starts a turn.
fn written_by_compaction(r: &Value) -> bool {
    r["isCompactSummary"].as_bool() == Some(true)
        || r["isMeta"].as_bool() == Some(true)
        || user_text(r).is_some_and(|t| {
            t.starts_with("<command-name>/compact</command-name>")
                || t.starts_with("<local-command-stdout>")
        })
}

/// The parent of a record. A compaction boundary starts a new chain
/// (`parentUuid: null`) and names the record it follows in
/// `logicalParentUuid`; following that keeps the answers from before it.
fn parent_of(r: &Value) -> Option<String> {
    let link = if r["type"] == "system" && r["subtype"] == "compact_boundary" {
        &r["logicalParentUuid"]
    } else {
        &r["parentUuid"]
    };
    link.as_str().map(str::to_string)
}

/// The text of a user record whose content is a single text block.
fn user_text(r: &Value) -> Option<&str> {
    match &r["message"]["content"] {
        Value::String(s) => Some(s),
        Value::Array(blocks) => blocks.first()?.get("text")?.as_str(),
        _ => None,
    }
}

/// The conversation records of a transcript and the newest of them.
struct Loaded {
    nodes: HashMap<String, Node>,
    leaf: Option<String>,
}

fn load(session: &ResolvedSession, limits: &ReadLimits) -> Result<Loaded, HandoffError> {
    let mut records = read_records(&session.transcript_path, limits)?;
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut leaf: Option<String> = None;
    for record in records.by_ref() {
        let record = record?;
        let r = &record.value;
        let Some(uuid) = r["uuid"].as_str() else {
            continue; // metadata: titles, modes, snapshots
        };
        if r["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        leaf = Some(uuid.to_string());
        nodes.entry(uuid.to_string()).or_insert(Node {
            parent: parent_of(r),
            kind: kind_of(r),
            same_session: r["sessionId"].as_str() == Some(&session.native_id),
            timestamp: r["timestamp"].as_str().map(str::to_string),
            offset: record.offset,
            len: record.len,
        });
    }
    if records.partial_tail() {
        return Err(HandoffError::CompletionUncertain(
            "the transcript is still being written".into(),
        ));
    }
    Ok(Loaded { nodes, leaf })
}

fn read_latest(
    session: &ResolvedSession,
    limits: &ReadLimits,
) -> Result<AnswerSnapshot, HandoffError> {
    let Loaded { nodes, leaf } = load(session, limits)?;
    let Some(leaf) = leaf else {
        return Err(HandoffError::NoCompletedAnswer(
            "this session has no answer yet".into(),
        ));
    };

    let mut chain = Chain {
        nodes: &nodes,
        steps: 0,
    };
    // Back from the newest record to the end of the turn.
    let mut cur = chain.node(&leaf)?;
    while cur.1.kind == Kind::Aside {
        cur = chain.parent(cur.1)?;
    }
    let turn_end = match &cur.1.kind {
        Kind::TurnDuration => cur,
        Kind::User { interrupted: true } => return Err(interrupted()),
        _ => {
            return Err(HandoffError::CompletionUncertain(
                "the latest turn has not finished".into(),
            ));
        }
    };
    // Back from the end of the turn to its last message.
    let mut cur = chain.parent(turn_end.1)?;
    while cur.1.kind == Kind::Aside {
        cur = chain.parent(cur.1)?;
    }
    let message_id = match &cur.1.kind {
        Kind::Assistant {
            api_error: true, ..
        } => {
            return Err(HandoffError::NoCompletedAnswer(
                "the latest turn ended with an API error".into(),
            ));
        }
        Kind::Assistant {
            message_id,
            stop_reason,
            ..
        } if stop_reason.as_deref() == Some("end_turn") && !message_id.is_empty() => {
            message_id.clone()
        }
        Kind::Assistant { .. } => {
            return Err(HandoffError::NoCompletedAnswer(
                "the latest answer did not end normally".into(),
            ));
        }
        Kind::User { interrupted: true } => return Err(interrupted()),
        _ => {
            return Err(HandoffError::NoCompletedAnswer(
                "the latest turn has no answer".into(),
            ));
        }
    };
    let (parts, _) = message_parts(&mut chain, cur, &message_id)?;
    snapshot(session, turn_end, &message_id, &parts)?
        .ok_or_else(|| HandoffError::NoCompletedAnswer("the answer is empty".into()))
}

/// The finished answers of the active branch, newest first, at most `max`.
/// Turns that ended without an answer (interrupted, failed, empty) and a
/// turn still running are left out.
fn read_history(
    session: &ResolvedSession,
    limits: &ReadLimits,
    max: usize,
) -> Result<Vec<AnswerSnapshot>, HandoffError> {
    let Loaded { nodes, leaf } = load(session, limits)?;
    let mut chain = Chain {
        nodes: &nodes,
        steps: 0,
    };
    let mut answers = Vec::new();
    let mut next = leaf.as_deref().and_then(|l| chain.node(l).ok());
    while let Some(cur) = next {
        if answers.len() >= max {
            break;
        }
        if cur.1.kind != Kind::TurnDuration {
            next = chain.parent(cur.1).ok();
            continue;
        }
        let turn_end = cur;
        let mut cur = match chain.parent(turn_end.1) {
            Ok(c) => c,
            Err(_) => break,
        };
        while cur.1.kind == Kind::Aside {
            match chain.parent(cur.1) {
                Ok(c) => cur = c,
                Err(_) => break,
            }
        }
        let finished = match &cur.1.kind {
            Kind::Assistant {
                message_id,
                stop_reason,
                api_error: false,
            } if stop_reason.as_deref() == Some("end_turn") && !message_id.is_empty() => {
                Some(message_id.clone())
            }
            _ => None,
        };
        next = Some(cur);
        if let Some(message_id) = finished {
            let (parts, first) = message_parts(&mut chain, cur, &message_id)?;
            if let Some(answer) = snapshot(session, turn_end, &message_id, &parts)? {
                answers.push(answer);
            }
            next = Some(first);
        }
        next = next.and_then(|n| chain.parent(n.1).ok());
    }
    Ok(answers)
}

type Entry<'a> = (&'a str, &'a Node);

/// The records of message `message_id` ending at `last`, newest first, and
/// its first record.
fn message_parts<'a>(
    chain: &mut Chain<'a>,
    last: Entry<'a>,
    message_id: &str,
) -> Result<(Vec<&'a Node>, Entry<'a>), HandoffError> {
    let mut parts = Vec::new();
    let mut cur = last;
    let mut first = last;
    loop {
        match &cur.1.kind {
            Kind::Assistant { message_id: id, .. } if id == message_id => {
                parts.push(cur.1);
                first = cur;
            }
            _ => break,
        }
        match cur.1.parent.as_deref() {
            Some(p) if chain.nodes.contains_key(p) => cur = chain.node(p)?,
            _ => break,
        }
    }
    Ok((parts, first))
}

/// The answer made of `parts` (newest first), ended by `turn_end`; None
/// when it has no text.
fn snapshot(
    session: &ResolvedSession,
    turn_end: Entry<'_>,
    message_id: &str,
    parts: &[&Node],
) -> Result<Option<AnswerSnapshot>, HandoffError> {
    if parts.iter().any(|n| !n.same_session) || !turn_end.1.same_session {
        return Err(HandoffError::UnsupportedTranscript(
            "transcript sessionId does not match the Herdr session".into(),
        ));
    }
    let mut text = String::new();
    for node in parts.iter().rev() {
        let r = read_at(&session.transcript_path, node.offset, node.len)?;
        for block in r["message"]["content"].as_array().into_iter().flatten() {
            if block["type"] == "text" {
                append_block(&mut text, block["text"].as_str().unwrap_or_default());
            }
        }
    }
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(AnswerSnapshot {
        source_fingerprint: fingerprint(&[&session.native_id, turn_end.0, message_id, &text]),
        session: session.clone(),
        answer_id: message_id.to_string(),
        text,
        finished_at: turn_end.1.timestamp.clone(),
        chosen: false,
    }))
}

fn interrupted() -> HandoffError {
    HandoffError::NoCompletedAnswer("the latest turn was interrupted".into())
}

/// Appends a text block, separating it from the previous one by a blank
/// line unless the boundary already breaks the line.
fn append_block(text: &mut String, block: &str) {
    if block.is_empty() {
        return;
    }
    if !text.is_empty() && !text.ends_with('\n') && !block.starts_with('\n') {
        text.push_str("\n\n");
    }
    text.push_str(block);
}

/// Parent links, followed with a step budget so a cyclic file cannot hang.
struct Chain<'a> {
    nodes: &'a HashMap<String, Node>,
    steps: usize,
}

impl<'a> Chain<'a> {
    fn node(&mut self, uuid: &'a str) -> Result<(&'a str, &'a Node), HandoffError> {
        self.steps += 1;
        if self.steps > self.nodes.len() + 1 {
            return Err(HandoffError::TranscriptCorrupt(
                "parent links form a cycle".into(),
            ));
        }
        let (key, node) = self.nodes.get_key_value(uuid).ok_or_else(|| {
            HandoffError::UnsupportedTranscript("a parent record is missing".into())
        })?;
        Ok((key.as_str(), node))
    }

    fn parent(&mut self, node: &'a Node) -> Result<(&'a str, &'a Node), HandoffError> {
        match node.parent.as_deref() {
            Some(p) => self.node(p),
            None => Err(HandoffError::NoCompletedAnswer(
                "this session has no answer yet".into(),
            )),
        }
    }
}
