//! A reply as it arrives, rather than all at once.
//!
//! [`Provider::stream`](crate::Provider::stream) hands back a sequence of [`Event`]s instead
//! of a finished [`ChatResponse`]. Everything else about the call is the same: the same
//! request goes out, the same provider answers, and the same rules apply to what comes back.
//!
//! # The three things a streamed call gets wrong on its own
//!
//! **Usage arrives last.** A provider reports token counts in its final frame, not with the
//! first word. A caller that reads usage early sees nothing, and nothing is not zero — write
//! a zero there and the call becomes free in every report that adds it up. [`Transcript`]
//! holds usage as absent until a [`Event::Metered`] arrives, and merges the ones that do.
//!
//! **Reasoning signatures have to survive being assembled.** A thinking block arrives as text
//! deltas and then a signature, and the provider checks the history you send back against
//! what it produced. A block reassembled without its signature is rejected on the *next*
//! turn, which is a long way from the mistake.
//!
//! **A stream can fail after some of it arrived.** That is not the same as a call that
//! failed, and a caller that cannot tell them apart will either discard work it paid for or
//! show a truncated answer as finished. See [`Transcript::drain`].
//!
//! # Assembling one
//!
//! ```no_run
//! use llmr::chat::stream::Transcript;
//! # async fn example(p: &impl llmr::Provider, request: llmr::ChatRequest) -> llmr::Result<()> {
//! let mut transcript = Transcript::new(request.model.clone());
//! let outcome = transcript.drain(p.stream(request).await?).await;
//!
//! // What arrived is yours either way. The error says why it stopped early.
//! let reply = transcript.finish();
//! if let Err(cut_short) = outcome {
//!     eprintln!("{} arrived before {cut_short}", reply.text().len());
//! }
//! assert!(reply.is_complete() || !reply.is_complete());
//! # Ok(())
//! # }
//! ```

use crate::chat::message::{ContentBlock, Message, Role, StopReason};
use crate::chat::response::ChatResponse;
use crate::cost::usage::Usage;
use crate::error::Result;
use crate::model::ModelId;
use futures_core::Stream;
use serde_json::{json, Value};
use std::pin::Pin;

/// A sequence of [`Event`]s, as a provider produces them.
///
/// Boxed because `Provider` is used as a trait object, and a trait object cannot name the
/// concrete stream type each provider builds.
pub type EventStream<'a> = Pin<Box<dyn Stream<Item = Result<Event>> + Send + 'a>>;

/// One piece of a reply, as it arrives.
///
/// A delta rather than a whole message. Text and reasoning arrive in fragments, tool
/// arguments arrive as a string being written a few characters at a time, and the stop
/// reason and the usage arrive at the end.
///
/// Assembling these by hand is easy to get subtly wrong, which is what [`Transcript`] is
/// for. Read them directly when you are showing them to somebody as they land.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// The turn began, and this is the model actually serving it.
    ///
    /// Can differ from the one you asked for. Price against this one.
    Started {
        /// Which model answered.
        model: ModelId,
    },

    /// Text appended to the answer.
    TextDelta(String),

    /// Reasoning appended to the thinking block in progress.
    ThinkingDelta(String),

    /// The provider's proof it produced the thinking block that just ended.
    ///
    /// Arrives after the reasoning it belongs to. It must end up attached to that block or
    /// the conversation cannot be continued.
    ThinkingSignature(String),

    /// The model began asking for a tool.
    ToolUseStarted {
        /// The provider's identifier for this call.
        id: String,
        /// Which tool.
        name: String,
    },

    /// More of the arguments for the tool call in progress.
    ///
    /// A fragment of a JSON document being written, not a JSON document. It only parses
    /// once the last fragment has arrived.
    ToolArgumentsDelta(String),

    /// A whole block this crate does not model.
    ///
    /// Kept verbatim for the same reason [`ContentBlock::Opaque`] is: the provider checks
    /// the history you send back against what it produced.
    Opaque {
        /// The provider's own name for this kind of block.
        kind: String,
        /// Exactly what arrived.
        raw: Value,
    },

    /// Why the model stopped.
    Stopped {
        /// The reason, mapped the same way a non streamed reply maps it.
        reason: StopReason,
        /// What the provider said about it, when it said anything.
        details: Option<String>,
    },

    /// What the call consumed, as far as the provider reported it.
    ///
    /// May arrive more than once — some protocols report the prompt at the start and the
    /// output at the end — and [`Transcript`] merges them. A stream that never sends one
    /// leaves usage [absent](Usage::absent), never zero.
    Metered(Usage),
}

/// A streamed reply, assembled.
///
/// Fold [`Event`]s into this and it produces the same [`ChatResponse`] the non streamed call
/// would have returned. That equality is checked by the contract suite rather than hoped
/// for: two ways to ask the same question that disagree about the answer are worse than one.
///
/// It is deliberately usable after a failure. Everything that arrived before the stream
/// broke is still here, and [`finish`](Transcript::finish) will hand it to you marked
/// [`StopReason::Interrupted`].
#[derive(Debug, Clone)]
pub struct Transcript {
    blocks: Vec<ContentBlock>,
    /// The tool call being written, if one is: id, name, and the arguments so far.
    pending_tool: Option<(String, String, String)>,
    stop: Option<(StopReason, Option<String>)>,
    usage: Option<Usage>,
    served_by: Option<ModelId>,
    asked_for: ModelId,
}

impl Transcript {
    /// An empty transcript for a request that named this model.
    ///
    /// The model is needed because a stream that fails before its first frame never says
    /// which model served it, and a reply has to name one.
    pub fn new(asked_for: impl Into<ModelId>) -> Self {
        Self {
            blocks: Vec::new(),
            pending_tool: None,
            stop: None,
            usage: None,
            served_by: None,
            asked_for: asked_for.into(),
        }
    }

    /// Folds one event in.
    pub fn push(&mut self, event: Event) {
        match event {
            Event::Started { model } => self.served_by = Some(model),

            Event::TextDelta(text) => {
                self.close_tool();
                match self.blocks.last_mut() {
                    Some(ContentBlock::Text(existing)) => existing.push_str(&text),
                    _ => self.blocks.push(ContentBlock::Text(text)),
                }
            }

            Event::ThinkingDelta(text) => {
                self.close_tool();
                match self.blocks.last_mut() {
                    // Only while the signature has not arrived. Once it has, the block is
                    // finished, and appending to it would put reasoning after the proof
                    // that covers it.
                    Some(ContentBlock::Thinking {
                        text: existing,
                        signature: None,
                    }) => existing.push_str(&text),
                    _ => self.blocks.push(ContentBlock::Thinking {
                        text,
                        signature: None,
                    }),
                }
            }

            Event::ThinkingSignature(sig) => {
                self.close_tool();
                // Onto the thinking block it belongs to, which is the last one still open.
                // A signature that lands anywhere else is a conversation that fails on the
                // turn after this one.
                if let Some(ContentBlock::Thinking { signature, .. }) =
                    self.blocks.iter_mut().rev().find(|b| {
                        matches!(
                            b,
                            ContentBlock::Thinking {
                                signature: None,
                                ..
                            }
                        )
                    })
                {
                    *signature = Some(sig);
                } else {
                    // A signature with no block to carry it. Keeping it verbatim is the
                    // only honest thing available: dropping it silently loses the proof.
                    self.blocks.push(ContentBlock::Opaque {
                        kind: "signature_without_block".into(),
                        raw: json!({ "signature": sig }),
                    });
                }
            }

            Event::ToolUseStarted { id, name } => {
                self.close_tool();
                self.pending_tool = Some((id, name, String::new()));
            }

            Event::ToolArgumentsDelta(fragment) => {
                if let Some((_, _, args)) = self.pending_tool.as_mut() {
                    args.push_str(&fragment);
                }
                // A fragment with no call in progress is dropped. There is nothing to
                // attach it to, and inventing a call to hold it would put a tool the model
                // never asked for into the conversation.
            }

            Event::Opaque { kind, raw } => {
                self.close_tool();
                self.blocks.push(ContentBlock::Opaque { kind, raw });
            }

            Event::Stopped { reason, details } => {
                self.close_tool();
                self.stop = Some((reason, details));
            }

            Event::Metered(usage) => {
                // Merged rather than replaced. A protocol that reports the prompt at the
                // start and the output at the end sends two, and keeping only the last
                // would throw away the input count.
                self.usage = Some(match self.usage {
                    Some(existing) => existing.merge(usage),
                    None => usage,
                });
            }
        }
    }

    /// Reads a whole stream into this transcript.
    ///
    /// # Errors
    ///
    /// Whatever the stream failed with. **The transcript is still valid**: everything that
    /// arrived before the failure is in it, and [`finish`](Transcript::finish) will hand it
    /// back marked [`StopReason::Interrupted`]. That is the whole point of this returning
    /// rather than consuming — a caller can tell what arrived, that the turn did not
    /// finish, and why, without inferring any of the three from the others.
    pub async fn drain(&mut self, mut stream: EventStream<'_>) -> Result<()> {
        while let Some(event) = std::future::poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await
        {
            self.push(event?);
        }
        Ok(())
    }

    /// The answer so far, as plain text.
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(ContentBlock::answer_text)
            .collect()
    }

    /// Whether a stop reason has arrived.
    ///
    /// False while the turn is still being written, and false forever if the stream broke.
    pub fn is_finished(&self) -> bool {
        self.stop.is_some()
    }

    /// The assembled reply.
    ///
    /// A transcript with no stop reason is one whose stream ended early, and it comes back
    /// as [`StopReason::Interrupted`] rather than as something that looks finished. Usage
    /// nobody reported is [`Usage::absent`], never zero.
    pub fn finish(mut self) -> ChatResponse {
        self.close_tool();

        let (reason, details) = self.stop.unwrap_or((
            StopReason::Interrupted,
            Some("the stream ended early".into()),
        ));

        let mut response = ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: self.blocks,
            },
            reason,
            self.usage.unwrap_or_else(Usage::absent),
            self.served_by.unwrap_or(self.asked_for),
        );
        if let Some(details) = details {
            response = response.with_stop_details(details);
        }
        response
    }

    /// Turns the tool call in progress, if there is one, into a finished block.
    fn close_tool(&mut self) {
        let Some((id, name, arguments)) = self.pending_tool.take() else {
            return;
        };
        self.blocks.push(ContentBlock::ToolUse {
            id,
            name,
            // The same fallback the non streamed readers use, so a call whose arguments
            // never parsed reads identically either way. Kept rather than dropped: a tool
            // call with its arguments thrown away is one nobody can diagnose.
            input: serde_json::from_str(&arguments).unwrap_or_else(|_| json!({ "raw": arguments })),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_deltas_join_into_one_block_rather_than_many() {
        let mut t = Transcript::new("m");
        t.push(Event::TextDelta("Hel".into()));
        t.push(Event::TextDelta("lo".into()));
        t.push(Event::Stopped {
            reason: StopReason::EndTurn,
            details: None,
        });
        let reply = t.finish();
        assert_eq!(reply.text(), "Hello");
        assert_eq!(reply.message.content.len(), 1);
    }

    #[test]
    fn a_signature_lands_on_the_thinking_block_it_belongs_to() {
        // The property a conversation with reasoning in it depends on. Assembled without
        // this, the turn *after* this one is rejected.
        let mut t = Transcript::new("m");
        t.push(Event::ThinkingDelta("becau".into()));
        t.push(Event::ThinkingDelta("se".into()));
        t.push(Event::ThinkingSignature("sig-abc".into()));
        t.push(Event::TextDelta("Four.".into()));
        let reply = t.finish();
        assert_eq!(
            reply.message.content[0],
            ContentBlock::Thinking {
                text: "because".into(),
                signature: Some("sig-abc".into()),
            }
        );
    }

    #[test]
    fn usage_that_never_arrived_is_absent_rather_than_zero() {
        // Zero here would make the call free in every report that adds it up.
        let mut t = Transcript::new("m");
        t.push(Event::TextDelta("ok".into()));
        assert_eq!(
            t.finish().usage.coverage(),
            crate::cost::usage::UsageCoverage::Absent
        );
    }

    #[test]
    fn two_usage_events_are_merged_rather_than_the_last_one_winning() {
        // A protocol reporting the prompt first and the output last sends two. Keeping only
        // the second throws away the input count.
        let mut t = Transcript::new("m");
        t.push(Event::Metered(Usage::absent().with_input(10)));
        t.push(Event::Metered(Usage::absent().with_output(7)));
        let usage = t.finish().usage;
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(7));
    }

    #[test]
    fn a_stream_that_stopped_early_is_not_a_finished_answer() {
        let mut t = Transcript::new("m");
        t.push(Event::TextDelta("half an answ".into()));
        let reply = t.finish();
        assert_eq!(reply.stop_reason, StopReason::Interrupted);
        assert!(
            !reply.is_complete(),
            "a cut off reply must not read as done"
        );
        assert_eq!(reply.text(), "half an answ", "what arrived is still ours");
    }

    #[test]
    fn tool_arguments_assemble_and_survive_not_parsing() {
        let mut t = Transcript::new("m");
        t.push(Event::ToolUseStarted {
            id: "call_1".into(),
            name: "search".into(),
        });
        t.push(Event::ToolArgumentsDelta("{\"q\":".into()));
        t.push(Event::ToolArgumentsDelta("\"rust\"}".into()));
        t.push(Event::Stopped {
            reason: StopReason::ToolUse,
            details: None,
        });
        match &t.finish().message.content[0] {
            ContentBlock::ToolUse { input, .. } => assert_eq!(input["q"], "rust"),
            other => panic!("expected a tool call, got {other:?}"),
        }

        let mut broken = Transcript::new("m");
        broken.push(Event::ToolUseStarted {
            id: "call_2".into(),
            name: "search".into(),
        });
        broken.push(Event::ToolArgumentsDelta("{not json".into()));
        match &broken.finish().message.content[0] {
            // Matches what the non streamed readers do with the same input.
            ContentBlock::ToolUse { input, .. } => assert_eq!(input["raw"], "{not json"),
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn the_model_that_answered_wins_over_the_one_that_was_asked_for() {
        let mut t = Transcript::new("asked-for");
        t.push(Event::Started {
            model: "served-by".into(),
        });
        assert_eq!(t.finish().model.as_str(), "served-by");
    }

    #[test]
    fn a_stream_that_named_no_model_falls_back_to_the_one_requested() {
        assert_eq!(
            Transcript::new("asked-for").finish().model.as_str(),
            "asked-for"
        );
    }
}
