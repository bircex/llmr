//! Providers reached over the network.
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

use crate::chat::{ChatRequest, ChatResponse};
use crate::error::{Error, Result};
use crate::model::{ModelCapabilities, ModelId, Reach};
use crate::provider::Provider;
use crate::registry::Registry;
use crate::secret::Secret;
use crate::transport::{HttpRequest, HttpTransport};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

#[cfg(feature = "anthropic")]
#[cfg_attr(docsrs, doc(cfg(feature = "anthropic")))]
pub mod anthropic;

#[cfg(feature = "openai")]
#[cfg_attr(docsrs, doc(cfg(feature = "openai")))]
pub mod openai;

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
}
