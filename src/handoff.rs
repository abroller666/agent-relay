//! Reading the source's answer, checking both panes again right before
//! sending, and sending exactly once.
//!
//! Herdr has no compare-and-send, so a pane can still change between the
//! last check and the send; the checks only narrow that window. A send
//! whose result is unknown is reported as such and never retried.

use crate::adapters::AdapterRegistry;
use crate::codex_daemon::{CodexDaemon, resolve_agent};
use crate::config::Config;
use crate::error::HandoffError;
use crate::herdr::{AgentSnapshot, HerdrApi};
use crate::model::{AnswerSnapshot, PaneBinding};
use crate::prompt::build_prompt;

/// How many past answers are offered to choose from.
pub const MAX_ANSWERS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Herdr accepted the prompt and submitted it. Says nothing about how
    /// the target handles it.
    Accepted,
    /// The prompt may have been submitted; check the target pane.
    DeliveryUnknown,
}

pub struct HandoffService<'a> {
    herdr: &'a dyn HerdrApi,
    adapters: &'a dyn AdapterRegistry,
    config: &'a Config,
    daemon: Option<&'a dyn CodexDaemon>,
}

impl<'a> HandoffService<'a> {
    pub fn new(
        herdr: &'a dyn HerdrApi,
        adapters: &'a dyn AdapterRegistry,
        config: &'a Config,
    ) -> Self {
        Self {
            herdr,
            adapters,
            config,
            daemon: None,
        }
    }

    /// Also identifies Codex panes on the shared app-server daemon by
    /// their titles (see `codex_daemon`).
    pub fn with_codex_daemon(mut self, daemon: &'a dyn CodexDaemon) -> Self {
        self.daemon = Some(daemon);
        self
    }

    /// The agent in `pane_id` with its session binding.
    pub fn agent(&self, pane_id: &str) -> Result<AgentSnapshot, HandoffError> {
        resolve_agent(self.herdr, self.daemon, pane_id)
    }

    /// The last finished answer of `source`, which must still be the same
    /// idle agent session.
    pub fn prepare(&self, source: PaneBinding) -> Result<AnswerSnapshot, HandoffError> {
        let before = self.current_source(&source)?;
        let adapter = self
            .adapters
            .adapter(source.agent)
            .ok_or_else(|| HandoffError::UnsupportedAgent(source.agent.display_name().into()))?;
        let session = adapter.resolve(&source, self.config)?;
        let answer = adapter.latest_completed(&session, &self.config.limits)?;
        // The agent must not have started another turn while it was read.
        let after = self.current_source(&source)?;
        if after.state_change_seq != before.state_change_seq {
            return Err(HandoffError::SourceChanged(
                "the source changed state while its answer was read".into(),
            ));
        }
        Ok(answer)
    }

    /// The finished answers of `source`, newest first, for choosing one.
    pub fn answers(&self, source: PaneBinding) -> Result<Vec<AnswerSnapshot>, HandoffError> {
        let before = self.current_source(&source)?;
        let adapter = self
            .adapters
            .adapter(source.agent)
            .ok_or_else(|| HandoffError::UnsupportedAgent(source.agent.display_name().into()))?;
        let session = adapter.resolve(&source, self.config)?;
        let answers = adapter.completed_answers(&session, &self.config.limits, MAX_ANSWERS)?;
        let after = self.current_source(&source)?;
        if after.state_change_seq != before.state_change_seq {
            return Err(HandoffError::SourceChanged(
                "the source changed state while its answers were read".into(),
            ));
        }
        Ok(answers)
    }

    /// Checks both panes and the answer again, then sends the prompt once.
    pub fn send(
        &self,
        answer: &AnswerSnapshot,
        target: &PaneBinding,
        instruction: &str,
    ) -> Result<SendOutcome, HandoffError> {
        let source = &answer.session.binding;
        if target.pane_id == source.pane_id {
            return Err(HandoffError::TargetChanged(
                "cannot send to the source pane itself".into(),
            ));
        }
        let server = self.herdr.server_key();
        if source.server_key != server || target.server_key != server {
            return Err(HandoffError::TargetChanged(
                "the pane belongs to another Herdr server".into(),
            ));
        }
        let before = self.current_source(source)?;
        let adapter = self
            .adapters
            .adapter(source.agent)
            .ok_or_else(|| HandoffError::UnsupportedAgent(source.agent.display_name().into()))?;
        // Resolve again: a rewind can move the session's history to another
        // file, leaving the rewound answer in the old one.
        let session = adapter.resolve(source, self.config)?;
        let same = |a: &AnswerSnapshot| {
            a.answer_id == answer.answer_id && a.source_fingerprint == answer.source_fingerprint
        };
        if answer.chosen {
            // An answer picked from the history only has to still be part
            // of the conversation; newer answers do not matter.
            let history = adapter.completed_answers(&session, &self.config.limits, usize::MAX)?;
            if !history.iter().any(same) {
                return Err(HandoffError::SourceChanged(
                    "the chosen answer is no longer in the conversation".into(),
                ));
            }
        } else {
            if session.transcript_path != answer.session.transcript_path {
                return Err(HandoffError::SourceChanged(
                    "the source session moved to another transcript (rewind or fork)".into(),
                ));
            }
            let fresh = adapter.latest_completed(&session, &self.config.limits)?;
            if !same(&fresh) {
                return Err(HandoffError::SourceChanged(
                    "the source has a newer answer; review it before sending".into(),
                ));
            }
        }
        // As in `prepare`: the source must not have started another turn
        // while its transcript was read.
        let after = self.current_source(source)?;
        if after.state_change_seq != before.state_change_seq {
            return Err(HandoffError::SourceChanged(
                "the source changed state while its answer was checked".into(),
            ));
        }
        let now = match self.agent(&target.pane_id) {
            Ok(now) => now,
            Err(HandoffError::UnsupportedAgent(_) | HandoffError::SessionUnavailable(_)) => {
                return Err(HandoffError::TargetChanged(
                    "the target agent was replaced".into(),
                ));
            }
            Err(e) => return Err(e),
        };
        if !now.binding.same_occupant(target) {
            return Err(HandoffError::TargetChanged(
                "the target agent was replaced".into(),
            ));
        }
        require_ready(&now, "the target")?;

        let source_name = crate::names::pane_display(self.herdr, source);
        let prompt = build_prompt(
            instruction,
            answer,
            &source_name,
            self.config.prompt_language,
        )?;
        if prompt.len() > self.config.max_payload_bytes {
            return Err(HandoffError::PayloadTooLarge(format!(
                "{} bytes (limit {} bytes)",
                prompt.len(),
                self.config.max_payload_bytes
            )));
        }
        match self.herdr.prompt(&target.pane_id, &prompt) {
            Ok(()) => Ok(SendOutcome::Accepted),
            Err(HandoffError::DeliveryUnknown(_)) => Ok(SendOutcome::DeliveryUnknown),
            Err(e) => Err(e),
        }
    }

    /// `source` as Herdr sees it now, if it is still the same idle session.
    fn current_source(&self, source: &PaneBinding) -> Result<AgentSnapshot, HandoffError> {
        let now = match self.agent(&source.pane_id) {
            Ok(now) => now,
            Err(HandoffError::UnsupportedAgent(_) | HandoffError::SessionUnavailable(_)) => {
                return Err(HandoffError::SourceChanged(
                    "the source agent was replaced".into(),
                ));
            }
            Err(e) => return Err(e),
        };
        if !now.binding.same_occupant(source) {
            return Err(HandoffError::SourceChanged(
                "the source agent was replaced".into(),
            ));
        }
        require_ready(&now, "the source")?;
        Ok(now)
    }
}

/// `role`: "the source" or "the target", for the message.
fn require_ready(agent: &AgentSnapshot, role: &str) -> Result<(), HandoffError> {
    if agent.is_ready() {
        return Ok(());
    }
    let state = if agent.launch_pending {
        "starting"
    } else {
        agent.agent_status.as_str()
    };
    Err(HandoffError::AgentNotReady(format!("{role} is {state}")))
}
