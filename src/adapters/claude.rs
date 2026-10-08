//! Claude Code transcripts: `<root>/<project>/<session id>.jsonl`.
//!
//! Verified on 2.1.293 (docs/compatibility.md). Conversation records form a
//! tree through `uuid` / `parentUuid`; the active branch is the chain from
//! the last record written. An assistant message is one record per content
//! block, sharing `message.id`. A turn has ended normally when its chain
//! reads, from the newest record back: `system` `turn_duration`, then
//! (past attachments and other system records) the assistant message with
//! `stop_reason: "end_turn"`.

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
        Some("user") => Kind::User {
            interrupted: user_text(r)
                .is_some_and(|t| t.starts_with("[Request interrupted by user")),
        },
        _ => Kind::Other,
    }
}

/// The text of a user record whose content is a single text block.
fn user_text(r: &Value) -> Option<&str> {
    match &r["message"]["content"] {
        Value::String(s) => Some(s),
        Value::Array(blocks) => blocks.first()?.get("text")?.as_str(),
        _ => None,
    }
}

fn read_latest(
    session: &ResolvedSession,
    limits: &ReadLimits,
) -> Result<AnswerSnapshot, HandoffError> {
    let path = &session.transcript_path;
    let mut records = read_records(path, limits)?;
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
            parent: r["parentUuid"].as_str().map(str::to_string),
            kind: kind_of(r),
            same_session: r["sessionId"].as_str() == Some(&session.native_id),
            offset: record.offset,
            len: record.len,
        });
    }
    if records.partial_tail() {
        return Err(HandoffError::CompletionUncertain(
            "履歴の書き込みが終わっていません".into(),
        ));
    }
    let Some(leaf) = leaf else {
        return Err(HandoffError::NoCompletedAnswer(
            "このセッションにはまだ回答がありません".into(),
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
        Kind::TurnDuration => cur.0.to_string(),
        Kind::User { interrupted: true } => return Err(interrupted()),
        _ => {
            return Err(HandoffError::CompletionUncertain(
                "最新のターンが完了していません".into(),
            ));
        }
    };
    // Back from the end of the turn to its last message.
    let mut cur = chain.parent(cur.1)?;
    while cur.1.kind == Kind::Aside {
        cur = chain.parent(cur.1)?;
    }
    let message_id = match &cur.1.kind {
        Kind::Assistant {
            api_error: true, ..
        } => {
            return Err(HandoffError::NoCompletedAnswer(
                "最新のターンはAPIエラーで終わりました".into(),
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
                "最新の回答が正常に終了していません".into(),
            ));
        }
        Kind::User { interrupted: true } => return Err(interrupted()),
        _ => {
            return Err(HandoffError::NoCompletedAnswer(
                "最新のターンに回答がありません".into(),
            ));
        }
    };
    // The records of that message, newest first.
    let mut parts = Vec::new();
    loop {
        match &cur.1.kind {
            Kind::Assistant { message_id: id, .. } if *id == message_id => parts.push(cur.1),
            _ => break,
        }
        match cur.1.parent.as_deref() {
            Some(p) if nodes.contains_key(p) => cur = chain.node(p)?,
            _ => break,
        }
    }
    if parts.iter().any(|n| !n.same_session) || !chain.node(&turn_end)?.1.same_session {
        return Err(HandoffError::UnsupportedTranscript(
            "履歴のsessionIdがHerdrのセッションと一致しません".into(),
        ));
    }

    let mut text = String::new();
    for node in parts.iter().rev() {
        let r = read_at(path, node.offset, node.len)?;
        for block in r["message"]["content"].as_array().into_iter().flatten() {
            if block["type"] == "text" {
                append_block(&mut text, block["text"].as_str().unwrap_or_default());
            }
        }
    }
    if text.trim().is_empty() {
        return Err(HandoffError::NoCompletedAnswer("回答本文が空です".into()));
    }
    Ok(AnswerSnapshot {
        source_fingerprint: fingerprint(&[&session.native_id, &turn_end, &message_id, &text]),
        session: session.clone(),
        answer_id: message_id,
        text,
    })
}

fn interrupted() -> HandoffError {
    HandoffError::NoCompletedAnswer("最新のターンは中断されました".into())
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
                "親子関係が循環しています".into(),
            ));
        }
        let (key, node) = self.nodes.get_key_value(uuid).ok_or_else(|| {
            HandoffError::UnsupportedTranscript("親レコードが見つかりません".into())
        })?;
        Ok((key.as_str(), node))
    }

    fn parent(&mut self, node: &'a Node) -> Result<(&'a str, &'a Node), HandoffError> {
        match node.parent.as_deref() {
            Some(p) => self.node(p),
            None => Err(HandoffError::NoCompletedAnswer(
                "このセッションにはまだ回答がありません".into(),
            )),
        }
    }
}
