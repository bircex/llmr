#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![warn(clippy::missing_errors_doc)]
#![warn(clippy::missing_panics_doc)]
// The bans above are about the library. A test that cannot panic cannot assert, so they are
// lifted inside `#[cfg(test)]` and nowhere else.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod chat;
pub mod cost;
pub mod error;
pub mod model;
pub mod provider;
pub mod registry;
pub mod router;
pub mod secret;
pub mod transport;

pub mod providers;

#[cfg(feature = "testkit")]
#[cfg_attr(docsrs, doc(cfg(feature = "testkit")))]
pub mod testkit;

pub use chat::{
    ChatRequest, ChatResponse, ContentBlock, Effort, Event, EventStream, Generation, Message,
    Needs, Role, StopReason, Thinking, ToolSchema, Transcript,
};
pub use cost::{Micros, PriceBook, Priced, Rate, Usage, UsageCoverage};
pub use error::{Error, Result};
pub use model::{ModelCapabilities, ModelId, Reach};
pub use provider::Provider;
pub use registry::Registry;
pub use router::{Requirements, Route, Routed, Router};
pub use secret::Secret;
pub use transport::{HttpRequest, HttpResponse, HttpTransport, Method};
