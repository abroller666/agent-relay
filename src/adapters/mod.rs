//! One adapter per agent reads that agent's transcript. Everything outside
//! this module is agent-neutral: it only sees `AnswerSnapshot`s.
//!
//! Adapters are read-only. They never write to a transcript, start an agent
//! or fall back to another session or to the screen.

pub mod claude;

use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::config::{Config, ReadLimits};
use crate::error::HandoffError;
use crate::model::{AgentKind, AnswerSnapshot, PaneBinding, ResolvedSession};

pub trait AnswerAdapter {
    /// The transcript `binding`'s session reference names.
    fn resolve(
        &self,
        binding: &PaneBinding,
        config: &Config,
    ) -> Result<ResolvedSession, HandoffError>;

    /// The final answer of the last turn, if that turn finished normally.
    /// Waits, within `limits.retry_delays`, for a transcript whose end is
    /// still being written.
    fn latest_completed(
        &self,
        session: &ResolvedSession,
        limits: &ReadLimits,
    ) -> Result<AnswerSnapshot, HandoffError>;
}

/// Which adapter reads which agent. Tests substitute their own.
pub trait AdapterRegistry {
    fn adapter(&self, agent: AgentKind) -> Option<&dyn AnswerAdapter>;
}

/// The adapters of this build.
pub struct DefaultAdapters;

impl AdapterRegistry for DefaultAdapters {
    fn adapter(&self, agent: AgentKind) -> Option<&dyn AnswerAdapter> {
        match agent {
            AgentKind::Claude => Some(&claude::ClaudeAdapter),
            AgentKind::Codex => None,
        }
    }
}

/// Runs `attempt` until it gives anything but `CompletionUncertain`,
/// sleeping `delays` in between; the last uncertainty is returned.
pub fn with_retries<T>(
    delays: &[Duration],
    mut attempt: impl FnMut() -> Result<T, HandoffError>,
) -> Result<T, HandoffError> {
    let mut delays = delays.iter();
    loop {
        match attempt() {
            Err(e @ HandoffError::CompletionUncertain(_)) => match delays.next() {
                Some(d) => std::thread::sleep(*d),
                None => return Err(e),
            },
            other => return other,
        }
    }
}

/// A digest naming one answer of one transcript.
pub fn fingerprint(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn retries_only_uncertainty() {
        let calls = Cell::new(0);
        let r: Result<(), _> = with_retries(&[Duration::ZERO; 2], || {
            calls.set(calls.get() + 1);
            Err(HandoffError::CompletionUncertain(String::new()))
        });
        assert!(r.is_err());
        assert_eq!(calls.get(), 3);

        calls.set(0);
        let r: Result<(), _> = with_retries(&[Duration::ZERO; 2], || {
            calls.set(calls.get() + 1);
            Err(HandoffError::NoCompletedAnswer(String::new()))
        });
        assert!(r.is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn fingerprint_parts_do_not_run_together() {
        assert_ne!(fingerprint(&["ab", "c"]), fingerprint(&["a", "bc"]));
    }
}
