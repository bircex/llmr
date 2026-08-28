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
    /// The text of this block, when it has any.
    ///
    /// A thinking block returns its reasoning, which is usually not what you want to show
    /// a user. Check the variant when the difference matters.
    pub fn text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text(text) => Some(text),
            ContentBlock::Thinking { text, .. } => Some(text),
            ContentBlock::ToolResult { content, .. } => Some(content),
            ContentBlock::ToolUse { .. } => None,
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
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.as_str()),
                _ => None,
            })
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
    fn waiting_for_a_tool_is_complete() {
        assert!(StopReason::ToolUse.is_complete());
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
