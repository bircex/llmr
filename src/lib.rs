#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![warn(clippy::missing_errors_doc)]
#![warn(clippy::missing_panics_doc)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod error;
pub mod message;
pub mod model;
pub mod pricing;
pub mod provider;
pub mod registry;
pub mod request;
pub mod response;
pub mod secret;
pub mod usage;

#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub mod http;

/// The providers that ship with this crate.
pub mod providers;

#[cfg(feature = "testkit")]
#[cfg_attr(docsrs, doc(cfg(feature = "testkit")))]
pub mod testkit;

pub use error::{Error, Result};
pub use message::{ContentBlock, Message, Role, StopReason};
pub use model::{ModelCapabilities, ModelId, Reach};
pub use pricing::{Micros, PriceBook, Priced, Rate};
pub use provider::Provider;
pub use registry::Registry;
pub use request::{ChatRequest, Effort, Generation, Needs, Thinking, ToolSchema};
pub use response::ChatResponse;
pub use secret::Secret;
pub use usage::{Usage, UsageCoverage};
