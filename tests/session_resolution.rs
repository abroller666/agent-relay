//! Task 1: a pane's session reference resolves to exactly one transcript,
//! or to an error; never to a guess.

use std::fs;
use std::path::Path;

use agent_relay::config::ReadLimits;
use agent_relay::error::HandoffError;
use agent_relay::model::AgentKind;
use agent_relay::session::{binding_from_agent, find_claude_transcript};
use serde_json::json;

const ID_A: &str = "c07c63dd-285d-4412-bb32-5654dd417828";
const ID_B: &str = "a5498f9c-a941-4f63-99a7-c81327440d7a";

fn agent_info(agent: &str, session: Option<serde_json::Value>) -> serde_json::Value {
    let mut info = json!({
        "agent": agent,
        "agent_status": "idle",
        "pane_id": "w4:p1",
        "tab_id": "w4:t1",
        "workspace_id": "w4",
        "terminal_id": "term_1",
        "focused": false,
        "revision": 1,
        "state_change_seq": 7,
    });
    if let Some(s) = session {
        info["agent_session"] = s;
    }
    info
}

fn session(agent: &str, value: &str) -> serde_json::Value {
    json!({"source": format!("herdr:{agent}"), "agent": agent, "kind": "id", "value": value})
}

fn touch(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "{}\n").unwrap();
}

#[test]
fn same_cwd_uses_exact_session_id() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("-work");
    touch(&project.join(format!("{ID_A}.jsonl")));
    touch(&project.join(format!("{ID_B}.jsonl")));

    let binding =
        binding_from_agent(&agent_info("claude", Some(session("claude", ID_A))), "srv").unwrap();
    assert_eq!(binding.agent, AgentKind::Claude);
    assert_eq!(binding.session.value, ID_A);
    assert_eq!(binding.terminal_id, "term_1");

    let found = find_claude_transcript(
        &[root.path().to_path_buf()],
        &binding.session.value,
        &ReadLimits::default(),
    )
    .unwrap();
    assert_eq!(found, project.join(format!("{ID_A}.jsonl")));
}

#[test]
fn missing_session_ref_is_error() {
    let err = binding_from_agent(&agent_info("claude", None), "srv").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn duplicate_id_is_ambiguous() {
    let root = tempfile::tempdir().unwrap();
    touch(&root.path().join("-work").join(format!("{ID_A}.jsonl")));
    touch(&root.path().join("-other").join(format!("{ID_A}.jsonl")));

    let err = find_claude_transcript(&[root.path().to_path_buf()], ID_A, &ReadLimits::default())
        .unwrap_err();
    assert!(matches!(err, HandoffError::SessionAmbiguous(_)), "{err:?}");
}

#[test]
fn absent_transcript_is_unavailable() {
    let root = tempfile::tempdir().unwrap();
    touch(&root.path().join("-work").join(format!("{ID_B}.jsonl")));
    let err = find_claude_transcript(&[root.path().to_path_buf()], ID_A, &ReadLimits::default())
        .unwrap_err();
    assert!(
        matches!(err, HandoffError::TranscriptUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn session_id_cannot_escape_the_root() {
    let root = tempfile::tempdir().unwrap();
    for bad in ["../x", "*", "a/b", ""] {
        let info = agent_info("claude", Some(session("claude", bad)));
        assert!(
            binding_from_agent(&info, "srv").is_err(),
            "accepted session id {bad:?}"
        );
    }
    let err = find_claude_transcript(&[root.path().to_path_buf()], "../x", &ReadLimits::default())
        .unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn unofficial_or_mismatched_session_source_is_rejected() {
    let spoofed = json!({"source": "someone", "agent": "claude", "kind": "id", "value": ID_A});
    let err = binding_from_agent(&agent_info("claude", Some(spoofed)), "srv").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );

    // The pane now runs Codex but still carries a Claude session reference.
    let stale = session("claude", ID_A);
    let err = binding_from_agent(&agent_info("codex", Some(stale)), "srv").unwrap_err();
    assert!(
        matches!(err, HandoffError::SessionUnavailable(_)),
        "{err:?}"
    );
}

#[test]
fn other_agents_are_unsupported() {
    let info = agent_info("gemini", Some(session("gemini", ID_A)));
    let err = binding_from_agent(&info, "srv").unwrap_err();
    assert!(matches!(err, HandoffError::UnsupportedAgent(_)), "{err:?}");
}

#[test]
fn too_many_candidates_is_a_limit_error() {
    let root = tempfile::tempdir().unwrap();
    for i in 0..5 {
        touch(&root.path().join(format!("-p{i}")).join("x.jsonl"));
    }
    let limits = ReadLimits {
        max_candidates: 3,
        ..ReadLimits::default()
    };
    let err = find_claude_transcript(&[root.path().to_path_buf()], ID_A, &limits).unwrap_err();
    assert!(matches!(err, HandoffError::ReadLimitExceeded(_)), "{err:?}");
}
