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
            "unexpected session id format".into(),
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
            "none"
        } else {
            agent_name
        };
        return Err(HandoffError::UnsupportedAgent(shown.to_string()));
    };
    let Some(session) = info.get("agent_session").filter(|s| !s.is_null()) else {
        return Err(HandoffError::SessionUnavailable(
            "check the Herdr integration and restart the session".to_string(),
        ));
    };
    let s = |key: &str| session.get(key).and_then(Value::as_str).unwrap_or_default();
    let expected_source = format!("herdr:{}", agent.herdr_name());
    if s("source") != expected_source || s("agent") != agent.herdr_name() {
        return Err(HandoffError::SessionUnavailable(
            "the session reference is not from the current agent".to_string(),
        ));
    }
    if s("kind") != "id" {
        return Err(HandoffError::SessionUnavailable(format!(
            "unsupported session reference ({})",
            s("kind")
        )));
    }
    validate_session_id(s("value"))?;
    let terminal_id = str_field("terminal_id").unwrap_or_default();
    let tab_id = str_field("tab_id").unwrap_or_default();
    if pane_id.is_empty() || terminal_id.is_empty() {
        return Err(HandoffError::Herdr("incomplete pane information".into()));
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
            "{id} (the session may not have had its first prompt yet)"
        ))),
        1 => Ok(found.remove(0)),
        n => Err(HandoffError::SessionAmbiguous(format!("{id}: {n} files"))),
    }
}

pub fn too_many(limits: &ReadLimits) -> HandoffError {
    HandoffError::ReadLimitExceeded(format!(
        "more than {} candidate files",
        limits.max_candidates
    ))
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

/// The rollout files of one Codex thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRollouts {
    /// `rollout-<time>-<id>.jsonl`, if present.
    pub base: Option<PathBuf>,
    /// `rollout-<time>-<id>_<segment>.jsonl`, oldest first. A rewind
    /// starts a new segment; later turns go to the newest one.
    pub segments: Vec<(String, PathBuf)>,
}

impl CodexRollouts {
    /// The file the thread's current history ends in.
    pub fn active(&self) -> &Path {
        match self.segments.last() {
            Some((_, path)) => path,
            None => self.base.as_deref().expect("rollouts without files"),
        }
    }
}

/// The rollouts of Codex thread `id` under `<root>/YYYY/MM/DD/`.
pub fn find_codex_rollouts(
    roots: &[PathBuf],
    id: &str,
    limits: &ReadLimits,
) -> Result<CodexRollouts, HandoffError> {
    validate_session_id(id)?;
    let mut seen = 0;
    let mut bases = Vec::new();
    let mut segments: Vec<(String, PathBuf)> = Vec::new();
    for root in roots {
        for year in subdirs(root)? {
            for month in subdirs(&year)? {
                for day in subdirs(&month)? {
                    let entries = std::fs::read_dir(&day).map_err(|e| {
                        HandoffError::TranscriptUnavailable(format!("{}: {e}", day.display()))
                    })?;
                    for entry in entries.filter_map(Result::ok) {
                        seen += 1;
                        if seen > limits.max_candidates {
                            return Err(too_many(limits));
                        }
                        let path = entry.path();
                        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                            continue;
                        };
                        match rollout_segment(name, id) {
                            Some(None) => bases.push(path),
                            Some(Some(segment)) => segments.push((segment, path)),
                            None => {}
                        }
                    }
                }
            }
        }
    }
    if bases.len() > 1 {
        return Err(HandoffError::SessionAmbiguous(format!(
            "{id}: {} files",
            bases.len()
        )));
    }
    segments.sort();
    if segments.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(HandoffError::SessionAmbiguous(format!(
            "{id}: duplicate segments"
        )));
    }
    let base = bases.pop();
    if base.is_none() && segments.is_empty() {
        return Err(unique(Vec::new(), id).unwrap_err());
    }
    Ok(CodexRollouts { base, segments })
}

/// For a rollout file of thread `id`: Some(None) for its base file,
/// Some(Some(segment)) for a segment, None for any other file.
pub fn rollout_segment(name: &str, id: &str) -> Option<Option<String>> {
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    // `YYYY-MM-DDTHH-MM-SS-<id>[_<segment>]`
    let (time, rest) = (stem.get(..19)?, stem.get(19..)?.strip_prefix('-')?);
    if time.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    if rest == id {
        return Some(None);
    }
    let segment = rest.strip_prefix(id)?.strip_prefix('_')?;
    validate_session_id(segment).ok()?;
    Some(Some(segment.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01a118e3-a882-7d10-9675-afffb81169d9";

    #[test]
    fn rollout_names() {
        assert_eq!(
            rollout_segment(&format!("rollout-2026-10-08T09-22-15-{ID}.jsonl"), ID),
            Some(None)
        );
        assert_eq!(
            rollout_segment(
                &format!(
                    "rollout-2026-10-08T09-26-33-{ID}_01a118e7-96eb-7db2-a456-fd93d22a289f.jsonl"
                ),
                ID
            ),
            Some(Some("01a118e7-96eb-7db2-a456-fd93d22a289f".into()))
        );
        // Another thread whose id ends the same way.
        assert_eq!(
            rollout_segment(
                &format!("rollout-2026-10-08T09-22-15-{ID}.jsonl"),
                "afffb81169d9"
            ),
            None
        );
        assert_eq!(rollout_segment(&format!("{ID}.jsonl"), ID), None);
        assert_eq!(
            rollout_segment(&format!("rollout-x-{ID}_../a.jsonl"), ID),
            None
        );
    }
}
