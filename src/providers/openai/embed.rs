//! Any endpoint speaking the OpenAI embeddings shape.
//!
//! The same bargain as [`super::api`]: a shape rather than a vendor. OpenAI, vLLM, Ollama,
//! LM Studio, Together and the rest answer at `/v1/embeddings` with the same envelope, so
//! the base URL is a constructor argument and the reach is given rather than guessed.
//!
//! # What this provider checks that the endpoint does not
//!
//! **The reply is put back in the order the inputs were given.** OpenAI sends an `index` on
//! every row precisely because the array is not promised to be ordered. Trusting arrival
//! order pairs every document with another document's vector, and nothing downstream fails:
//! the index builds, the queries run, and the results are wrong.
//!
//! **A short batch is an error rather than a short list.** Fewer vectors than inputs cannot
//! be lined up, and guessing which input was dropped is worse than failing.
//!
//! **A dimension count that was asked for and not honoured is refused.** A caller asking for
//! 256 has sized something for 256. An endpoint that ignores the parameter returns full
//! length vectors with a 200, which is the failure this crate refuses images for.

use crate::cost::usage::Usage;
use crate::embed::{EmbedRequest, Embedder, Embedding, EmbeddingCapabilities, Embeddings};
use crate::error::{Error, Result};
use crate::model::{ModelId, Reach};
use crate::provider::Access;
use crate::secret::Secret;
use crate::transport::{HttpRequest, HttpTransport};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Anything speaking the OpenAI embeddings shape.
///
/// Immutable once built, like every provider here, so one instance serves any number of
/// concurrent calls with nothing to contend on.
pub struct OpenAiEmbeddings {
    id: &'static str,
    base_url: String,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
    reach: Reach,
    known: BTreeMap<String, EmbeddingCapabilities>,
}

/// An embedder at a base URL.
///
/// The id goes into every record beside the calls it made. The base URL should include the
/// version prefix the endpoint expects, usually `/v1`.
///
/// It knows no models to begin with, so [`Embedder::capabilities`] answers `None` — "this
/// embedder does not know", which is the honest answer until somebody writes a row down.
/// Add what you have checked with [`OpenAiEmbeddings::knowing`].
pub fn at(
    id: &'static str,
    base_url: impl Into<String>,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
    reach: Reach,
) -> OpenAiEmbeddings {
    OpenAiEmbeddings {
        id,
        base_url: base_url.into().trim_end_matches('/').to_string(),
        transport,
        key,
        reach,
        known: BTreeMap::new(),
    }
}

/// OpenAI's own embeddings API, reading `OPENAI_API_KEY` from the environment.
///
/// Ships no capability table, for the same reason [`super::api::from_env`] ships no model
/// registry: rows nobody verified would invent exactly the provenance this crate exists to
/// record. Add your own with [`OpenAiEmbeddings::knowing`].
///
/// # Errors
///
/// [`Error::Auth`] when the variable is unset or blank, and [`Error::Transient`] when the
/// HTTP client cannot be built.
#[cfg(feature = "reqwest")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
pub fn from_env(timeout: std::time::Duration) -> Result<OpenAiEmbeddings> {
    Ok(at(
        "openai-embeddings",
        "https://api.openai.com/v1",
        Arc::new(crate::transport::Reqwest::new(timeout)?),
        Secret::from_env("openai-api-key", "OPENAI_API_KEY")?,
        Reach::FirstPartyApi,
    ))
}

impl OpenAiEmbeddings {
    /// Records what you have checked about a model.
    ///
    /// Yours to supply and yours to date, exactly like [`crate::Registry`]. A dimension count
    /// this crate invented would be a database column of the wrong width.
    #[must_use]
    pub fn knowing(
        mut self,
        model: impl Into<String>,
        capabilities: EmbeddingCapabilities,
    ) -> Self {
        self.known.insert(model.into(), capabilities);
        self
    }

    /// Where this embedder's data goes.
    pub fn reach(&self) -> Reach {
        self.reach
    }

    /// The request, as this shape writes it.
    ///
    /// `encoding_format` is sent explicitly rather than left to the endpoint's default.
    /// Some servers answer base64 unless told otherwise, and a base64 string where a number
    /// array was expected is a parse failure at the far end of a batch.
    ///
    /// [`crate::embed::Purpose`] has no place to go in this shape, which is why
    /// [`EmbeddingCapabilities::purposes`] is never true for it. Nothing is dropped: for a
    /// model that does not distinguish, the vector is the same either way.
    fn body(&self, request: &EmbedRequest) -> Value {
        let mut body = json!({
            "model": request.model.as_str(),
            "input": request.inputs,
            "encoding_format": "float",
        });
        if let Some(dimensions) = request.dimensions {
            body["dimensions"] = json!(dimensions);
        }
        body
    }

    /// The reply, in the order the inputs were given.
    ///
    /// # Errors
    ///
    /// [`Error::Unreadable`] when the envelope is not the expected shape, when a row has no
    /// index or no vector, or when the count does not match what was sent.
    fn read(&self, body: &Value, request: &EmbedRequest) -> Result<Embeddings> {
        let served: ModelId = body
            .get("model")
            .and_then(Value::as_str)
            .map_or_else(|| request.model.clone(), Into::into);

        let rows = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unreadable("the reply had no data array".into()))?;

        // Index first, vector second, and both required. A row with no index cannot be put
        // anywhere, and putting it where it happened to arrive is the bug this whole
        // function exists to prevent.
        let mut placed: Vec<(usize, Embedding)> = Vec::with_capacity(rows.len());
        for (position, row) in rows.iter().enumerate() {
            let index = row
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| {
                    Error::Unreadable(format!(
                        "the embedding at position {position} carried no index, so nothing \
                         says which input it belongs to"
                    ))
                })?;

            let numbers = row
                .get("embedding")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Error::Unreadable(format!("the embedding at index {index} had no vector"))
                })?;

            let mut vector = Vec::with_capacity(numbers.len());
            for number in numbers {
                vector.push(number.as_f64().ok_or_else(|| {
                    Error::Unreadable(format!(
                        "the vector at index {index} held something that was not a number. \
                         An endpoint answering base64 does this"
                    ))
                })? as f32);
            }

            placed.push((index, Embedding::new(served.clone(), vector)));
        }

        placed.sort_by_key(|(index, _)| *index);

        // Not a warning. A batch that comes back short cannot be lined up with its inputs,
        // and a caller storing the result would attach every vector after the gap to the
        // wrong text.
        if placed.len() != request.inputs.len() {
            return Err(Error::Unreadable(format!(
                "{} inputs were sent and {} vectors came back, so nothing can be lined up",
                request.inputs.len(),
                placed.len()
            )));
        }
        if placed
            .iter()
            .enumerate()
            .any(|(position, (index, _))| position != *index)
        {
            return Err(Error::Unreadable(
                "the reply's indices are not one per input, so nothing can be lined up".into(),
            ));
        }

        let vectors: Vec<Embedding> = placed.into_iter().map(|(_, vector)| vector).collect();

        // Asked for and not honoured. The endpoint answered 200 with vectors of the wrong
        // width, and a caller who sized a database for the number they asked for has no
        // other way to find out.
        if let (Some(asked), Some(got)) = (
            request.dimensions,
            vectors.first().map(Embedding::dimensions),
        ) {
            if usize::try_from(asked).is_ok_and(|asked| asked != got) {
                return Err(Error::Unsupported(format!(
                    "{} vectors came back with {got} dimensions after {asked} were asked for, \
                     so this model does not resize",
                    self.id
                )));
            }
        }

        Ok(Embeddings::new(vectors, served, read_usage(body)))
    }
}

/// What the call consumed, when the endpoint said.
///
/// An embeddings reply carries `prompt_tokens` and nothing else worth having: text goes in
/// and a vector comes out, and a vector is not tokens. [`Usage::embedding`] is the shape
/// that says so, and reports [`crate::UsageCoverage::Exact`] rather than hedging about three
/// fields that did not happen.
///
/// A reply with no usage block at all is [`Usage::absent`], never zeros.
fn read_usage(body: &Value) -> Usage {
    body.get("usage")
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .map_or_else(Usage::absent, Usage::embedding)
}

#[async_trait]
impl Embedder for OpenAiEmbeddings {
    fn id(&self) -> &str {
        self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<EmbeddingCapabilities> {
        self.known.get(model.as_str()).copied()
    }

    async fn embed(&self, request: EmbedRequest) -> Result<Embeddings> {
        // Before the network, because an empty batch is a 400 from most endpoints and a
        // loop that reached the wire with nothing to say is a bug further up.
        if request.is_empty() {
            return Err(Error::InvalidRequest(
                "there is nothing to embed".to_string(),
            ));
        }

        // A dimension count asked of a model known not to resize. Refused here rather than
        // after the call, when a table says so; when no table does, the check after the
        // reply catches it instead.
        if let (Some(asked), Some(known)) = (request.dimensions, self.capabilities(&request.model))
        {
            if !known.resizable {
                return Err(Error::Unsupported(format!(
                    "{asked} dimensions were asked for and {} does not resize",
                    request.model.as_str()
                )));
            }
        }

        let http = HttpRequest::new(
            format!("{}/embeddings", self.base_url),
            serde_json::to_vec(&self.body(&request))
                .map_err(|e| Error::Unreadable(format!("the request would not serialise: {e}")))?,
        );

        let key = self
            .key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;
        let http = http
            .with_header("authorization", format!("Bearer {key}"))
            .with_header("content-type", "application/json");

        let response = self.transport.send(http).await?;
        response.check()?;

        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the reply was not JSON: {e}")))?;

        self.read(&body, &request)
    }

    async fn validate(&self, model: &ModelId) -> Access {
        // The model list is free, and it is the one thing that establishes the credential
        // and the entitlement together. A rejected key is settled and must not read as
        // "ask again later".
        let http = HttpRequest::get(format!("{}/models", self.base_url));
        let key = match self.key.expose_str() {
            Ok(key) => key,
            Err(_) => return Access::denied("the API key is not valid UTF-8"),
        };
        let http = http.with_header("authorization", format!("Bearer {key}"));

        let response = match self.transport.send(http).await {
            Ok(response) => response,
            Err(e) => return doubt_or_refusal(self.id, &e),
        };
        if let Err(e) = response.check() {
            return doubt_or_refusal(self.id, &e);
        }

        let body: Value = match serde_json::from_slice(&response.body) {
            Ok(body) => body,
            Err(e) => return Access::unknown(format!("{}: the list was not JSON: {e}", self.id)),
        };

        let listed = body
            .get("data")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("id").and_then(Value::as_str))
                    .any(|id| id == model.as_str())
            })
            .unwrap_or(false);

        if listed {
            Access::Ready
        } else {
            Access::denied(format!(
                "{} does not list {}. Either the name is wrong or this key cannot reach it",
                self.id,
                model.as_str()
            ))
        }
    }
}

/// A settled refusal or an unsettled moment.
///
/// The same split [`crate::providers::api`] makes, and for the same reason: an unknown reads
/// as "ask again later", so a credential a person has to fix must never arrive as one.
fn doubt_or_refusal(id: &str, e: &Error) -> Access {
    match e {
        Error::Auth(reason) => Access::denied(format!("{id}: {reason}")),
        Error::NotFound(reason) => Access::denied(format!("{id}: {reason}")),
        other => Access::unknown(format!("{id}: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::HttpResponse;
    use std::sync::Mutex;

    struct Scripted {
        replies: Mutex<Vec<Result<HttpResponse>>>,
        sent: Mutex<Vec<Vec<u8>>>,
    }

    #[async_trait]
    impl HttpTransport for Scripted {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.sent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.body.clone());
            let mut replies = self.replies.lock().unwrap_or_else(|e| e.into_inner());
            if replies.is_empty() {
                return Err(Error::Transient("the script ran out".into()));
            }
            replies.remove(0)
        }
    }

    fn scripted(replies: Vec<Result<HttpResponse>>) -> Arc<Scripted> {
        Arc::new(Scripted {
            replies: Mutex::new(replies),
            sent: Mutex::new(Vec::new()),
        })
    }

    fn embedder(transport: Arc<Scripted>) -> OpenAiEmbeddings {
        at(
            "test",
            "https://example.invalid/v1/",
            transport,
            Secret::new("test-key", "sk-test"),
            Reach::FirstPartyApi,
        )
    }

    fn reply(json: Value) -> Result<HttpResponse> {
        Ok(HttpResponse::new(200, json.to_string().into_bytes()))
    }

    fn row(index: u64, vector: &[f32]) -> Value {
        json!({ "object": "embedding", "index": index, "embedding": vector })
    }

    #[tokio::test]
    async fn vectors_come_back_in_the_order_the_inputs_went_out() {
        // The reply is deliberately out of order, which is why the vendor sends an index at
        // all. Trusting arrival order here pairs every document with the wrong vector, and
        // nothing downstream fails.
        let transport = scripted(vec![reply(json!({
            "data": [row(1, &[9.0]), row(0, &[1.0]), row(2, &[5.0])],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": 12, "total_tokens": 12 },
        }))]);

        let answer = embedder(transport)
            .embed(EmbedRequest::new(
                "text-embedding-3-small".into(),
                vec!["first".into(), "second".into(), "third".into()],
            ))
            .await
            .unwrap();

        assert_eq!(
            answer.into_vectors(),
            vec![vec![1.0], vec![9.0], vec![5.0]],
            "index 0 is the first input, whatever order it arrived in"
        );
    }

    #[tokio::test]
    async fn every_vector_carries_the_model_the_reply_named() {
        // Not the model that was asked for. A vector filed under the wrong name is one that
        // will be compared against a space it does not belong to.
        let transport = scripted(vec![reply(json!({
            "data": [row(0, &[1.0])],
            "model": "text-embedding-3-small-v2",
        }))]);

        let answer = embedder(transport)
            .embed(EmbedRequest::one("text-embedding-3-small", "hello"))
            .await
            .unwrap();

        assert_eq!(answer.model.as_str(), "text-embedding-3-small-v2");
        assert_eq!(
            answer.get(0).map(|v| v.model.as_str()),
            Some("text-embedding-3-small-v2")
        );
    }

    #[tokio::test]
    async fn a_batch_that_comes_back_short_is_an_error_rather_than_a_short_list() {
        let transport = scripted(vec![reply(json!({
            "data": [row(0, &[1.0])],
            "model": "m",
        }))]);

        let answer = embedder(transport)
            .embed(EmbedRequest::new(
                "m".into(),
                vec!["one".into(), "two".into()],
            ))
            .await;

        match answer {
            Err(Error::Unreadable(message)) => {
                assert!(message.contains("lined up"), "{message}");
            }
            other => panic!("a short batch has to fail: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_row_with_no_index_cannot_be_put_anywhere() {
        let transport = scripted(vec![reply(json!({
            "data": [{ "embedding": [1.0] }],
            "model": "m",
        }))]);

        match embedder(transport)
            .embed(EmbedRequest::one("m", "hello"))
            .await
        {
            Err(Error::Unreadable(message)) => assert!(message.contains("index"), "{message}"),
            other => panic!("guessing the position is the bug: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dimensions_asked_for_and_not_honoured_are_refused() {
        // The endpoint answered 200 with vectors of the wrong width. A caller who sized a
        // column for 256 has no other way to find out.
        let transport = scripted(vec![reply(json!({
            "data": [row(0, &[1.0, 2.0, 3.0, 4.0])],
            "model": "m",
        }))]);

        match embedder(transport)
            .embed(EmbedRequest::one("m", "hello").with_dimensions(2))
            .await
        {
            Err(Error::Unsupported(message)) => {
                assert!(message.contains("does not resize"), "{message}");
            }
            other => panic!("silently wrong width is the failure: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_model_known_not_to_resize_is_refused_before_the_network() {
        let transport = scripted(vec![]);
        let embedder = embedder(Arc::clone(&transport)).knowing(
            "text-embedding-ada-002",
            EmbeddingCapabilities::none(Reach::FirstPartyApi).with_dimensions(1_536),
        );

        let answer = embedder
            .embed(EmbedRequest::one("text-embedding-ada-002", "hello").with_dimensions(256))
            .await;

        assert!(matches!(answer, Err(Error::Unsupported(_))), "{answer:?}");
        assert!(
            transport.sent.lock().unwrap().is_empty(),
            "a request nobody had to send"
        );
    }

    #[tokio::test]
    async fn an_empty_batch_never_reaches_the_wire() {
        let transport = scripted(vec![]);
        let answer = embedder(Arc::clone(&transport))
            .embed(EmbedRequest::new("m".into(), vec![]))
            .await;

        assert!(
            matches!(answer, Err(Error::InvalidRequest(_))),
            "{answer:?}"
        );
        assert!(transport.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_reply_with_no_usage_is_absent_rather_than_nought() {
        let transport = scripted(vec![reply(json!({
            "data": [row(0, &[1.0])],
            "model": "m",
        }))]);

        let answer = embedder(transport)
            .embed(EmbedRequest::one("m", "hello"))
            .await
            .unwrap();

        assert_eq!(answer.usage, Usage::absent());
        assert_eq!(answer.usage.coverage(), crate::UsageCoverage::Absent);
    }

    #[tokio::test]
    async fn a_measured_call_reads_as_exact_rather_than_partial() {
        // The reason `Usage::embedding` exists. Left as one field of four, every embedding
        // call would turn a ledger total into a floor for good.
        let transport = scripted(vec![reply(json!({
            "data": [row(0, &[1.0])],
            "model": "m",
            "usage": { "prompt_tokens": 8, "total_tokens": 8 },
        }))]);

        let answer = embedder(transport)
            .embed(EmbedRequest::one("m", "hello"))
            .await
            .unwrap();

        assert_eq!(answer.usage.prompt_tokens(), Some(8));
        assert_eq!(answer.usage.coverage(), crate::UsageCoverage::Exact);
    }

    #[tokio::test]
    async fn the_request_says_float_and_carries_dimensions_only_when_asked() {
        let transport = scripted(vec![
            reply(json!({ "data": [row(0, &[1.0])], "model": "m" })),
            reply(json!({ "data": [row(0, &[1.0, 2.0])], "model": "m" })),
        ]);
        let embedder = embedder(Arc::clone(&transport));

        embedder
            .embed(EmbedRequest::one("m", "hello"))
            .await
            .unwrap();
        embedder
            .embed(EmbedRequest::one("m", "hello").with_dimensions(2))
            .await
            .unwrap();

        let sent = transport.sent.lock().unwrap();
        let plain: Value = serde_json::from_slice(&sent[0]).unwrap();
        let sized: Value = serde_json::from_slice(&sent[1]).unwrap();

        assert_eq!(plain["encoding_format"], "float");
        assert!(
            plain.get("dimensions").is_none(),
            "nothing asked, nothing sent"
        );
        assert_eq!(sized["dimensions"], 2);
        assert_eq!(sized["input"], json!(["hello"]));
    }

    #[tokio::test]
    async fn a_rejected_credential_is_denied_rather_than_unknown() {
        // The rule the whole `Access` split exists for. An unknown reads as "ask again
        // later", so the one failure a person has to fix would never surface.
        let transport = scripted(vec![Ok(HttpResponse::new(401, b"no".to_vec()))]);
        let answer = embedder(transport).validate(&"m".into()).await;
        assert!(answer.is_denied(), "{answer}");
    }

    #[tokio::test]
    async fn a_moment_rather_than_an_answer_is_unknown() {
        let transport = scripted(vec![Err(Error::Transient("the network went away".into()))]);
        let answer = embedder(transport).validate(&"m".into()).await;
        assert!(answer.is_unknown(), "{answer}");
    }

    #[tokio::test]
    async fn a_model_the_vendor_lists_is_ready_and_one_it_does_not_is_denied() {
        let listing = json!({ "data": [{ "id": "text-embedding-3-small" }] });
        let transport = scripted(vec![reply(listing.clone()), reply(listing)]);
        let embedder = embedder(transport);

        assert!(embedder
            .validate(&"text-embedding-3-small".into())
            .await
            .is_ready());
        assert!(embedder.validate(&"not-a-model".into()).await.is_denied());
    }
}
