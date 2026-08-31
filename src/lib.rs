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

#[cfg(feature = "embeddings")]
#[cfg_attr(docsrs, doc(cfg(feature = "embeddings")))]
pub mod embed;

pub mod error;
pub mod model;
mod observe;
pub mod provider;
pub mod registry;
pub mod retry;
pub mod router;
pub mod secret;
pub mod transport;

pub mod providers;

#[cfg(feature = "testkit")]
#[cfg_attr(docsrs, doc(cfg(feature = "testkit")))]
pub mod testkit;

pub use chat::{
    ChatRequest, ChatResponse, ContentBlock, Effort, Event, EventStream, Generation, ImageSource,
    Message, Needs, Role, StopReason, Thinking, ToolSchema, Transcript,
};
pub use cost::{Ledger, Micros, PriceBook, Priced, Rate, Recheck, Usage, UsageCoverage};
#[cfg(feature = "embeddings")]
pub use embed::{EmbedRequest, Embedder, Embedding, EmbeddingCapabilities, Embeddings, Purpose};
pub use error::{Error, Result};
pub use model::{ModelCapabilities, ModelId, Reach};
pub use provider::{Access, Provider};
pub use registry::Registry;
pub use retry::{Delay, Retry};
pub use router::{Requirements, Route, Routed, Router};
pub use secret::Secret;
pub use transport::{HttpRequest, HttpResponse, HttpTransport, Method};
