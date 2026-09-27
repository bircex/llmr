//! The providers that ship with this crate.
//!
//! # Two ways in, and they are for different people
//!
//! **A top level module is where you start.** Each one names **who you reach and whose
//! credential pays** — the vendor for a first party API, the gateway for a gateway — and
//! holds one module per reach:
//!
//! ```text
//! providers::anthropic::api   the Messages API
//! providers::openai::api      anything speaking /v1/chat/completions
//! providers::gemini::api      Gemini's generateContent
//! providers::bedrock::api     Anthropic's models through Amazon
//! ```
//!
//! Who you are reaching is what a caller knows first, and the same models turn up behind more
//! than one of these: Anthropic's answer over the Messages API, and through Amazon. Those differ in what they can carry, in whose credential pays and in which
//! company ends up holding the prompt — so `bedrock` is its own node rather than a folder
//! inside `anthropic`, because Claude through Bedrock is not Anthropic answering.
//!
//! `docs/DESIGN.md` has that argument in full, including the friendlier arrangement that was
//! rejected and what it would have cost.
//!
//! **A reach module is where you extend.** [`api`] holds the machinery every network
//! provider shares: [`api::Protocol`] and [`api::ApiProvider`] — the transport, the
//! credential, the status codes and the error mapping, so a provider writes only what URL,
//! what headers, what JSON.
//!
//! That split is the point. What is *shared* follows the reach, because reach is what
//! decides how a model is spoken to. What is *chosen* follows the vendor, because that is
//! what a caller picks. The files under `anthropic/` and `openai/` are short for exactly
//! this reason: the engine is not in them.
//!
//! # Reach is still not a directory
//!
//! Grouping by vendor does not soften what [`crate::Reach`] is for. Where a model runs
//! decides where your data goes and whose credential pays, and that answer travels on
//! [`crate::ModelCapabilities`], at runtime, where a caller can read it before sending.
//! A module path could never be read that way. `providers::openai::api` pointed at OpenAI and
//! at a server on your own machine is the same module, and they are not the same place for a
//! prompt to go — `capabilities()` is what says so.
//!
//! # Features
//!
//! Each protocol is behind a feature, so a program that speaks only one does not build the
//! others' translations. A vendor module exists when any of its reaches is enabled.

pub mod api;

#[cfg(feature = "anthropic")]
pub mod anthropic;

#[cfg(feature = "bedrock")]
pub mod bedrock;

#[cfg(feature = "gemini")]
pub mod gemini;

#[cfg(feature = "openai")]
pub mod openai;
