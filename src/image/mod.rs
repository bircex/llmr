//! Pictures from a prompt, which is a different question from chat.
//!
//! A prompt goes in and pictures come out: no conversation, no stop reason, no tools. Its own
//! trait for the reason [`crate::embed`] is one, and it shares the same things a caller
//! relies on — [`crate::Error`] and its retry advice, [`Usage`] with its absent-is-not-zero
//! rule, and the transport boundary.
//!
//! # Bytes, or a link the provider holds
//!
//! Some providers hand back the picture and some hand back a link to where they keep it for
//! an hour. [`Picture`] says which it got rather than fetching the link: fetching it would
//! mean this crate reaching a host the caller never named, and a link that expires is still
//! the caller's to decide about.

use crate::cost::usage::Usage;
use crate::model::ModelId;
use crate::Result;
use async_trait::async_trait;

/// A picture to make.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ImageRequest {
    /// Which model to ask.
    pub model: ModelId,
    /// What to draw.
    pub prompt: String,
    /// How many pictures. `None` is the provider's own default, which is one everywhere.
    pub count: Option<u32>,
    /// The size as `WIDTHxHEIGHT`, or `auto`. `None` is the model's default.
    pub size: Option<String>,
    /// The quality, in the provider's words: `low`, `high`, `hd`.
    pub quality: Option<String>,
    /// `transparent`, `opaque` or `auto`, where the model can be asked.
    pub background: Option<String>,
    /// `png`, `jpeg` or `webp`, where the model can be asked.
    pub output_format: Option<String>,
    /// `b64_json` or `url`, for the models that answer either way. `None` sends nothing, and
    /// the model answers the way it always does.
    pub response_format: Option<String>,
}

impl ImageRequest {
    /// A request for one model with one prompt, everything else left to the model.
    pub fn new(model: impl Into<ModelId>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            prompt: prompt.into(),
            count: None,
            size: None,
            quality: None,
            background: None,
            output_format: None,
            response_format: None,
        }
    }

    /// Asks for this many pictures.
    #[must_use]
    pub fn with_count(mut self, count: u32) -> Self {
        self.count = Some(count);
        self
    }

    /// Asks for this size, written `WIDTHxHEIGHT`.
    #[must_use]
    pub fn with_size(mut self, size: impl Into<String>) -> Self {
        self.size = Some(size.into());
        self
    }
}

/// One picture that came back.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Picture {
    /// The picture itself.
    Bytes {
        /// `image/png`, `image/jpeg` or `image/webp`.
        media_type: String,
        /// The encoded image.
        data: Vec<u8>,
    },
    /// A link to where the provider keeps it, usually for a limited time.
    Url(String),
}

/// What came back from one call.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Images {
    /// The pictures, in the order the provider gave them.
    pub pictures: Vec<Picture>,
    /// The prompt as the model rewrote it before drawing, when it says.
    pub revised_prompt: Option<String>,
    /// Which model actually served this.
    pub model: ModelId,
    /// What the call consumed, as far as the provider reported it. [`Usage::absent`] when it
    /// reported nothing, which most do: a picture is not priced in tokens everywhere.
    pub usage: Usage,
}

impl Images {
    /// A reply. This is how a provider outside this crate builds one.
    pub fn new(pictures: Vec<Picture>, model: ModelId, usage: Usage) -> Self {
        Self {
            pictures,
            revised_prompt: None,
            model,
            usage,
        }
    }

    /// The same reply, with the prompt the model drew from.
    #[must_use]
    pub fn with_revised_prompt(mut self, prompt: Option<String>) -> Self {
        self.revised_prompt = prompt;
        self
    }
}

/// Something that makes pictures.
///
/// # Errors
///
/// The same variants a chat call returns, meaning the same things. A request the model
/// cannot honour — a size it does not draw, more pictures than it makes at once, a link
/// when it only hands back bytes — is [`crate::Error::Unsupported`] before anything is sent,
/// never a picture of a different size.
#[async_trait]
pub trait ImageGenerator: Send + Sync {
    /// A short name, recorded beside every call.
    fn id(&self) -> &str;

    /// Makes the pictures.
    async fn generate(&self, request: ImageRequest) -> Result<Images>;
}
