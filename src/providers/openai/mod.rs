//! OpenAI, by every reach this crate has — and, through `api`, most of the rest.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | you say | Anything speaking `/v1/chat/completions` |
//! | `embed` | you say | Anything speaking `/v1/embeddings` |
//! | `image` | you say | Anything speaking `/v1/images/generations` |
//! | `audio` | you say | Anything speaking `/v1/audio/speech` and `/v1/audio/transcriptions` |
//!
//! # `api` is a shape, not a vendor
//!
//! This is the one module in the tree whose name is wider than what it holds. OpenAI,
//! Groq, Together, Fireworks, vLLM, Ollama, LM Studio, OpenRouter and LiteLLM all answer at
//! `/v1/chat/completions` with the same envelope, so the base URL is a constructor argument
//! and one protocol covers every one of them. It sits here because the shape is OpenAI's and
//! that is what the ecosystem calls it, not because a caller reaching Ollama is reaching
//! OpenAI.
//!
//! Which is why `api` is the only provider in this crate whose reach you supply. Everywhere
//! else the module name settles it; here a model on your laptop and a hosted API are the same
//! JSON over the same path, and nothing in a request can tell them apart. Guessing would mean
//! guessing where a prompt is allowed to go, so it is asked for instead — see
//! [`crate::Reach`] for what the answer changes.

#[cfg(feature = "openai")]
pub mod api;

// Both features, because this is the embeddings trait spoken in the OpenAI shape and needs
// each half. It is a sibling of `api` rather than something inside it: embeddings are a
// different request, a different reply and a different trait, and the only thing the two
// share is a base URL and a key.
#[cfg(all(feature = "openai", feature = "embeddings"))]
pub mod embed;

// Pictures and speech in the same shape, each behind its own feature like embeddings.
#[cfg(all(feature = "openai", feature = "image-generation"))]
pub mod image;

#[cfg(all(feature = "openai", feature = "audio"))]
pub mod audio;
