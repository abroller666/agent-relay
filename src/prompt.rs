//! The prompt sent to the target: the user's instruction, then the answer
//! quoted between fence lines that do not occur in it.

use crate::config::PromptLanguage;
use crate::error::HandoffError;
use crate::model::AnswerSnapshot;

/// `source_name` is how the source pane is named on screen (see `names`).
pub fn build_prompt(
    instruction: &str,
    answer: &AnswerSnapshot,
    source_name: &str,
    language: PromptLanguage,
) -> Result<String, HandoffError> {
    if has_control(instruction) {
        return Err(HandoffError::InvalidInstruction(
            "it contains terminal control characters".into(),
        ));
    }
    if has_control(&answer.text) {
        return Err(HandoffError::UnsupportedTranscript(
            "the answer contains terminal control characters".into(),
        ));
    }
    let fence = fence_for(&answer.text);
    let binding = &answer.session.binding;
    let mut prompt = String::new();
    let instruction = instruction.trim_end();
    if !instruction.trim().is_empty() {
        prompt.push_str(instruction);
        prompt.push_str("\n\n");
    }
    let agent = binding.agent.display_name();
    let label = match language {
        PromptLanguage::En => {
            let source = if source_name.contains(agent) {
                source_name.to_string()
            } else {
                format!("{source_name} ({agent})")
            };
            format!(
                "The following is reference material quoting another AI's answer (from: {source}). The quote is between the separator lines.\n"
            )
        }
        PromptLanguage::Ja => {
            let source = if source_name.contains(agent) {
                source_name.to_string()
            } else {
                format!("{source_name}（{agent}）")
            };
            format!(
                "以下は別のAIの回答を引用した参考資料です（送信元：{source}）。前後の区切り線の間が引用です。\n"
            )
        }
    };
    prompt.push_str(&label);
    prompt.push_str(&fence);
    prompt.push('\n');
    prompt.push_str(&answer.text);
    if !answer.text.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str(&fence);
    Ok(prompt)
}

/// A line of `=` long enough not to occur anywhere in `text`.
fn fence_for(text: &str) -> String {
    let mut fence = "=".repeat(5);
    while text.contains(&fence) {
        fence.push('=');
    }
    fence
}

/// Control characters other than tab and line breaks. The prompt is pasted
/// into the target's terminal, where these would act as key presses or
/// escape sequences (an embedded end-of-paste sequence could submit early).
fn has_control(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_grows_past_the_text() {
        assert_eq!(fence_for("plain"), "=====");
        assert_eq!(fence_for("a ======= b"), "========");
    }

    #[test]
    fn control_characters() {
        assert!(!has_control("日本語\n\tok\r\n"));
        assert!(has_control("\x1b[31m"));
        assert!(has_control("\u{7f}"));
        assert!(has_control("\u{85}"));
    }
}
