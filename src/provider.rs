//! The one trait every provider implements.

use crate::chat::request::ChatRequest;
use crate::chat::response::ChatResponse;
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
