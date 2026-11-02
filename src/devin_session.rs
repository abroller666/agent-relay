//! Which Devin CLI session a pane runs.
//!
//! Herdr's Devin integration registers the session Devin starts with but
//! does not follow `/clear`, which starts another session in the same pane
//! (verified on Devin 3000.11.234 with integration v3). The `devin` TUI runs
//! each session in a child process (`devin acp`) that writes its pid to
//! `<root>/session_locks/<session id>.lock` and keeps it open; `/clear`
//! replaces that child. So the session of a pane is the one whose lock
//! names a Devin process in the pane's foreground, and Herdr's report is
//! not used. No such lock, or two, is an error.

use std::path::Path;

use serde_json::{Value, json};

use crate::config::Config;
use crate::error::HandoffError;
use crate::herdr::{AgentSnapshot, HerdrApi, PaneProcess};
use crate::session::validate_devin_session_id;

/// The Devin agent in `pane_id`, bound to the session its process holds;
/// None when the pane runs another agent.
pub fn resolve_devin(
    herdr: &dyn HerdrApi,
    config: &Config,
    pane_id: &str,
) -> Result<Option<AgentSnapshot>, HandoffError> {
    let mut info = herdr.agent_info(pane_id)?;
    if info["agent"] != "devin" {
        return Ok(None);
    }
    let unseen = || {
        HandoffError::SessionUnavailable(
            "cannot see which Devin session this pane runs; restart Devin in this pane".into(),
        )
    };
    let processes = herdr.foreground_processes(pane_id).map_err(|_| unseen())?;
    let pids: Vec<u32> = processes
        .iter()
        .filter(|p| is_devin(p))
        .map(|p| p.pid)
        .collect();
    if pids.is_empty() {
        return Err(unseen());
    }
    let id = held_session(&config.devin_roots, &pids, config.limits.max_candidates)?
        .ok_or_else(unseen)?;
    info["agent_session"] = session_ref(&id);
    AgentSnapshot::from_info(&info, &herdr.server_key()).map(Some)
}

fn session_ref(id: &str) -> Value {
    json!({"source": "herdr:devin", "agent": "devin", "kind": "id", "value": id})
}

fn is_devin(p: &PaneProcess) -> bool {
    p.argv.first().is_some_and(|a| {
        Path::new(a)
            .file_name()
            .is_some_and(|n| n.to_string_lossy() == "devin")
    })
}

/// The session whose lock under one of `roots` names one of `pids`.
pub fn held_session(
    roots: &[std::path::PathBuf],
    pids: &[u32],
    max_candidates: usize,
) -> Result<Option<String>, HandoffError> {
    let mut found = Vec::new();
    let mut seen = 0;
    for root in roots {
        let dir = root.join("session_locks");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(HandoffError::SessionUnavailable(format!(
                    "{}: {e}",
                    dir.display()
                )));
            }
        };
        for entry in entries.filter_map(Result::ok) {
            seen += 1;
            if seen > max_candidates {
                return Err(HandoffError::ReadLimitExceeded(format!(
                    "more than {max_candidates} candidate files"
                )));
            }
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".lock"))
            else {
                continue;
            };
            if validate_devin_session_id(id).is_err() {
                continue;
            }
            // A pid and a newline; anything longer is not a lock.
            let Ok(text) = std::fs::read(&path) else {
                continue;
            };
            let pid = (text.len() <= 32)
                .then(|| String::from_utf8(text).ok())
                .flatten()
                .and_then(|t| t.trim().parse::<u32>().ok());
            if pid.is_some_and(|pid| pids.contains(&pid)) {
                found.push(id.to_string());
            }
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        n => Err(HandoffError::SessionAmbiguous(format!(
            "the pane's Devin holds {n} sessions"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_naming_a_pane_process_is_the_session() {
        let root = tempfile::tempdir().unwrap();
        let locks = root.path().join("session_locks");
        std::fs::create_dir_all(&locks).unwrap();
        // The session before /clear: its process has exited.
        std::fs::write(locks.join("scratch-earthquake.lock"), "70520\n").unwrap();
        std::fs::write(locks.join("harmless-hourglass.lock"), "71727\n").unwrap();
        std::fs::write(locks.join("cookie-grin.lock"), "46829\n").unwrap();
        std::fs::write(locks.join("not-a-lock.txt"), "71727\n").unwrap();
        let roots = [root.path().to_path_buf()];
        assert_eq!(
            held_session(&roots, &[70519, 71727], 100).unwrap(),
            Some("harmless-hourglass".into())
        );
        assert_eq!(held_session(&roots, &[1], 100).unwrap(), None);
        std::fs::write(locks.join("other-session.lock"), "70519").unwrap();
        assert!(matches!(
            held_session(&roots, &[70519, 71727], 100),
            Err(HandoffError::SessionAmbiguous(_))
        ));
        assert!(matches!(
            held_session(&roots, &[1], 2),
            Err(HandoffError::ReadLimitExceeded(_))
        ));
    }

    #[test]
    fn only_devin_processes_count() {
        let p = |argv: &[&str]| PaneProcess {
            pid: 1,
            argv: argv.iter().map(|s| s.to_string()).collect(),
        };
        assert!(is_devin(&p(&["/Users/me/.local/bin/devin", "acp"])));
        assert!(is_devin(&p(&["devin"])));
        assert!(!is_devin(&p(&["devin-other"])));
        assert!(!is_devin(&p(&["zsh"])));
    }
}
