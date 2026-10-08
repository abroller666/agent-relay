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
            Self::UnsupportedAgent(d) => ("対応していないエージェントです", d),
            Self::SessionUnavailable(d) => ("Herdrにセッションが登録されていません", d),
            Self::SessionAmbiguous(d) => ("セッションの履歴が複数見つかりました", d),
            Self::TranscriptUnavailable(d) => ("セッションの履歴が見つかりません", d),
            Self::UnsupportedTranscript(d) => ("未対応の履歴形式です", d),
            Self::CompletionUncertain(d) => ("回答の完了を確認できません", d),
            Self::NoCompletedAnswer(d) => ("完了した回答がありません", d),
            Self::TranscriptCorrupt(d) => ("履歴が壊れています", d),
            Self::ReadLimitExceeded(d) => ("読み取り上限を超えました", d),
            Self::SourceChanged(d) => ("回答が更新されました", d),
            Self::TargetChanged(d) => ("送信先が変わりました", d),
            Self::AgentNotReady(d) => ("エージェントが受付可能な状態ではありません", d),
            Self::PayloadTooLarge(d) => ("送信サイズが上限を超えています", d),
            Self::InvalidInstruction(d) => ("指示を送れません", d),
            Self::DeliveryUnknown(d) => ("送信結果を確認できません", d),
            Self::Herdr(d) => ("Herdrとの通信に失敗しました", d),
        };
        if detail.is_empty() {
            f.write_str(what)
        } else {
            write!(f, "{what}：{detail}")
        }
    }
}

impl std::error::Error for HandoffError {}
