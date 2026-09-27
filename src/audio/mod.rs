//! Speech from text and text from speech.
//!
//! Two traits, because they are two different calls that happen to share a medium: one
//! takes text and hands back a recording, the other takes a recording and hands back text.
//! Neither is a conversation, so neither is a method on [`crate::Provider`].
//!
//! # A recording says what it is
//!
//! [`Speech`] carries its media type beside the bytes. Providers disagree about what they
//! send by default — one answers MP3, another raw samples with no header at all — and bytes
//! handed on without their type are a file nobody can play.

use crate::cost::usage::Usage;
use crate::model::ModelId;
use crate::Result;
use async_trait::async_trait;

/// Text to read aloud.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SpeechRequest {
    /// Which model to ask.
    pub model: ModelId,
    /// What to say.
    pub input: String,
    /// Whose voice, in the provider's words.
    pub voice: String,
    /// The format to answer in: `mp3`, `wav`, `opus`, `aac`, `flac` or `pcm`. `None` is the
    /// provider's default, and [`Speech::media_type`] says what that was.
    pub format: Option<String>,
    /// How fast, where `1.0` is normal. `None` is normal.
    pub speed: Option<f32>,
    /// How to say it — tone, pace, accent — where the model takes direction.
    pub instructions: Option<String>,
}

impl SpeechRequest {
    /// A request for one model to say this in this voice.
    pub fn new(
        model: impl Into<ModelId>,
        input: impl Into<String>,
        voice: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            input: input.into(),
            voice: voice.into(),
            format: None,
            speed: None,
            instructions: None,
        }
    }

    /// Asks for this format.
    #[must_use]
    pub fn with_format(mut self, format: impl Into<String>) -> Self {
        self.format = Some(format.into());
        self
    }
}

/// A recording that came back.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Speech {
    /// What the bytes are: `audio/mpeg`, `audio/wav`.
    pub media_type: String,
    /// The recording.
    pub data: Vec<u8>,
    /// Which model actually served this.
    pub model: ModelId,
    /// What the call consumed, as far as the provider reported it.
    pub usage: Usage,
}

impl Speech {
    /// A reply. This is how a provider outside this crate builds one.
    pub fn new(media_type: impl Into<String>, data: Vec<u8>, model: ModelId, usage: Usage) -> Self {
        Self {
            media_type: media_type.into(),
            data,
            model,
            usage,
        }
    }
}

/// Something that reads text aloud.
///
/// # Errors
///
/// A format, speed or direction the model cannot honour is [`crate::Error::Unsupported`]
/// before anything is sent, never a recording in a different format.
#[async_trait]
pub trait SpeechSynthesizer: Send + Sync {
    /// A short name, recorded beside every call.
    fn id(&self) -> &str;

    /// Reads the text aloud.
    async fn speak(&self, request: SpeechRequest) -> Result<Speech>;
}

/// A recording to write down.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TranscriptionRequest {
    /// Which model to ask.
    pub model: ModelId,
    /// The recording.
    pub audio: Vec<u8>,
    /// Its file name. Providers read the format from the extension, so it matters.
    pub file_name: String,
    /// What it is: `audio/mpeg`, `audio/wav`.
    pub media_type: String,
    /// The spoken language, as ISO 639-1, when the caller knows it.
    pub language: Option<String>,
    /// Text that came before, or words to expect, to steer the spelling.
    pub prompt: Option<String>,
    /// How much to vary, from 0 to 1. `None` is the model's default.
    pub temperature: Option<f32>,
}

impl TranscriptionRequest {
    /// A request for one model to write down this recording.
    pub fn new(
        model: impl Into<ModelId>,
        audio: Vec<u8>,
        file_name: impl Into<String>,
        media_type: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            audio,
            file_name: file_name.into(),
            media_type: media_type.into(),
            language: None,
            prompt: None,
            temperature: None,
        }
    }
}

/// What was said.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Transcription {
    /// The words.
    pub text: String,
    /// Which model actually served this.
    pub model: ModelId,
    /// What the call consumed, when it was reported in tokens.
    pub usage: Usage,
    /// How long the recording was, when the provider billed by the second and said so.
    pub seconds: Option<f64>,
}

impl Transcription {
    /// A reply. This is how a provider outside this crate builds one.
    pub fn new(text: impl Into<String>, model: ModelId, usage: Usage) -> Self {
        Self {
            text: text.into(),
            model,
            usage,
            seconds: None,
        }
    }

    /// The same reply, with the length the provider billed.
    #[must_use]
    pub fn with_seconds(mut self, seconds: Option<f64>) -> Self {
        self.seconds = seconds;
        self
    }
}

/// Something that writes down what was said.
#[async_trait]
pub trait Transcriber: Send + Sync {
    /// A short name, recorded beside every call.
    fn id(&self) -> &str;

    /// Writes the recording down.
    async fn transcribe(&self, request: TranscriptionRequest) -> Result<Transcription>;
}
