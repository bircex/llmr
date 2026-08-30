//! The one trait every provider implements.

use crate::chat::request::ChatRequest;
use crate::chat::response::ChatResponse;
use crate::chat::stream::{Event, EventStream};
use crate::model::{ModelCapabilities, ModelId};
use crate::Result;
use async_trait::async_trait;

/// Something that can answer a [`ChatRequest`].
///
/// # Implementing one
///
/// Three rules, and the third is the one that is easy to get wrong.
///
/// 1. [`Provider::capabilities`] must be honest. Returning `None` means you do not know
///    this model, which is different from knowing it and having nothing to offer.
/// 2. [`Provider::chat`] must not invent usage. If the provider reported nothing, return
///    [`crate::Usage::absent`] rather than zeros.
/// 3. `chat` takes `&self`. Anything you need to share must be immutable after
///    construction, or behind an atomic. Do not hold a lock across the await inside it.
///
/// The third rule is why every provider in this crate stores only an `Arc` to a transport
/// and some configuration. It means one provider can serve any number of concurrent calls
/// with nothing to contend on, and it makes a deadlock inside a provider impossible rather
/// than unlikely. The `await_holding_lock` lint is denied crate wide so that this stays
/// true as the code grows.
///
/// If you write a provider of your own, the `testkit` feature has a contract suite that
/// checks these properties for you.
#[async_trait]
pub trait Provider: Send + Sync {
    /// A short name for this provider, used in records and reports.
    ///
    /// Two providers reporting the same usage are only comparable if you can tell which is
    /// which, so this ends up in the ledger beside every call.
    fn id(&self) -> &str;

    /// What this model can do when reached through this provider.
    ///
    /// Returns `None` for a model this provider does not recognise. That is a different
    /// answer from a model it knows and has nothing to offer for, which is a
    /// [`ModelCapabilities`] with everything off.
    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities>;

    /// Sends one request and waits for the reply.
    ///
    /// # Errors
    ///
    /// See [`crate::Error`]. A reply the provider sent and this crate could not read is an
    /// [`crate::Error::Unreadable`], never an empty answer.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// Sends one request and reads the reply as it arrives.
    ///
    /// The default calls [`Provider::chat`] and yields the finished reply as one burst of
    /// events, so a provider that implements only `chat` still compiles and still answers
    /// here. It is a real answer rather than a refusal: a caller that streams gets the same
    /// text and the same usage, just all at once.
    ///
    /// Say whether a pairing streams for real through
    /// [`ModelCapabilities::streaming`](crate::ModelCapabilities::streaming). A caller who
    /// needs it word by word can then find out by asking, which is the same bargain this
    /// crate makes for tools and caching.
    ///
    /// # Errors
    ///
    /// The same errors as [`Provider::chat`] for a failure before the stream starts. A
    /// failure *after* it starts arrives as an `Err` item inside the stream, and whatever
    /// came before it is still valid — see [`crate::Transcript::drain`].
    async fn stream(&self, request: ChatRequest) -> Result<EventStream<'_>> {
        Ok(replay_stream(&self.chat(request).await?))
    }

    /// What the provider says it serves right now.
    ///
    /// A local table of model names goes stale on the vendor's schedule rather than yours,
    /// and this is the only way to find out that it has.
    ///
    /// # Errors
    ///
    /// The default returns [`crate::Error::Unsupported`], because a provider with no
    /// catalogue endpoint has no answer. That is different from an empty list, which would
    /// read as the vendor having retired everything.
    async fn catalogue(&self) -> Result<Vec<ModelId>> {
        Err(crate::Error::Unsupported(format!(
            "{} has no model catalogue",
            self.id()
        )))
    }
}

/// A finished reply, as the stream that would have produced it.
///
/// Shared by the default [`Provider::stream`] and by
/// [`ApiProvider`](crate::providers::api::ApiProvider)'s fallback for protocols with no
/// streaming form, so the two cannot drift into producing different event shapes for the
/// same reply.
pub(crate) fn replay_stream(reply: &ChatResponse) -> EventStream<'static> {
    let mut events = vec![Event::Started {
        model: reply.model.clone(),
    }];
    events.extend(replay(reply));
    events.push(Event::Stopped {
        reason: reply.stop_reason,
        details: reply.stop_details.clone(),
    });
    // Last, exactly where a real stream puts it. A caller that reads usage early must see
    // nothing here either, or the default would be the one place the rule does not hold and
    // the bug would surface only against a provider that really streams.
    events.push(Event::Metered(reply.usage));

    Box::pin(Replayed {
        events: events.into_iter(),
    })
}

/// A finished reply, taken apart into the content events that would have produced it.
///
/// The inverse of [`crate::Transcript`], and the tests check the round trip: a reply broken
/// into events and folded back must be the reply it started as.
fn replay(reply: &ChatResponse) -> Vec<Event> {
    let mut events = Vec::new();
    for block in &reply.message.content {
        match block {
            crate::chat::message::ContentBlock::Text(text) => {
                events.push(Event::TextDelta(text.clone()));
            }
            crate::chat::message::ContentBlock::Thinking { text, signature } => {
                events.push(Event::ThinkingDelta(text.clone()));
                if let Some(signature) = signature {
                    events.push(Event::ThinkingSignature(signature.clone()));
                }
            }
            crate::chat::message::ContentBlock::ToolUse { id, name, input } => {
                events.push(Event::ToolUseStarted {
                    id: id.clone(),
                    name: name.clone(),
                });
                events.push(Event::ToolArgumentsDelta(input.to_string()));
            }
            crate::chat::message::ContentBlock::Opaque { kind, raw } => {
                events.push(Event::Opaque {
                    kind: kind.clone(),
                    raw: raw.clone(),
                });
            }
            // A tool result is something the caller sent, not something a model produces.
            // It cannot appear in a reply, and inventing an event for it would put one
            // into a transcript that reassembles into a message the provider never sent.
            crate::chat::message::ContentBlock::ToolResult { .. } => {}
        }
    }
    events
}

/// A stream over events already in hand.
struct Replayed {
    events: std::vec::IntoIter<Event>,
}

impl futures_core::Stream for Replayed {
    type Item = Result<Event>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(self.events.next().map(Ok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::message::{ContentBlock, Message, Role, StopReason};
    use crate::chat::stream::Transcript;
    use crate::cost::usage::Usage;

    /// A reply carrying one of everything a model can produce.
    fn rich_reply() -> ChatResponse {
        ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "working".into(),
                        signature: Some("sig".into()),
                    },
                    ContentBlock::Text("Four.".into()),
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "search".into(),
                        input: serde_json::json!({ "q": "rust" }),
                    },
                    ContentBlock::Opaque {
                        kind: "redacted_thinking".into(),
                        raw: serde_json::json!({ "type": "redacted_thinking", "data": "x" }),
                    },
                ],
            },
            StopReason::ToolUse,
            Usage::absent().with_input(10).with_output(7),
            "m".into(),
        )
    }

    #[test]
    fn a_reply_broken_into_events_folds_back_into_itself() {
        // What makes the default `stream` an answer rather than an approximation. If this
        // drifts, a caller who switches to streaming gets a different reply for the same
        // question and nothing tells them.
        let reply = rich_reply();

        let mut transcript = Transcript::new("m");
        transcript.push(Event::Started {
            model: reply.model.clone(),
        });
        for event in replay(&reply) {
            transcript.push(event);
        }
        transcript.push(Event::Stopped {
            reason: reply.stop_reason,
            details: reply.stop_details.clone(),
        });
        transcript.push(Event::Metered(reply.usage));

        let assembled = transcript.finish();
        assert_eq!(assembled.message, reply.message);
        assert_eq!(assembled.stop_reason, reply.stop_reason);
        assert_eq!(assembled.usage, reply.usage);
        assert_eq!(assembled.model, reply.model);
    }

    #[test]
    fn a_tool_result_never_becomes_an_event() {
        // It is the caller's, not the model's. One replayed here would assemble into a
        // message the provider never sent.
        let reply = ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "42".into(),
                    is_error: false,
                }],
            },
            StopReason::EndTurn,
            Usage::absent(),
            "m".into(),
        );
        assert!(replay(&reply).is_empty());
    }
}
