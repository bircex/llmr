//! What goes into a conversation and what comes back.

use serde::{Deserialize, Serialize};

/// Who said something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// You, or your user.
    User,
    /// The model.
    Assistant,
}

/// One piece of a message.
///
/// A message is a list of these rather than a string, because a model's turn can contain
/// reasoning, text and tool calls at once, and flattening them into one string loses which
/// was which.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContentBlock {
    /// Ordinary text.
    Text(String),

    /// Reasoning the model did before answering.
    ///
    /// The text may be empty when the provider chooses not to show it. That is not an
    /// error, and the block still matters.
    ///
    /// The signature is the provider's proof that it produced this block. Hand it back
    /// unchanged when you continue the conversation. A thinking block replayed without its
    /// signature is rejected on arrival, and one dropped from the history changes the turn
    /// the model sees.
    Thinking {
        /// The reasoning, when the provider shows it.
        text: String,
        /// The provider's proof it produced this. Pass it back untouched.
        signature: Option<String>,
    },

    /// The model asking for a tool to be run.
    ToolUse {
        /// The provider's identifier for this call. Your result must quote it back.
        id: String,
        /// Which tool.
        name: String,
        /// The arguments, as the model produced them.
        input: serde_json::Value,
    },

    /// A block this crate does not model.
    ///
    /// A redacted reasoning blob, a server side tool result, something a vendor added after
    /// this code was written. Kept **verbatim** so a turn can be replayed to the provider
    /// that produced it without loss.
    ///
    /// This is not an answer and must never be read as one. Dropping it silently corrupts a
    /// conversation, because the provider checks what it sent you against what you send
    /// back. Interpreting it would be a guess about a format nobody here has seen.
    Opaque {
        /// The provider's own name for this kind of block.
        kind: String,
        /// Exactly what arrived.
        raw: serde_json::Value,
    },

    /// Your answer to a [`ContentBlock::ToolUse`].
    ToolResult {
        /// The id from the call this answers.
        tool_use_id: String,
        /// What the tool produced.
        content: String,
        /// Whether the tool failed. A failure the model is told about is one it can work
        /// around; a failure hidden in the content reads as data.
        is_error: bool,
    },
}

impl ContentBlock {
    /// The text a caller may read as the answer.
    ///
    /// `None` for everything else. Reasoning and opaque blocks are not answers however
    /// much they look like prose, and a single `text()` that returned both is a method
    /// somebody uses to fill a screen with the model's private working out.
    pub fn answer_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The reasoning in this block, when it is a thinking block.
    ///
    /// Separate from [`ContentBlock::answer_text`] on purpose. Asking for reasoning is a
    /// deliberate act, and it should read like one at the call site.
    pub fn reasoning(&self) -> Option<&str> {
        match self {
            ContentBlock::Thinking { text, .. } => Some(text),
            _ => None,
        }
    }
}

/// One turn in a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who is speaking.
    pub role: Role,
    /// What they said, in order.
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// A user turn containing one piece of text.
    pub fn user(text: impl Into<String>) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    /// An assistant turn containing one piece of text.
    pub fn assistant(text: impl Into<String>) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    /// Every text block joined with newlines. Tool calls are skipped.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::answer_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every tool the model asked for in this turn.
    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .collect()
    }
}

/// Why the model stopped.
///
/// The last three are failures wearing the shape of a result. A caller that treats every
/// stop reason as success will hand a truncated answer to a user and call it done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StopReason {
    /// The model finished what it was saying.
    EndTurn,
    /// The model wants a tool run before it continues.
    ToolUse,
    /// A sequence you asked it to stop at.
    StopSequence,
    /// The output limit was reached. The answer is cut off.
    MaxTokens,
    /// The provider declined to continue.
    Refusal,
    /// A server side tool loop hit its own limit. Resumable, and capped.
    ///
    /// Not a failure and not a finished answer. Send the turn back to continue it.
    PauseTurn,
    /// The conversation no longer fits. Nothing will fix this except sending less.
    ContextWindowExceeded,
    /// A streamed reply stopped arriving before the model said it was done.
    ///
    /// Not something a provider reports — it is what this crate knows when a stream ends
    /// without a stop reason. It lives here, beside the reasons a provider does give,
    /// because [`StopReason::is_complete`] is the guard callers already check, and a
    /// separate channel for "the stream broke" is one they can forget to look at while
    /// rendering half an answer as finished.
    ///
    /// What arrived is still real. See [`crate::chat::stream::Transcript`].
    Interrupted,
    /// The provider reported a reason this crate does not know.
    ///
    /// Kept rather than mapped to something close, because a stop reason invented on the
    /// caller's behalf is worse than one it can look up.
    Other,
}

impl StopReason {
    /// Whether the answer is complete.
    ///
    /// A tool call is complete: the model said what it wanted and is waiting. A truncation
    /// or a refusal is not.
    pub fn is_complete(self) -> bool {
        matches!(
            self,
            StopReason::EndTurn | StopReason::ToolUse | StopReason::StopSequence
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_truncated_answer_is_not_complete() {
        assert!(!StopReason::MaxTokens.is_complete());
        assert!(!StopReason::Refusal.is_complete());
    }

    #[test]
    fn a_paused_turn_is_not_a_finished_one() {
        // Resumable, which is neither a failure nor an answer. A caller that read it as
        // complete would show a user half of what the model was saying.
        assert!(!StopReason::PauseTurn.is_complete());
        assert!(!StopReason::ContextWindowExceeded.is_complete());
    }

    #[test]
    fn a_block_this_crate_does_not_know_survives_a_round_trip() {
        // The property a conversation depends on. A provider checks what it sent against
        // what comes back, so a dropped block is a rejected continuation.
        let block = ContentBlock::Opaque {
            kind: "redacted_thinking".into(),
            raw: serde_json::json!({ "type": "redacted_thinking", "data": "EroBCkY..." }),
        };
        let json = serde_json::to_string(&block).unwrap_or_default();
        let back: ContentBlock =
            serde_json::from_str(&json).unwrap_or(ContentBlock::Text("lost".into()));
        assert_eq!(block, back);
        assert_eq!(
            block.answer_text(),
            None,
            "an opaque block is not an answer"
        );
    }

    #[test]
    fn waiting_for_a_tool_is_complete() {
        assert!(StopReason::ToolUse.is_complete());
    }

    #[test]
    fn reasoning_is_not_an_answer() {
        // The trap this pair exists to remove. One method returning both is one a caller
        // uses to put the model's private working out on a screen.
        let thinking = ContentBlock::Thinking {
            text: "the user probably means".into(),
            signature: None,
        };
        assert_eq!(thinking.answer_text(), None);
        assert_eq!(thinking.reasoning(), Some("the user probably means"));

        let answer = ContentBlock::Text("Here it is.".into());
        assert_eq!(answer.answer_text(), Some("Here it is."));
        assert_eq!(answer.reasoning(), None);
    }

    #[test]
    fn joining_text_leaves_out_the_reasoning() {
        let turn = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    text: "let me work this out".into(),
                    signature: None,
                },
                ContentBlock::Text("Four.".into()),
            ],
        };
        assert_eq!(turn.text(), "Four.");
    }

    #[test]
    fn joining_text_leaves_out_the_tool_calls() {
        let turn = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text("Looking that up.".into()),
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "search".into(),
                    input: serde_json::json!({ "q": "rust" }),
                },
            ],
        };
        assert_eq!(turn.text(), "Looking that up.");
        assert_eq!(turn.tool_calls().len(), 1);
    }

    #[test]
    fn a_thinking_block_keeps_its_signature_through_a_round_trip() {
        // The property that makes a multi turn conversation with reasoning work at all.
        let block = ContentBlock::Thinking {
            text: "considering".into(),
            signature: Some("sig-abc".into()),
        };
        let json = serde_json::to_string(&block).unwrap_or_default();
        let back: ContentBlock = serde_json::from_str(&json)
            .unwrap_or(ContentBlock::Text("deserialization failed".into()));
        assert_eq!(block, back);
    }
}
