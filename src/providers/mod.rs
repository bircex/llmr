//! The providers that ship with this crate.
//!
//! Each is behind a feature so that a build only compiles what it uses. A program that
//! reaches a model through a local command line tool has no reason to build an HTTP client
//! and a TLS stack.

#[cfg(feature = "anthropic")]
#[cfg_attr(docsrs, doc(cfg(feature = "anthropic")))]
pub mod anthropic;

#[cfg(feature = "openai")]
#[cfg_attr(docsrs, doc(cfg(feature = "openai")))]
pub mod openai;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;
