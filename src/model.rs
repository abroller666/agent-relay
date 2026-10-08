//! The contract between Herdr, the answer adapters and the UI.
//! Text is always UTF-8 and kept exactly as the agent wrote it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    /// The agent name Herdr reports (`agent.get` `agent`).
    pub fn from_herdr(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn herdr_name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// Herdr's reference to the agent's native session (`agent_session`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    pub kind: String,
    pub value: String,
}

/// One agent session in one pane, as Herdr reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneBinding {
    /// The Herdr server, as its normalized socket path.
    pub server_key: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub tab_id: String,
    pub agent: AgentKind,
    pub session: SessionRef,
}

impl PaneBinding {
    /// Whether `other` is the same agent session in the same terminal.
    pub fn same_occupant(&self, other: &PaneBinding) -> bool {
        self == other
    }
}

/// A binding matched to the transcript that belongs to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSession {
    pub binding: PaneBinding,
    /// The session id as the transcript records it.
    pub native_id: String,
    pub transcript_path: PathBuf,
}

/// The last finished answer of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerSnapshot {
    pub session: ResolvedSession,
    /// The native message id of the answer.
    pub answer_id: String,
    pub text: String,
    /// Identifies this answer in this transcript: changes when a newer
    /// answer replaces it or its text changes.
    pub source_fingerprint: String,
    /// When the turn ended, as the transcript records it (RFC 3339).
    #[serde(default)]
    pub finished_at: Option<String>,
    /// Picked by the user from the history rather than taken as the
    /// latest: sending it does not stop when a newer answer appears.
    #[serde(default)]
    pub chosen: bool,
}
