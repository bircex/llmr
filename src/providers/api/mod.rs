//! The machinery every provider reached over the network shares.
//!
//! Nothing vendor specific lives here. The providers themselves are under their vendor —
//! `providers::anthropic::api`, `providers::openai::api` — because that is
//! what a caller picks. This is what a contributor implements.
//!
//! # One machine, many protocols
//!
//! Everything an API provider does apart from writing JSON is the same: build a request,
//! attach a credential, send it, read the status, parse the body, turn a failure into an
//! [`crate::Error`]. Written per provider, that is the same twenty lines repeated with one
//! word changed, and the copies drift the first time one of them is fixed.
//!
//! So [`ApiProvider`] does all of it, and a provider supplies only [`Protocol`]: what URL,
//! what headers, what JSON goes out, and what comes back. Anthropic is 120 lines of that and
//! nothing else.
//!
//! The generic is a type parameter rather than a trait object, so the protocol call is
//! resolved at compile time. There is no vtable on the path a request takes.
//!
//! # No feature gate
//!
//! This module is always present, including with no features at all. [`Protocol`] is the
//! extension point for a protocol nobody has written yet, and needing somebody else's vendor
//! feature switched on to reach it would be a strange toll to pay.

use crate::chat::stream::{Event, EventStream};
use crate::chat::{ChatRequest, ChatResponse};
use crate::error::{Error, Result};
use crate::model::{ModelCapabilities, ModelId, Reach};
use crate::observe;
use crate::provider::{Access, Provider};
use crate::registry::Registry;
use crate::secret::Secret;
use crate::transport::{ByteStream, HttpRequest, HttpTransport};
use async_trait::async_trait;
use futures_core::Stream;
use serde_json::Value;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// One server sent event, reassembled from the lines that carried it.
///
/// The wire format is lines: `event: name`, then one or more `data:` lines, then a blank
/// line ending the frame. A protocol reads this rather than the wire, because splitting on
/// blank lines and rejoining multi line data is the same code for every vendor and exactly
/// the code that is wrong in the interesting cases.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SseFrame {
    /// What the server called this frame, from its `event:` line.
    ///
    /// Empty when it sent none. Anthropic names every frame; the OpenAI shape names none
    /// and puts the type inside the JSON.
    pub event: String,
    /// The `data:` lines, joined with newlines and with the leading space removed.
    pub data: String,
}

impl SseFrame {
    /// The data as JSON.
    ///
    /// `None` when it is not JSON, which covers the OpenAI shape's `[DONE]` sentinel as
    /// well as anything malformed. A protocol decides which of those it is looking at.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.data).ok()
    }
}

/// What one vendor's HTTP protocol says, and nothing else.
///
/// Implement this to add a provider. You are writing a translation, not a client: there is
/// no transport here, no retry, no error mapping, and no place to hold state. A protocol is
/// a set of pure functions over a request and a body.
///
/// That is deliberate. Everything you are not writing is the part that is identical between
/// vendors, and the part where a mistake is subtle.
pub trait Protocol: Send + Sync {
    /// A short name, recorded beside every call this provider made.
    fn id(&self) -> &str;

    /// Where a chat request goes, given the base URL.
    fn chat_url(&self, base_url: &str) -> String;

    /// The headers a chat request carries.
    ///
    /// # Errors
    ///
    /// Return [`Error::Auth`] when the credential cannot be used, which in practice means
    /// it is not valid UTF-8.
    fn headers(&self, key: &Secret) -> Result<Vec<(String, String)>>;

    /// The request, as this protocol writes it.
    ///
    /// # Errors
    ///
    /// Return [`Error::InvalidRequest`] for anything this protocol cannot express. Do not
    /// drop it silently: a request sent without half of what the caller asked for produces
    /// a reply they will be billed for and cannot explain.
    fn body(&self, request: &ChatRequest) -> Result<Value>;

    /// The reply, as this protocol writes it.
    ///
    /// `asked_for` is the model the caller named, to fall back on when the reply does not
    /// say which one served it.
    ///
    /// # Errors
    ///
    /// Return [`Error::Unreadable`] when the body cannot be read. Never return an empty
    /// answer: a caller cannot tell one from a failure, and one of them means carry on.
    fn read(&self, body: &Value, asked_for: &ModelId) -> Result<ChatResponse>;

    /// Where the model list is, when this protocol has one.
    ///
    /// `None` by default, which becomes [`Error::Unsupported`] rather than an empty list.
    /// An empty list would read as the vendor having retired everything.
    fn catalogue_url(&self, _base_url: &str) -> Option<String> {
        None
    }

    /// The model list, as this protocol writes it.
    ///
    /// # Errors
    ///
    /// Return [`Error::Unreadable`] when the body cannot be read.
    fn read_catalogue(&self, _body: &Value) -> Result<Vec<ModelId>> {
        Err(Error::Unsupported(format!(
            "{} has no model catalogue",
            self.id()
        )))
    }

    /// The request, as this protocol writes it when the reply should arrive in pieces.
    ///
    /// `None` by default, meaning this protocol has no streaming form. [`ApiProvider`] then
    /// falls back to a whole call and hands the finished reply over as one burst, which is
    /// an answer rather than a refusal — the caller gets the same text and the same usage.
    ///
    /// Most protocols want the ordinary [`Protocol::body`] with a flag added, and some also
    /// need to ask for usage explicitly. A streamed call that forgets to ask reports nothing,
    /// and nothing becomes zero in whatever adds it up.
    ///
    /// # Errors
    ///
    /// The same as [`Protocol::body`].
    fn stream_body(&self, _request: &ChatRequest) -> Result<Option<Value>> {
        Ok(None)
    }

    /// One frame, translated into zero or more [`Event`]s.
    ///
    /// Zero is ordinary: protocols send keep alives, opening frames and terminators that
    /// carry nothing a caller needs.
    ///
    /// `asked_for` is the model the caller named, for a frame that reports one.
    ///
    /// # Errors
    ///
    /// Return [`Error::Unreadable`] for a frame this protocol cannot read. Do not return an
    /// empty list for one: an unreadable frame silently dropped is a reply missing a piece,
    /// and the caller has no way to know which piece.
    fn read_event(&self, _frame: &SseFrame, _asked_for: &ModelId) -> Result<Vec<Event>> {
        Ok(Vec::new())
    }
}

/// A protocol, plus everything every network provider needs.
///
/// Immutable once built. No lock and no interior mutability, so one instance serves any
/// number of concurrent calls with nothing to contend on.
pub struct ApiProvider<P: Protocol> {
    protocol: P,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
    base_url: String,
    reach: Reach,
    registry: Arc<Registry>,
}

impl<P: Protocol> ApiProvider<P> {
    /// A provider speaking this protocol at this base URL.
    ///
    /// The reach is given rather than inferred. The same protocol is spoken by a vendor's
    /// hosted API and by a model on this laptop, and the difference between them is where
    /// your data goes, which nothing here can work out.
    ///
    /// A trailing slash on the base URL is removed, so both spellings behave the same.
    pub fn new(
        protocol: P,
        base_url: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
        key: Secret,
        reach: Reach,
        registry: Arc<Registry>,
    ) -> Self {
        Self {
            protocol,
            transport,
            key,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            reach,
            registry,
        }
    }

    /// Where this provider's data goes.
    pub fn reach(&self) -> Reach {
        self.reach
    }

    /// The protocol underneath, for a caller that needs something specific to it.
    pub fn protocol(&self) -> &P {
        &self.protocol
    }

    /// Sends a body and returns the parsed reply.
    ///
    /// The whole shared path, in one place: headers, send, status, parse. A provider that
    /// wrote this itself would be a provider that could disagree with the others about what
    /// a 429 means.
    async fn call(&self, request: HttpRequest) -> Result<Value> {
        let mut request = request;
        request.headers = self.protocol.headers(&self.key)?;
        let response = self.transport.send(request).await?;

        response.check()?;

        serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the reply was not JSON: {e}")))
    }
}

#[async_trait]
impl<P: Protocol> Provider for ApiProvider<P> {
    fn id(&self) -> &str {
        self.protocol.id()
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        self.registry.capabilities(model)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let span = observe::calling(self.protocol.id(), &request.model, self.reach);
        observe::inside(span.clone(), async move {
            let body = serde_json::to_vec(&self.protocol.body(&request)?)
                .map_err(|e| Error::InvalidRequest(format!("building the request: {e}")))?;

            let parsed = self
                .call(HttpRequest::new(
                    self.protocol.chat_url(&self.base_url),
                    body,
                ))
                .await?;

            let reply = self.protocol.read(&parsed, &request.model)?;
            observe::measured(&span, reply.usage.coverage());
            Ok(reply)
        })
        .await
    }

    async fn catalogue(&self) -> Result<Vec<ModelId>> {
        let Some(url) = self.protocol.catalogue_url(&self.base_url) else {
            return Err(Error::Unsupported(format!(
                "{} has no model catalogue",
                self.protocol.id()
            )));
        };

        let parsed = self.call(HttpRequest::get(url)).await?;
        self.protocol.read_catalogue(&parsed)
    }

    async fn stream(&self, request: ChatRequest) -> Result<EventStream<'_>> {
        // A protocol with no streaming form gets the shared default: one whole call, handed
        // over as a burst of events. An answer rather than a refusal.
        let Some(body) = self.protocol.stream_body(&request)? else {
            // The same burst the shared default produces, from the same builder, so a
            // protocol that does not stream cannot answer in a different shape from one
            // whose provider never implemented `stream` at all.
            return Ok(crate::provider::replay_stream(&self.chat(request).await?));
        };

        let body = serde_json::to_vec(&body)
            .map_err(|e| Error::InvalidRequest(format!("building the request: {e}")))?;
        let mut http = HttpRequest::new(self.protocol.chat_url(&self.base_url), body);
        http.headers = self.protocol.headers(&self.key)?;

        Ok(Box::pin(Frames {
            bytes: self.transport.send_streaming(http).await?,
            protocol: &self.protocol,
            asked_for: request.model,
            buffer: Vec::new(),
            ready: VecDeque::new(),
            done: false,
        }))
    }

    /// Asks the vendor for its model list, which costs nothing and answers both halves.
    ///
    /// A list that came back proves the credential, because that endpoint refuses without
    /// one, and a model in it proves the entitlement, because the list is what this account
    /// may reach. No request is sent to [`Protocol::chat_url`], so nothing here can be
    /// billed.
    ///
    /// A protocol with no model list answers [`Access::Unknown`]. The only other question
    /// available is a real request, and a preflight that spends money is one that gets
    /// wrapped in a flag and skipped.
    async fn validate(&self, model: &ModelId) -> Access {
        let id = self.protocol.id();

        let Some(url) = self.protocol.catalogue_url(&self.base_url) else {
            return Access::unknown(format!(
                "{id} has no model list to ask for, and the only other question it answers                  costs a call"
            ));
        };

        let parsed = match self.call(HttpRequest::get(url)).await {
            Ok(parsed) => parsed,
            Err(e) => return refusal_or_doubt(id, &e),
        };

        let listed = match self.protocol.read_catalogue(&parsed) {
            Ok(listed) => listed,
            // The vendor answered and this crate could not read it. That says nothing about
            // the credential or the model, so it is not a denial.
            Err(e) => {
                return Access::unknown(format!(
                    "{id} sent a model list that could not be read: {e}"
                ))
            }
        };

        if listed.contains(model) {
            return Access::Ready;
        }

        // Settled, and worth a reason somebody can act on. The two ways to land here need
        // different fixes: an account without the entitlement, and a name the list spells
        // differently. Neither is visible from a bare "denied".
        Access::denied(format!(
            "{id} answered with {} models and none of them is {model}. Either this account              cannot reach it or the list spells it differently",
            listed.len()
        ))
    }
}

/// A byte stream, read as server sent event frames and translated by a protocol.
///
/// The buffering is here rather than in each protocol because splitting on blank lines,
/// rejoining multi line `data:` and handling a chunk boundary that falls inside a frame is
/// the same code for every vendor — and it is the code that is wrong in the cases nobody
/// tests. Two protocols must not be able to disagree about what half a frame means.
struct Frames<'a, P: Protocol> {
    bytes: ByteStream,
    protocol: &'a P,
    asked_for: ModelId,
    /// Bytes seen but not yet forming a whole frame.
    buffer: Vec<u8>,
    /// Events read from the last frame and not yet handed out.
    ready: VecDeque<Event>,
    done: bool,
}

impl<P: Protocol> Frames<'_, P> {
    /// Pulls whole frames out of the buffer and translates them.
    ///
    /// # Errors
    ///
    /// Whatever the protocol makes of a frame it cannot read.
    fn absorb(&mut self, chunk: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(chunk);
        while let Some(end) = frame_end(&self.buffer) {
            let raw = self.buffer.drain(..end.0).collect::<Vec<u8>>();
            self.buffer.drain(..end.1);
            if let Some(frame) = parse_frame(&raw) {
                self.ready
                    .extend(self.protocol.read_event(&frame, &self.asked_for)?);
            }
        }
        Ok(())
    }
}

impl<P: Protocol> Stream for Frames<'_, P> {
    type Item = Result<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.ready.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }
            if this.done {
                return Poll::Ready(None);
            }

            match this.bytes.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(e))) => {
                    // The stream broke partway. Everything already handed over stays valid;
                    // this is the caller's signal that the rest is not coming.
                    this.done = true;
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    if let Err(e) = this.absorb(&chunk) {
                        this.done = true;
                        return Poll::Ready(Some(Err(e)));
                    }
                }
                Poll::Ready(None) => {
                    this.done = true;
                    // A server that ended without a blank line after its last frame still
                    // sent that frame. Dropping it loses the terminator, which is where the
                    // stop reason and the usage live.
                    let trailing = std::mem::take(&mut this.buffer);
                    if let Some(frame) = parse_frame(&trailing) {
                        match this.protocol.read_event(&frame, &this.asked_for) {
                            Ok(events) => this.ready.extend(events),
                            Err(e) => return Poll::Ready(Some(Err(e))),
                        }
                    }
                }
            }
        }
    }
}

/// Where the first frame ends, as (length of the frame, length of the separator).
///
/// Both `\n\n` and `\r\n\r\n` end a frame. Servers send either, and one that sends the
/// second to a reader looking only for the first appears to hang until the connection
/// closes, then delivers everything at once.
fn frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buffer.len() {
        if buffer[i..].starts_with(b"\r\n\r\n") {
            return Some((i, 4));
        }
        if buffer[i..].starts_with(b"\n\n") {
            return Some((i, 2));
        }
        i += 1;
    }
    None
}

/// One frame's lines, as an [`SseFrame`].
///
/// `None` for a frame carrying no data, which is what a keep alive comment looks like.
fn parse_frame(raw: &[u8]) -> Option<SseFrame> {
    let text = String::from_utf8_lossy(raw);
    let mut frame = SseFrame::default();
    let mut data: Vec<&str> = Vec::new();

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        // A comment. Servers send these to keep the connection open.
        if line.starts_with(':') || line.is_empty() {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        // One leading space after the colon is part of the format, not the value.
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => frame.event = value.to_string(),
            "data" => data.push(value),
            // `id` and `retry` are for reconnection, which this crate does not do. Ignored
            // rather than an error: a server sending them is not misbehaving.
            _ => {}
        }
    }

    if data.is_empty() {
        return None;
    }
    frame.data = data.join("\n");
    Some(frame)
}

/// Which failures settle the question and which leave it open.
///
/// In one place, for the reason [`crate::transport::HttpResponse::check`] is: two providers
/// that disagreed about whether a 429 means "no" would produce reports that cannot be read
/// together. A caller acts on this without reading the message, so the split matters more
/// than the wording.
fn refusal_or_doubt(id: &str, e: &Error) -> Access {
    match e {
        // The vendor was asked and said no. Reported as unknown, this reads as "try again
        // later", so a router keeps the provider and nobody ever fixes the key.
        Error::Auth(said) => Access::denied(format!("{id} rejected the credential: {said}")),

        // The endpoint or the account is not there. Asking again produces the same 404.
        Error::NotFound(said) => Access::denied(format!("{id} has no such endpoint: {said}")),

        // Everything else establishes nothing. A rate limit, a server fault and a timeout
        // are all moments rather than answers, and a request this crate built wrongly is a
        // bug here rather than a fact about the account.
        other => Access::unknown(format!("{id} could not be asked: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ContentBlock, Message, Role, StopReason};
    use crate::cost::Usage;
    use crate::transport::{HttpResponse, Method};
    use crate::Provider;
    use std::sync::Mutex;

    /// A protocol with nothing in it but the two answers `validate` reads.
    ///
    /// Written here rather than borrowing a vendor's, so these tests check `ApiProvider`
    /// rather than whichever protocol happened to be compiled in.
    struct Test {
        lists: bool,
    }

    impl Protocol for Test {
        fn id(&self) -> &str {
            "test-protocol"
        }

        fn chat_url(&self, base_url: &str) -> String {
            format!("{base_url}/chat")
        }

        fn headers(&self, _key: &Secret) -> Result<Vec<(String, String)>> {
            Ok(Vec::new())
        }

        fn body(&self, _request: &ChatRequest) -> Result<Value> {
            Ok(serde_json::json!({}))
        }

        fn read(&self, _body: &Value, asked_for: &ModelId) -> Result<ChatResponse> {
            Ok(ChatResponse::new(
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text("ok".into())],
                },
                StopReason::EndTurn,
                Usage::absent(),
                asked_for.clone(),
            ))
        }

        fn catalogue_url(&self, base_url: &str) -> Option<String> {
            self.lists.then(|| format!("{base_url}/models"))
        }

        fn read_catalogue(&self, body: &Value) -> Result<Vec<ModelId>> {
            Ok(body
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::Unreadable("no data array".into()))?
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str))
                .map(ModelId::from)
                .collect())
        }
    }

    /// Answers with what the test scripted, and remembers what it was asked.
    ///
    /// The record is what proves `validate` sent nothing to the chat endpoint, which is the
    /// property that keeps a preflight free.
    struct Scripted {
        replies: Mutex<Vec<Result<HttpResponse>>>,
        seen: Mutex<Vec<(Method, String)>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<HttpResponse>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn urls(&self) -> Vec<(Method, String)> {
            self.seen.lock().map(|s| s.clone()).unwrap_or_default()
        }
    }

    #[async_trait]
    impl crate::transport::HttpTransport for Scripted {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
            if let Ok(mut seen) = self.seen.lock() {
                seen.push((request.method, request.url.clone()));
            }
            let mut replies = self
                .replies
                .lock()
                .map_err(|_| Error::Transient("poisoned".into()))?;
            if replies.is_empty() {
                return Err(Error::Transient("the test scripted no more replies".into()));
            }
            match replies.remove(0) {
                Ok(reply) => Ok(reply),
                Err(e) => Err(Error::Transient(e.to_string())),
            }
        }
    }

    fn listing(models: &[&str]) -> Result<HttpResponse> {
        let body = serde_json::json!({
            "data": models.iter().map(|m| serde_json::json!({ "id": m })).collect::<Vec<_>>()
        });
        Ok(HttpResponse::new(
            200,
            serde_json::to_vec(&body).unwrap_or_default(),
        ))
    }

    fn status(code: u16) -> Result<HttpResponse> {
        Ok(HttpResponse::new(
            code,
            b"the vendor said something".to_vec(),
        ))
    }

    fn provider(lists: bool, transport: Arc<Scripted>) -> ApiProvider<Test> {
        ApiProvider::new(
            Test { lists },
            "https://example.invalid",
            transport,
            Secret::new("key", "sk-test"),
            Reach::FirstPartyApi,
            Arc::new(Registry::empty("test", Reach::FirstPartyApi)),
        )
    }

    #[tokio::test]
    async fn a_protocol_with_no_model_list_answers_unknown_rather_than_denied() {
        // It was never asked. Denied here would strike a working provider off a list for
        // the crime of speaking a protocol without a catalogue endpoint.
        let transport = Scripted::new(Vec::new());
        let access = provider(false, transport.clone())
            .validate(&"any".into())
            .await;

        assert!(access.is_unknown(), "{access:?}");
        assert!(
            transport.urls().is_empty(),
            "nothing should have been sent: {:?}",
            transport.urls()
        );
    }

    #[tokio::test]
    async fn a_model_the_vendor_lists_is_ready() {
        let transport = Scripted::new(vec![listing(&["gpt-test", "gpt-other"])]);
        let access = provider(true, transport).validate(&"gpt-test".into()).await;
        assert_eq!(access, Access::Ready, "{access:?}");
    }

    #[tokio::test]
    async fn a_model_the_vendor_does_not_list_is_denied_and_says_which_two_things_it_could_be() {
        // A bare "denied" sends somebody looking at their account when the answer is that
        // the list spells the name differently, or the other way round.
        let transport = Scripted::new(vec![listing(&["gpt-other"])]);
        let access = provider(true, transport).validate(&"gpt-test".into()).await;

        assert!(access.is_denied(), "{access:?}");
        let said = access.detail().unwrap_or_default();
        assert!(said.contains("gpt-test"), "{said}");
        assert!(said.contains("cannot reach it"), "{said}");
        assert!(said.contains("spells it differently"), "{said}");
    }

    #[tokio::test]
    async fn a_rejected_credential_is_denied_rather_than_unknown() {
        // The one that matters most. Reported as unknown it reads as "ask again later", so
        // a router keeps the provider, a retry loop keeps trying, and nobody fixes the key.
        for code in [401, 403] {
            let transport = Scripted::new(vec![status(code)]);
            let access = provider(true, transport).validate(&"gpt-test".into()).await;
            assert!(access.is_denied(), "{code} gave {access:?}");
            assert!(
                access.detail().unwrap_or_default().contains("credential"),
                "{code}: {access}"
            );
        }
    }

    #[tokio::test]
    async fn an_endpoint_that_is_not_there_is_denied() {
        let transport = Scripted::new(vec![status(404)]);
        let access = provider(true, transport).validate(&"gpt-test".into()).await;
        assert!(access.is_denied(), "{access:?}");
    }

    #[tokio::test]
    async fn a_moment_rather_than_an_answer_is_unknown() {
        // A server fault, a rate limit and a gateway that gave up all clear on their own.
        // Denied would make a five minute outage look like a configuration problem.
        for code in [429, 500, 502, 503] {
            let transport = Scripted::new(vec![status(code)]);
            let access = provider(true, transport).validate(&"gpt-test".into()).await;
            assert!(access.is_unknown(), "{code} gave {access:?}");
        }
    }

    #[tokio::test]
    async fn a_transport_that_could_not_connect_is_unknown() {
        let transport = Scripted::new(vec![Err(Error::Transient("connection refused".into()))]);
        let access = provider(true, transport).validate(&"gpt-test".into()).await;
        assert!(access.is_unknown(), "{access:?}");
    }

    #[tokio::test]
    async fn a_model_list_that_could_not_be_read_is_unknown_rather_than_denied() {
        // The vendor answered and this crate could not read it. That says nothing about the
        // credential or the model, so it is doubt rather than a refusal.
        let transport = Scripted::new(vec![Ok(HttpResponse::new(
            200,
            b"{\"models\":[]}".to_vec(),
        ))]);
        let access = provider(true, transport).validate(&"gpt-test".into()).await;

        assert!(access.is_unknown(), "{access:?}");
        assert!(
            access
                .detail()
                .unwrap_or_default()
                .contains("could not be read"),
            "{access}"
        );
    }

    #[tokio::test]
    async fn a_reply_that_was_not_json_at_all_is_unknown() {
        let transport = Scripted::new(vec![Ok(HttpResponse::new(200, b"<html>".to_vec()))]);
        let access = provider(true, transport).validate(&"gpt-test".into()).await;
        assert!(access.is_unknown(), "{access:?}");
    }

    #[tokio::test]
    async fn validate_sends_one_get_and_never_touches_the_chat_endpoint() {
        // The property that keeps a preflight free. A validate that cost a call would be
        // called once, then wrapped in a flag, then skipped.
        let transport = Scripted::new(vec![listing(&["gpt-test"])]);
        let _ = provider(true, transport.clone())
            .validate(&"gpt-test".into())
            .await;

        let seen = transport.urls();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].0, Method::Get, "{seen:?}");
        assert!(seen[0].1.ends_with("/models"), "{seen:?}");
        assert!(!seen[0].1.contains("/chat"), "{seen:?}");
    }

    #[tokio::test]
    async fn the_answer_is_not_remembered_between_calls() {
        // A credential rotates and a subscription lapses. An answer kept from earlier is a
        // claim about a moment that has passed, so the second call must ask again.
        let transport = Scripted::new(vec![status(401), listing(&["gpt-test"])]);
        let provider = provider(true, transport.clone());

        let first = provider.validate(&"gpt-test".into()).await;
        let second = provider.validate(&"gpt-test".into()).await;

        assert!(first.is_denied(), "{first:?}");
        assert_eq!(second, Access::Ready, "{second:?}");
        assert_eq!(transport.urls().len(), 2);
    }
}
