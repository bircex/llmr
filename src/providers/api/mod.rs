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
use crate::provider::Provider;
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
        let body = serde_json::to_vec(&self.protocol.body(&request)?)
            .map_err(|e| Error::InvalidRequest(format!("building the request: {e}")))?;

        let parsed = self
            .call(HttpRequest::new(
                self.protocol.chat_url(&self.base_url),
                body,
            ))
            .await?;

        self.protocol.read(&parsed, &request.model)
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
