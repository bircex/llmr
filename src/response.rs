//! What a model gives back.

use crate::message::{ContentBlock, Message, StopReason};
use crate::model::ModelId;
use crate::usage::Usage;
use serde::{Deserialize, Serialize};

/// One reply from one model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChatResponse {
    /// The model's turn.
    pub message: Message,
    /// Why it stopped. Check [`StopReason::is_complete`] before you use the text.
    pub stop_reason: StopReason,
    /// What the call consumed, as far as the provider reported it.
    pub usage: Usage,
    /// Which model actually served this.
    ///
    /// Can differ from the one you asked for. Providers alias names, and some fall back to
    /// a different model under load. Price against this one, not against your request.
    pub model: ModelId,
}

impl ChatResponse {
    /// A reply.
    ///
    /// This type is marked non exhaustive so fields can be added without breaking your
    /// code, which also means you cannot build one with a struct literal. This is how a
    /// provider outside this crate builds its answer.
    ///
    /// Pass [`Usage::absent`] when the provider reported nothing. Do not pass zeros: an
    /// unknown cost written as zero becomes a free call in every report that adds it up.
    pub fn new(message: Message, stop_reason: StopReason, usage: Usage, model: ModelId) -> Self {
        Self {
            message,
            stop_reason,
            usage,
            model,
        }
    }

    /// The reply as plain text, with tool calls left out.
    pub fn text(&self) -> String {
        self.message.text()
    }

    /// The tools the model asked to have run.
    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.message.tool_calls()
    }

    /// Whether the answer is whole.
    ///
    /// A reply that hit the output limit is not, and it arrives with a status code that
    /// says everything went fine.
    pub fn is_complete(&self) -> bool {
        self.stop_reason.is_complete()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Role;

    fn reply(stop: StopReason) -> ChatResponse {
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text("half an answ".into())],
            },
            stop_reason: stop,
            usage: Usage::absent(),
            model: "m".into(),
        }
    }

    #[test]
    fn a_truncated_reply_says_so_even_though_the_call_succeeded() {
        assert!(!reply(StopReason::MaxTokens).is_complete());
        assert!(reply(StopReason::EndTurn).is_complete());
    }
}
