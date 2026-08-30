//! `InvokeModel`, for the models Anthropic serves through Bedrock.
//!
//! # Signing is the transport's job, not this protocol's
//!
//! Bedrock authenticates with SigV4, which signs the **whole HTTP request**: the method, the
//! path, the query, a set of headers, and a hash of the body. That is exactly the surface
//! [`crate::HttpTransport`] sees and a [`Protocol`] does not — a protocol writes JSON and has
//! no idea what URL or headers the shared machinery is about to attach.
//!
//! It also needs state a protocol is not allowed to have. A signature covers a timestamp, so
//! signing needs a clock; it covers the region; and it needs credentials that rotate. Every
//! `Protocol` method here is a pure function, deliberately, and that is what makes one
//! instance safe to share across any number of concurrent calls.
//!
//! So this crate ships the translation and you supply a transport that signs. That is the
//! same bargain the crate already makes for HTTP itself — `reqwest` is a feature, not a
//! requirement — and it means you sign with whatever your program already uses, `aws-sigv4`
//! and `aws-config` most likely, rather than with an implementation of SigV4 written here
//! that nobody could test against the real thing.
//!
//! ```no_run
//! # use llmr::transport::{HttpRequest, HttpResponse, HttpTransport};
//! # use std::sync::Arc;
//! /// Wraps any transport and signs on the way past.
//! struct Signed<T>(T);
//!
//! #[async_trait::async_trait]
//! impl<T: HttpTransport> HttpTransport for Signed<T> {
//!     async fn send(&self, mut request: HttpRequest) -> llmr::Result<HttpResponse> {
//!         // Add the SigV4 headers here, over the request as it stands.
//!         request = request.with_header("authorization", "AWS4-HMAC-SHA256 ...");
//!         self.0.send(request).await
//!     }
//! }
//! ```
//!
//! # No streaming
//!
//! Bedrock streams over its own binary event framing rather than server sent events, so the
//! frame reader in [`crate::providers::api`] cannot read it. [`Protocol::stream_body`] is
//! left at its default, which means [`crate::Provider::stream`] falls back to one whole call
//! handed over as a burst. That is an answer rather than a refusal, and
//! [`crate::ModelCapabilities::streaming`] is what says it is not the real thing.

use crate::chat::{ChatRequest, ChatResponse};
use crate::error::Result;
use crate::model::{ModelId, Reach};
use crate::providers::anthropic::api::Messages;
use crate::providers::api::{ApiProvider, Protocol};
use crate::registry::Registry;
use crate::secret::Secret;
use crate::transport::HttpTransport;
use serde_json::Value;
use std::sync::Arc;

/// The version string Bedrock wants in place of a model name.
const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// `InvokeModel`, speaking the body Anthropic's models take.
///
/// Holds nothing, like every protocol here. The translation is [`Messages`]', reused rather
/// than copied: Claude through Bedrock and Claude direct should send the same JSON, and two
/// copies of that translation would disagree the first time one was fixed.
#[derive(Debug, Clone, Copy, Default)]
pub struct InvokeModel;

/// Bedrock, ready to call.
pub type Bedrock = ApiProvider<InvokeModel>;

/// The endpoint for a region.
pub fn endpoint(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com")
}

/// A provider for Anthropic's models in one region.
///
/// **The transport must sign.** There is no key argument because SigV4 is not a bearer token
/// and does not fit one: the credential belongs to the transport, along with the clock and
/// the region the signature covers. See this module's header for the shape of one.
///
/// The reach is [`Reach::CloudPartner`] and is not a choice. A prompt sent through here goes
/// to Amazon on an Amazon credential, whoever trained the model, and a program asking where
/// its data may go must be told that rather than `FirstPartyApi`.
pub fn anthropic_family(
    region: &str,
    transport: Arc<dyn HttpTransport>,
    registry: Arc<Registry>,
) -> Bedrock {
    ApiProvider::new(
        InvokeModel,
        endpoint(region),
        transport,
        // Nothing reads this. The credential is the signing transport's, and an argument
        // here would be one a caller had to invent something for.
        Secret::new("bedrock-signed-by-the-transport", ""),
        Reach::CloudPartner,
        registry,
    )
}

impl Protocol for InvokeModel {
    fn id(&self) -> &str {
        "bedrock"
    }

    /// The model is in the path, and its id is region qualified.
    ///
    /// `anthropic.claude-sonnet-5-v1:0`, or a cross region profile like
    /// `eu.anthropic.claude-sonnet-5-v1:0`. [`ModelId`] is a string for exactly this reason:
    /// a type that parsed model names would reject half of these.
    fn chat_url(&self, base_url: &str, model: &ModelId) -> String {
        format!("{base_url}/model/{}/invoke", model.as_str())
    }

    fn headers(&self, _key: &Secret) -> Result<Vec<(String, String)>> {
        // No credential here on purpose. The transport signs, and a bearer token attached
        // beside a signature is at best ignored and at worst a request Bedrock rejects.
        Ok(vec![
            ("content-type".into(), "application/json".into()),
            ("accept".into(), "application/json".into()),
        ])
    }

    fn body(&self, request: &ChatRequest) -> Result<Value> {
        let mut body = Messages.body(request)?;

        // The model is addressed by URL here, and Bedrock rejects a body that also names
        // one. In its place goes the version of the Anthropic schema being spoken.
        if let Some(object) = body.as_object_mut() {
            object.remove("model");
        }
        body["anthropic_version"] = Value::String(ANTHROPIC_VERSION.to_string());

        Ok(body)
    }

    fn read(&self, body: &Value, asked_for: &ModelId) -> Result<ChatResponse> {
        // The reply is the Messages reply, unchanged. Reading it any other way here would be
        // a second implementation of the part that is hardest to get right — the signature on
        // a thinking block, the opaque blocks, the uncached remainder in the usage.
        Messages.read(body, asked_for)
    }

    // No `catalogue_url`. Bedrock lists models through its control plane on a different host
    // with a different action, which is a separate credential and a separate reach. Answering
    // `Unsupported` is honest; an empty list would read as Amazon serving nothing.
}
