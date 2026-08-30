//! OpenAI, by every reach this crate has — and, through `api`, most of the rest.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | you say | Anything speaking `/v1/chat/completions` |
//! | `cli` | [`crate::Reach::LocalCli`] | The Codex tool on this machine |
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
#[cfg_attr(docsrs, doc(cfg(feature = "openai")))]
pub mod api;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;
