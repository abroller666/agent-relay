//! From Herdr's report of a pane to the one transcript file it names.
//! Nothing here guesses: a missing, unofficial or mismatched session
//! reference is an error, and so is a reference that matches two files.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::ReadLimits;
use crate::error::HandoffError;
use crate::model::{AgentKind, PaneBinding, SessionRef};

/// A session id safe to use as part of a file name: hex digits and dashes,
/// as Claude Code and Codex write them.
pub fn validate_session_id(id: &str) -> Result<(), HandoffError> {
    let ok = (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(HandoffError::SessionUnavailable(
            "セッションIDの形式が想定外です".into(),
        ))
    }
}

/// The binding for an `AgentInfo` object from `agent.get` / `agent.list`.
pub fn binding_from_agent(info: &Value, server_key: &str) -> Result<PaneBinding, HandoffError> {
    let str_field = |key: &str| info.get(key).and_then(Value::as_str);
    let pane_id = str_field("pane_id").unwrap_or_default();
    let agent_name = str_field("agent").unwrap_or_default();
    let Some(agent) = AgentKind::from_herdr(agent_name) else {
        let shown = if agent_name.is_empty() {
            "なし"
        } else {
            agent_name
        };
        return Err(HandoffError::UnsupportedAgent(format!(
            "{pane_id}: {shown}"
        )));
    };
    let Some(session) = info.get("agent_session").filter(|s| !s.is_null()) else {
        return Err(HandoffError::SessionUnavailable(format!(
            "{pane_id}: Herdr integrationを確認し、セッションを開始し直してください"
        )));
    };
    let s = |key: &str| session.get(key).and_then(Value::as_str).unwrap_or_default();
    let expected_source = format!("herdr:{}", agent.herdr_name());
    if s("source") != expected_source || s("agent") != agent.herdr_name() {
        return Err(HandoffError::SessionUnavailable(format!(
            "{pane_id}: セッション参照が現在のエージェントのものではありません"
        )));
    }
    if s("kind") != "id" {
        return Err(HandoffError::SessionUnavailable(format!(
            "{pane_id}: 未対応のセッション参照（{}）",
            s("kind")
        )));
    }
    validate_session_id(s("value"))?;
    let terminal_id = str_field("terminal_id").unwrap_or_default();
    let tab_id = str_field("tab_id").unwrap_or_default();
    if pane_id.is_empty() || terminal_id.is_empty() {
        return Err(HandoffError::Herdr("pane情報が不完全です".into()));
    }
    Ok(PaneBinding {
        server_key: server_key.to_string(),
        pane_id: pane_id.to_string(),
        terminal_id: terminal_id.to_string(),
        tab_id: tab_id.to_string(),
        agent,
        session: SessionRef {
            kind: "id".into(),
            value: s("value").to_string(),
        },
    })
}

/// The transcript `<root>/<project>/<id>.jsonl` of a Claude Code session,
/// which must exist under exactly one project of all `roots`.
pub fn find_claude_transcript(
    roots: &[PathBuf],
    id: &str,
    limits: &ReadLimits,
) -> Result<PathBuf, HandoffError> {
    validate_session_id(id)?;
    let name = format!("{id}.jsonl");
    let mut seen = 0;
    let mut found = Vec::new();
    for root in roots {
        for project in subdirs(root)? {
            seen += 1;
            if seen > limits.max_candidates {
                return Err(too_many(limits));
            }
            let path = project.join(&name);
            if path.is_file() {
                found.push(path);
            }
        }
    }
    unique(found, id)
}

/// Exactly one of `found`, or the error saying why not.
pub fn unique(mut found: Vec<PathBuf>, id: &str) -> Result<PathBuf, HandoffError> {
    match found.len() {
        0 => Err(HandoffError::TranscriptUnavailable(format!(
            "{id}（まだ最初の発言をしていないセッションの可能性があります）"
        ))),
        1 => Ok(found.remove(0)),
        n => Err(HandoffError::SessionAmbiguous(format!("{id}: {n}件"))),
    }
}

pub fn too_many(limits: &ReadLimits) -> HandoffError {
    HandoffError::ReadLimitExceeded(format!("探索候補が{}件を超えました", limits.max_candidates))
}

/// The directories directly inside `dir` (none if `dir` does not exist),
/// without following symbolic links out of it.
pub fn subdirs(dir: &Path) -> Result<Vec<PathBuf>, HandoffError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(HandoffError::TranscriptUnavailable(format!(
                "{}: {e}",
                dir.display()
            )));
        }
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    Ok(dirs)
}
