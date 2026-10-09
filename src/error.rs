//! Every way a handoff can stop. Each variant carries a short detail for
//! the popup; none ever carries answer text, instructions or transcript
//! content.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffError {
    /// The pane runs no agent, or one without an adapter.
    UnsupportedAgent(String),
    /// Herdr has no usable session reference for the pane.
    SessionUnavailable(String),
    /// More than one transcript matches the session reference.
    SessionAmbiguous(String),
    /// Two Codex threads in the pane's directory carry the name in its
    /// title, so the title cannot tell which one the pane shows (the name).
    ThreadNameShared(String),
    /// No transcript matches the session reference.
    TranscriptUnavailable(String),
    /// The transcript uses a shape this adapter has not been verified on.
    UnsupportedTranscript(String),
    /// The latest turn may still be running, or its end cannot be proven.
    CompletionUncertain(String),
    /// The latest turn ended without an answer (interrupted, failed, empty).
    NoCompletedAnswer(String),
    /// A finished line of the transcript is not valid JSON.
    TranscriptCorrupt(String),
    /// A file, line or search went past a configured limit.
    ReadLimitExceeded(String),
    /// The source's answer changed since it was read.
    SourceChanged(String),
    /// The target pane is no longer the agent session that was chosen.
    TargetChanged(String),
    /// A pane is busy, blocked, starting or in an unknown state.
    AgentNotReady(String),
    /// The prompt is larger than the configured or protocol limit.
    PayloadTooLarge(String),
    /// The instruction cannot be sent as typed.
    InvalidInstruction(String),
    /// The prompt may or may not have reached the target.
    DeliveryUnknown(String),
    /// Herdr could not be reached or refused the request before any input
    /// was written.
    Herdr(String),
}

impl HandoffError {
    /// Whether retrying the same handoff could succeed without the user
    /// changing anything (the transcript may simply not be written yet).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::CompletionUncertain(_) | Self::SourceChanged(_) | Self::Herdr(_)
        )
    }
}

impl fmt::Display for HandoffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (what, detail) = match self {
            Self::UnsupportedAgent(d) => ("Unsupported agent", d),
            Self::SessionUnavailable(d) => ("No session registered in Herdr", d),
            Self::SessionAmbiguous(d) => ("More than one transcript matches the session", d),
            Self::ThreadNameShared(name) => {
                return write!(
                    f,
                    "Run /rename in one of the Codex panes: another Codex thread in this directory is also named {name:?}, and the screen does not show which one this is"
                );
            }
            Self::TranscriptUnavailable(d) => ("Session transcript not found", d),
            Self::UnsupportedTranscript(d) => ("Unsupported transcript format", d),
            Self::CompletionUncertain(d) => ("Cannot confirm the answer is finished", d),
            Self::NoCompletedAnswer(d) => ("No finished answer", d),
            Self::TranscriptCorrupt(d) => ("Transcript is corrupt", d),
            Self::ReadLimitExceeded(d) => ("Read limit exceeded", d),
            Self::SourceChanged(d) => ("The answer was updated", d),
            Self::TargetChanged(d) => ("The target changed", d),
            Self::AgentNotReady(d) => ("Agent is not ready", d),
            Self::PayloadTooLarge(d) => ("Prompt is too large", d),
            Self::InvalidInstruction(d) => ("Cannot send the instruction", d),
            Self::DeliveryUnknown(d) => ("Delivery could not be confirmed", d),
            Self::Herdr(d) => ("Herdr request failed", d),
        };
        if detail.is_empty() {
            f.write_str(what)
        } else {
            write!(f, "{what}: {detail}")
        }
    }
}

impl std::error::Error for HandoffError {}
