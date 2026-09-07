//! Amazon Bedrock, and the models several vendors serve through it.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | [`crate::Reach::CloudPartner`] | `InvokeModel`, for the Anthropic model family |
//!
//! # Why this is a top level node and not a folder inside `anthropic`
//!
//! Because the top level of `providers::` names **who you reach and whose credential pays**,
//! and for Bedrock that is Amazon. Claude through Bedrock is not Anthropic answering: a
//! different endpoint, a different credential, a different company holding your prompt.
//! `docs/DESIGN.md` has the argument, including the friendlier arrangement that was rejected
//! and what it would have cost.
//!
//! This is also the first thing to use [`crate::Reach::CloudPartner`] for what it was added
//! for. Every route through here reports that reach rather than `FirstPartyApi`, so a
//! program deciding where a prompt may go gets the true answer.
//!
//! # Making a call
//!
//! There is no key argument and there is deliberately no SigV4 here. Read
//! [`api`]'s header for why, and **`docs/BEDROCK.md` for the worked example**: what has to be
//! signed and in what order, where the region comes from, what the colon in a model id does
//! to a canonical URI, and why credentials that rotate belong in the transport.
//!
//! The seam is [`crate::HttpTransport`], and the wrapper is small enough to read:
//!
//! ```no_run
//! use llmr::transport::{ByteStream, HttpRequest, HttpResponse, HttpTransport};
//! use std::sync::Arc;
//!
//! /// Whatever you already use to sign, behind one method.
//! trait Signer: Send + Sync {
//!     /// Returns the request with `x-amz-date`, `authorization` and, for temporary
//!     /// credentials, `x-amz-security-token` attached.
//!     fn sign(&self, request: HttpRequest) -> llmr::Result<HttpRequest>;
//! }
//!
//! /// Signs, then hands the request to whatever really sends it.
//! struct Signing {
//!     inner: Arc<dyn HttpTransport>,
//!     signer: Arc<dyn Signer>,
//! }
//!
//! #[async_trait::async_trait]
//! impl HttpTransport for Signing {
//!     async fn send(&self, request: HttpRequest) -> llmr::Result<HttpResponse> {
//!         self.inner.send(self.signer.sign(request)?).await
//!     }
//!
//!     // Both, always. A wrapper that signs one and forwards the other works until the
//!     // first call down the unsigned path, and then fails with a 403 that says nothing
//!     // about which of the two was wrong.
//!     async fn send_streaming(&self, request: HttpRequest) -> llmr::Result<ByteStream> {
//!         self.inner.send_streaming(self.signer.sign(request)?).await
//!     }
//! }
//! ```
//!
//! Signing is the last thing that touches the request, because a signature covers the
//! request as it will be sent. That is why this is a transport wrapper and not a step
//! anywhere earlier: by the time [`crate::HttpTransport`] is handed an
//! [`HttpRequest`](crate::HttpRequest), the URL is final and the protocol's headers are on.
//!
//! # One family so far
//!
//! Bedrock is one address for many vendors' models, and each family takes a different body.
//! [`api::anthropic_family`] speaks the one Anthropic's models take, which is the Messages
//! shape this crate already translates — and it reuses that translation rather than copying
//! it, so a fix to one is a fix to both. Another family means another constructor beside it.

#[cfg(feature = "bedrock")]
#[cfg_attr(docsrs, doc(cfg(feature = "bedrock")))]
pub mod api;
