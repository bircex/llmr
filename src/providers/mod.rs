//! The providers that ship with this crate.
//!
//! # Two ways in, and they are for different people
//!
//! **A vendor module is where you start.** `anthropic` and `openai` each hold every way this
//! crate can reach that vendor, one module per reach:
//!
//! ```text
//! providers::anthropic::api   the Messages API
//! providers::anthropic::cli   the Claude Code tool
//! providers::openai::api      anything speaking /v1/chat/completions
//! providers::openai::cli      the Codex tool
//! ```
//!
//! Which vendor is what a caller knows first, and the same models turn up behind more than
//! one reach: Anthropic's are reachable over the API and through Claude Code, and the two
//! differ in what they can carry rather than in what they are. Grouping by vendor puts that
//! choice in one place instead of two directories apart.
//!
//! **A reach module is where you extend.** [`api`] and `cli` hold the machinery every
//! provider of that kind shares:
//!
//! * [`api::Protocol`] and [`api::ApiProvider`] — the transport, the credential, the status
//!   codes and the error mapping, so a network provider writes only what URL, what headers,
//!   what JSON.
//! * `cli::LocalCli` — the spawning, the deadline, the kill on drop and the difference
//!   between a missing binary and a silent one, so a tool is a program name, its arguments
//!   and the shape of what it prints.
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
//! A module path could never be read that way. `providers::anthropic::cli` and
//! `providers::anthropic::api` are the same vendor and the same models, and they are not
//! the same place for a prompt to go — `capabilities()` is what says so.
//!
//! # Features
//!
//! Each way in is behind a feature, so a program that only reaches a local tool does not
//! build an HTTP client and a TLS stack. A vendor module exists when any of its reaches is
//! enabled, so `anthropic` alone gives you `anthropic::api` and no `anthropic::cli`.

pub mod api;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;

#[cfg(any(feature = "anthropic", feature = "cli"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "anthropic", feature = "cli"))))]
pub mod anthropic;

#[cfg(any(feature = "openai", feature = "cli"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "openai", feature = "cli"))))]
pub mod openai;
