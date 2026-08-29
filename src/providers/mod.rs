//! The providers that ship with this crate, grouped by how they are reached.
//!
//! The split is not cosmetic. An API provider and a command line one differ in what they
//! can carry, what they report, and whose credential pays, and those differences are what
//! [`crate::Reach`] exists to name. Two directories keep the difference visible in the file
//! tree rather than only in a doc comment.
//!
//! Each is behind a feature, so a program that only reaches a local tool does not build an
//! HTTP client and a TLS stack.

#[cfg(any(feature = "anthropic", feature = "openai"))]
#[cfg_attr(docsrs, doc(cfg(any(feature = "anthropic", feature = "openai"))))]
pub mod api;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;
