//! The one trait every provider implements.

use crate::chat::request::ChatRequest;
use crate::chat::response::ChatResponse;
use crate::chat::stream::{Event, EventStream};
use crate::model::{ModelCapabilities, ModelId};
use crate::Result;
use async_trait::async_trait;

/// Whether a model can be reached, as far as a free check can tell.
///
/// Three answers rather than two. `Unknown` is the one a boolean loses, and it is the one
/// that matters: a tool that is not installed is denied, and a network that happened to be
/// down while the check ran is not. Collapsed into `false`, the second takes a working
/// provider out of a router for a reason that had cleared before anybody read the log.
///
/// It is the same rule [`crate::Usage`] follows. What nobody measured is absent rather than
/// zero, and what nobody established is unknown rather than denied.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Access {
    /// Checked, and nothing was found that would stop a call.
    ///
    /// This is the absence of a known blocker rather than a guarantee, and how much it
    /// establishes depends on the reach. A provider that asked a vendor for its model list
    /// has established the credential and the entitlement, because those are what that
    /// endpoint answers with. A command line tool that ran and printed its version has
    /// established that it is installed, and nothing about the login inside it.
    Ready,

    /// The provider was asked and said no.
    ///
    /// Settled. Asking again in a minute returns the same answer, and clearing it needs a
    /// person: a key, an entitlement, an install, a spelling.
    Denied {
        /// What was said, for whoever has to fix it.
        reason: String,
    },

    /// It could not be established.
    ///
    /// Not a polite `Denied`. Nothing is known either way, so a caller may still try, and a
    /// router must not strike the route off on the strength of it.
    Unknown {
        /// What stopped the check, for whoever reads the report.
        why: String,
    },
}

impl Access {
    /// Whether nothing was found that would stop a call.
    pub fn is_ready(&self) -> bool {
        matches!(self, Access::Ready)
    }

    /// Whether the provider was asked and said no.
    ///
    /// Deliberately not `!is_ready()`. An unknown is not a refusal, and the two differ in
    /// what a caller should do next.
    pub fn is_denied(&self) -> bool {
        matches!(self, Access::Denied { .. })
    }

    /// Whether the check established nothing.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Access::Unknown { .. })
    }

    /// Denied by something a person has to fix.
    pub fn denied(reason: impl Into<String>) -> Access {
        Access::Denied {
            reason: reason.into(),
        }
    }

    /// Not established, and here is what stopped it.
    pub fn unknown(why: impl Into<String>) -> Access {
        Access::Unknown { why: why.into() }
    }

    /// How an answer is written down, in a record or a report.
    ///
    /// One spelling in one place, like [`crate::Reach::as_str`], so two reports cannot
    /// disagree about what a denied route was called.
    pub fn as_str(&self) -> &'static str {
        match self {
            Access::Ready => "ready",
            Access::Denied { .. } => "denied",
            Access::Unknown { .. } => "unknown",
        }
    }

    /// What was said, when anything was.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Access::Ready => None,
            Access::Denied { reason } => Some(reason),
            Access::Unknown { why } => Some(why),
        }
    }
}

impl std::fmt::Display for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.detail() {
            None => f.write_str(self.as_str()),
            Some(detail) => write!(f, "{}: {detail}", self.as_str()),
        }
    }
}

/// Something that can answer a [`ChatRequest`].
///
/// # Implementing one
///
/// Four rules, and the third is the one that is easy to get wrong.
///
/// 1. [`Provider::capabilities`] must be honest. Returning `None` means you do not know
///    this model, which is different from knowing it and having nothing to offer.
/// 2. [`Provider::chat`] must not invent usage. If the provider reported nothing, return
///    [`crate::Usage::absent`] rather than zeros.
/// 3. `chat` takes `&self`. Anything you need to share must be immutable after
///    construction, or behind an atomic. Do not hold a lock across the await inside it.
/// 4. [`Provider::validate`] must not send a billable request, and must not report a
///    rejected credential as [`Access::Unknown`].
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

    /// Whether a request for this model would be accepted, without sending one.
    ///
    /// Two rules, and both are about what this must not do.
    ///
    /// **It must not cost anything.** Ask for a model list, ask a program whether it is
    /// there, ask anything that does not generate a token. A preflight that spends money is
    /// called once, then wrapped in a flag, then skipped.
    ///
    /// **It must not report a rejected credential as [`Access::Unknown`].** An unknown reads
    /// as "ask again later", so a router keeps the provider and a retry loop keeps trying
    /// it, and the one failure a person has to fix is the one that never surfaces.
    ///
    /// There is no `Result` here on purpose. An `Err` and an `Unknown` would be two channels
    /// carrying the same meaning, and a caller handles one of them. Deciding which failures
    /// are settled and which are not is this crate's job, because it is the crate that knows
    /// a 401 is settled and a 503 is not.
    ///
    /// Nothing caches this. A credential rotates and a subscription lapses, so an answer
    /// kept from earlier is a claim about a moment that has passed. Call it at startup
    /// through [`crate::Router::preflight`] rather than on the path a request takes.
    ///
    /// The default answers [`Access::Unknown`], which is the honest answer for a provider
    /// with nothing free to ask. It is not [`Access::Denied`].
    async fn validate(&self, _model: &ModelId) -> Access {
        Access::unknown(format!("{} has no free way to be asked", self.id()))
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
            // Neither of these is something a model produces. A tool result is something
            // the caller sent, and no protocol here reads an image out of a reply — an
            // unrecognised block comes back as `Opaque`. Inventing an event for either
            // would put it into a transcript that reassembles into a message the provider
            // never sent.
            crate::chat::message::ContentBlock::ToolResult { .. }
            | crate::chat::message::ContentBlock::Image { .. } => {}
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
    use crate::chat::request::ChatRequest;
    use crate::chat::response::ChatResponse;

    /// A provider that implements only what the trait requires, so the defaults are what is
    /// under test.
    struct Bare;

    #[async_trait]
    impl Provider for Bare {
        fn id(&self) -> &str {
            "bare"
        }

        fn capabilities(&self, _model: &ModelId) -> Option<ModelCapabilities> {
            None
        }

        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Err(crate::Error::Unsupported("not in this test".into()))
        }
    }

    #[tokio::test]
    async fn a_provider_with_nothing_to_ask_answers_unknown_rather_than_denied() {
        // The difference is what a caller does next. Denied means stop and fix something,
        // and a provider that was never asked has not earned that answer.
        let access = Bare.validate(&"any-model".into()).await;
        assert!(access.is_unknown(), "{access:?}");
        assert!(!access.is_denied());
        assert_eq!(access.detail().map(|d| d.contains("bare")), Some(true));
    }

    #[test]
    fn denied_is_not_merely_the_absence_of_ready() {
        // A caller reading `!is_ready()` as a refusal would drop a provider that was only
        // unreachable for a minute, so the three answers stay three.
        let unknown = Access::unknown("the network was down");
        assert!(!unknown.is_ready());
        assert!(!unknown.is_denied());
        assert!(unknown.is_unknown());

        let denied = Access::denied("the key was rejected");
        assert!(!denied.is_ready());
        assert!(denied.is_denied());
        assert!(!denied.is_unknown());
    }

    #[test]
    fn ready_carries_no_detail_and_the_other_two_do() {
        assert_eq!(Access::Ready.detail(), None);
        assert_eq!(
            Access::denied("no entitlement").detail(),
            Some("no entitlement")
        );
        assert_eq!(Access::unknown("timed out").detail(), Some("timed out"));
    }

    #[test]
    fn every_answer_has_one_spelling() {
        // Two reports that spell a denied route differently cannot be compared, which is
        // the same reason `Reach` has an `as_str`.
        assert_eq!(Access::Ready.as_str(), "ready");
        assert_eq!(Access::denied("x").as_str(), "denied");
        assert_eq!(Access::unknown("x").as_str(), "unknown");
    }

    #[test]
    fn what_was_said_reaches_the_line_somebody_reads() {
        assert_eq!(Access::Ready.to_string(), "ready");
        assert_eq!(
            Access::denied("the key was rejected").to_string(),
            "denied: the key was rejected"
        );
        assert_eq!(
            Access::unknown("503 from the vendor").to_string(),
            "unknown: 503 from the vendor"
        );
    }
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
