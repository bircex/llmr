//! Gemini's `batchEmbedContents` API.
//!
//! The second [`Embedder`] in this crate, and it is here for what it disagrees with rather
//! than for the vendor. A trait with one implementation is a description of that
//! implementation; these two differ on all three of the things [`crate::embed`] makes claims
//! about, so the trait had to be a specification or it had to break.
//!
//! # Three things this shape does that the OpenAI one does not
//!
//! **It has a place for [`Purpose`].** `taskType` distinguishes text being stored from text
//! being searched with, and the two are meant to be used together: a query vector is built to
//! land near the documents that answer it. This is the only reach in the crate where
//! [`EmbeddingCapabilities::purposes`] is true, and until it existed `Purpose` was an enum
//! nothing wrote to a wire.
//!
//! **Its reply carries no index.** OpenAI sends one on every row and this crate sorts by it.
//! There is nothing to sort by here: `batchEmbedContents` answers an array whose order *is*
//! the promise. So the check that remains is the count, and it is the one that matters —
//! `n` requests that come back as anything other than `n` embeddings cannot be lined up with
//! their inputs at all, and every vector after the gap would be filed under the wrong text.
//!
//! **It reports no usage.** No token block, in either the batch or the single form. So every
//! call answers [`crate::Usage::absent`], and a [`crate::Ledger`] holding one says "at least"
//! for the whole run. That is the honest reading and it is worth seeing: zeros here would
//! turn a cost nobody measured into a call that was free.
//!
//! # The model name is written twice
//!
//! Once in the path and once inside every element of the batch, because this API requires it
//! in both places and rejects a request carrying only one. Both are written from the same
//! [`crate::ModelId`], so they cannot disagree.

use crate::cost::usage::Usage;
use crate::embed::{EmbedRequest, Embedder, Embedding, EmbeddingCapabilities, Embeddings, Purpose};
use crate::error::{Error, Result};
use crate::model::{ModelId, Reach};
use crate::provider::Access;
use crate::secret::Secret;
use crate::transport::{HttpRequest, HttpTransport};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Google's own endpoint, the same one [`super::api`] uses.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Gemini's embeddings API.
///
/// Immutable once built, like every provider here, so one instance serves any number of
/// concurrent calls with nothing to contend on.
pub struct GeminiEmbeddings {
    base_url: String,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
    known: BTreeMap<String, EmbeddingCapabilities>,
}

/// An embedder at Google's endpoint, with a key you supply.
///
/// It knows no models to begin with, so [`Embedder::capabilities`] answers `None` — "this
/// embedder does not know", which is the honest answer until somebody writes a row down.
/// Add what you have checked with [`GeminiEmbeddings::knowing`].
///
/// The reach is [`Reach::FirstPartyApi`] and is not a parameter. Unlike the OpenAI shape,
/// which a dozen vendors and a laptop all speak, this endpoint is Google's.
pub fn with(transport: Arc<dyn HttpTransport>, key: Secret) -> GeminiEmbeddings {
    GeminiEmbeddings {
        base_url: DEFAULT_BASE_URL.to_string(),
        transport,
        key,
        known: BTreeMap::new(),
    }
}

/// An embedder reading `GEMINI_API_KEY` from the environment.
///
/// # Errors
///
/// [`Error::Auth`] when the variable is unset or blank, and [`Error::Transient`] when the
/// HTTP client cannot be built.
#[cfg(feature = "reqwest")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
pub fn from_env(timeout: std::time::Duration) -> Result<GeminiEmbeddings> {
    Ok(with(
        Arc::new(crate::transport::Reqwest::new(timeout)?),
        Secret::from_env("gemini-api-key", "GEMINI_API_KEY")?,
    ))
}

/// How this API spells a [`Purpose`].
///
/// Only the two the crate has. `taskType` has more values — classification, clustering,
/// similarity — and adding one here without a caller who needs it would be guessing at what
/// somebody meant.
fn task_type(purpose: Purpose) -> &'static str {
    match purpose {
        Purpose::Document => "RETRIEVAL_DOCUMENT",
        Purpose::Query => "RETRIEVAL_QUERY",
    }
}

impl GeminiEmbeddings {
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
    ///
    /// Not a parameter and not a guess: this endpoint is Google's. The OpenAI shape asks
    /// because a dozen vendors and a laptop all speak it, and nothing in a request there can
    /// tell them apart.
    pub fn reach(&self) -> Reach {
        Reach::FirstPartyApi
    }

    /// A different base URL, for a proxy or a recorded endpoint.
    #[must_use]
    pub fn at(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// The request, as this shape writes it.
    ///
    /// One element per input, each naming the model again because this API requires it
    /// inside the batch as well as in the path.
    ///
    /// `taskType` is sent only when the caller said what the text was for. A guessed purpose
    /// is a retrieval quality problem that never surfaces as an error, so the absence has to
    /// travel as an absence.
    fn body(&self, request: &EmbedRequest) -> Value {
        let qualified = format!("models/{}", request.model.as_str());

        let requests: Vec<Value> = request
            .inputs
            .iter()
            .map(|text| {
                let mut one = json!({
                    "model": qualified,
                    "content": { "parts": [{ "text": text }] },
                });
                if let Some(purpose) = request.purpose {
                    one["taskType"] = json!(task_type(purpose));
                }
                if let Some(dimensions) = request.dimensions {
                    one["outputDimensionality"] = json!(dimensions);
                }
                one
            })
            .collect();

        json!({ "requests": requests })
    }

    /// The reply.
    ///
    /// # Errors
    ///
    /// [`Error::Unreadable`] when the envelope is not the expected shape, when a vector holds
    /// something that is not a number, or when the count does not match what was sent.
    fn read(&self, body: &Value, request: &EmbedRequest) -> Result<Embeddings> {
        let rows = body
            .get("embeddings")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unreadable("the reply had no embeddings array".into()))?;

        // Before anything is read. This API carries no index, so position is the only thing
        // tying a vector to its text — which makes a count that does not match unrecoverable
        // rather than merely wrong, and there is nothing to fall back on.
        if rows.len() != request.inputs.len() {
            return Err(Error::Unreadable(format!(
                "{} inputs were sent and {} vectors came back. This API carries no index, so \
                 position is all there is and nothing can be lined up",
                request.inputs.len(),
                rows.len()
            )));
        }

        let mut vectors = Vec::with_capacity(rows.len());
        for (position, row) in rows.iter().enumerate() {
            let numbers = row.get("values").and_then(Value::as_array).ok_or_else(|| {
                Error::Unreadable(format!(
                    "the embedding at position {position} had no values"
                ))
            })?;

            let mut vector = Vec::with_capacity(numbers.len());
            for number in numbers {
                vector.push(number.as_f64().ok_or_else(|| {
                    Error::Unreadable(format!(
                        "the vector at position {position} held something that was not a number"
                    ))
                })? as f32);
            }

            // The model the caller asked for. Unlike the OpenAI shape, this reply does not
            // name what served it, so there is nothing better to record — and recording
            // nothing is not an option, because a vector with no model is one that will be
            // compared against a space it does not belong to.
            vectors.push(Embedding::new(request.model.clone(), vector));
        }

        // Asked for and not honoured. The endpoint answered 200 with vectors of the wrong
        // width, and a caller who sized a database for the number they asked for has no
        // other way to find out.
        if let (Some(asked), Some(got)) = (
            request.dimensions,
            vectors.first().map(Embedding::dimensions),
        ) {
            if usize::try_from(asked).is_ok_and(|asked| asked != got) {
                return Err(Error::Unsupported(format!(
                    "gemini-embeddings: vectors came back with {got} dimensions after {asked} \
                     were asked for, so {} does not resize",
                    request.model.as_str()
                )));
            }
        }

        // No token block in this shape, in either form. Absent, never zeros: a cost nobody
        // measured written as nought is a call that reads as free in whatever adds it up.
        Ok(Embeddings::new(
            vectors,
            request.model.clone(),
            Usage::absent(),
        ))
    }

    /// The key, as a header.
    ///
    /// # Errors
    ///
    /// [`Error::Auth`] when the key is not valid UTF-8.
    fn header(&self) -> Result<(String, String)> {
        let key = self
            .key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;
        Ok(("x-goog-api-key".to_string(), key.to_string()))
    }
}

#[async_trait]
impl Embedder for GeminiEmbeddings {
    fn id(&self) -> &str {
        "gemini-embeddings"
    }

    fn capabilities(&self, model: &ModelId) -> Option<EmbeddingCapabilities> {
        self.known.get(model.as_str()).copied()
    }

    async fn embed(&self, request: EmbedRequest) -> Result<Embeddings> {
        if request.is_empty() {
            return Err(Error::InvalidRequest(
                "there is nothing to embed".to_string(),
            ));
        }

        if let (Some(asked), Some(known)) = (request.dimensions, self.capabilities(&request.model))
        {
            if !known.resizable {
                return Err(Error::Unsupported(format!(
                    "{asked} dimensions were asked for and {} does not resize",
                    request.model.as_str()
                )));
            }
        }

        let (name, value) = self.header()?;
        let http = HttpRequest::new(
            format!(
                "{}/models/{}:batchEmbedContents",
                self.base_url,
                request.model.as_str()
            ),
            serde_json::to_vec(&self.body(&request))
                .map_err(|e| Error::Unreadable(format!("the request would not serialise: {e}")))?,
        )
        .with_header(name, value)
        .with_header("content-type", "application/json");

        let response = self.transport.send(http).await?;
        response.check()?;

        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the reply was not JSON: {e}")))?;

        self.read(&body, &request)
    }

    async fn validate(&self, model: &ModelId) -> Access {
        // Asking for one model rather than the whole list. It costs nothing, it establishes
        // the credential and the entitlement together, and a name that is not there answers
        // 404 rather than an empty list somebody has to interpret.
        let (name, value) = match self.header() {
            Ok(header) => header,
            Err(e) => return Access::denied(format!("{}: {e}", self.id())),
        };
        let http = HttpRequest::get(format!("{}/models/{}", self.base_url, model.as_str()))
            .with_header(name, value);

        let response = match self.transport.send(http).await {
            Ok(response) => response,
            Err(e) => return doubt_or_refusal(self.id(), &e),
        };
        match response.check() {
            Ok(()) => Access::Ready,
            Err(e) => doubt_or_refusal(self.id(), &e),
        }
    }
}

/// A settled refusal or an unsettled moment.
///
/// The same split every other provider here makes. An unknown reads as "ask again later", so
/// a credential a person has to fix must never arrive as one.
fn doubt_or_refusal(id: &str, e: &Error) -> Access {
    match e {
        Error::Auth(reason) | Error::NotFound(reason) => Access::denied(format!("{id}: {reason}")),
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
        sent: Mutex<Vec<(String, Vec<u8>)>>,
    }

    #[async_trait]
    impl HttpTransport for Scripted {
        async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
            self.sent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((request.url.clone(), request.body.clone()));
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

    fn embedder(transport: Arc<Scripted>) -> GeminiEmbeddings {
        with(transport, Secret::new("gemini-api-key", "test-key"))
    }

    fn reply(json: Value) -> Result<HttpResponse> {
        Ok(HttpResponse::new(200, json.to_string().into_bytes()))
    }

    fn vectors(rows: &[&[f32]]) -> Value {
        json!({
            "embeddings": rows.iter().map(|v| json!({ "values": v })).collect::<Vec<_>>()
        })
    }

    #[tokio::test]
    async fn a_purpose_reaches_the_wire_as_a_task_type() {
        // The reason this provider exists. Before it, `Purpose` was an enum nothing wrote
        // anywhere, and a caller setting it got the same vector either way with nothing
        // saying so.
        let transport = scripted(vec![
            reply(vectors(&[&[1.0]])),
            reply(vectors(&[&[1.0]])),
            reply(vectors(&[&[1.0]])),
        ]);
        let embedder = embedder(Arc::clone(&transport));

        embedder
            .embed(EmbedRequest::one("text-embedding-004", "a"))
            .await
            .unwrap();
        embedder
            .embed(EmbedRequest::one("text-embedding-004", "a").for_purpose(Purpose::Document))
            .await
            .unwrap();
        embedder
            .embed(EmbedRequest::one("text-embedding-004", "a").for_purpose(Purpose::Query))
            .await
            .unwrap();

        let sent = transport.sent.lock().unwrap();
        let read = |i: usize| -> Value { serde_json::from_slice(&sent[i].1).unwrap() };

        assert!(
            read(0)["requests"][0].get("taskType").is_none(),
            "nothing said, nothing sent"
        );
        assert_eq!(read(1)["requests"][0]["taskType"], "RETRIEVAL_DOCUMENT");
        assert_eq!(read(2)["requests"][0]["taskType"], "RETRIEVAL_QUERY");
    }

    #[tokio::test]
    async fn the_model_is_written_in_the_path_and_in_every_element() {
        // This API requires both and rejects a request carrying only one. Both come from the
        // same `ModelId`, so they cannot disagree.
        let transport = scripted(vec![reply(vectors(&[&[1.0], &[2.0]]))]);

        embedder(Arc::clone(&transport))
            .embed(EmbedRequest::new(
                "text-embedding-004".into(),
                vec!["a".into(), "b".into()],
            ))
            .await
            .unwrap();

        let sent = transport.sent.lock().unwrap();
        assert!(
            sent[0]
                .0
                .ends_with("/models/text-embedding-004:batchEmbedContents"),
            "{}",
            sent[0].0
        );
        let body: Value = serde_json::from_slice(&sent[0].1).unwrap();
        assert_eq!(body["requests"][0]["model"], "models/text-embedding-004");
        assert_eq!(body["requests"][1]["model"], "models/text-embedding-004");
        assert_eq!(body["requests"][1]["content"]["parts"][0]["text"], "b");
    }

    #[tokio::test]
    async fn a_count_that_does_not_match_is_unrecoverable_here() {
        // With no index in the reply, position is the only thing tying a vector to its text.
        // A short batch cannot be repaired and must not be returned.
        let transport = scripted(vec![reply(vectors(&[&[1.0]]))]);

        match embedder(transport)
            .embed(EmbedRequest::new(
                "text-embedding-004".into(),
                vec!["a".into(), "b".into()],
            ))
            .await
        {
            Err(Error::Unreadable(message)) => {
                assert!(message.contains("no index"), "{message}");
            }
            other => panic!("a short batch has to fail: {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_call_reports_usage_as_absent_rather_than_nought() {
        // This shape has no token block at all. Zeros would turn a cost nobody measured into
        // a call that was free in whatever adds it up.
        let transport = scripted(vec![reply(vectors(&[&[1.0, 2.0]]))]);

        let answer = embedder(transport)
            .embed(EmbedRequest::one("text-embedding-004", "a"))
            .await
            .unwrap();

        assert_eq!(answer.usage, Usage::absent());
        assert_eq!(answer.usage.coverage(), crate::UsageCoverage::Absent);
    }

    #[tokio::test]
    async fn a_ledger_holding_one_of_these_says_at_least() {
        // The absent rule, one step along, which is the whole reason it is a rule.
        let transport = scripted(vec![reply(vectors(&[&[1.0]]))]);
        let answer = embedder(transport)
            .embed(EmbedRequest::one("text-embedding-004", "a"))
            .await
            .unwrap();

        let mut ledger = crate::Ledger::new();
        ledger.record_unpriced(answer.model.clone(), answer.usage);

        let total = ledger
            .total()
            .unwrap_or_else(|| unreachable!("one currency"));
        assert!(!total.is_exact(), "{total}");
        assert_eq!(ledger.calls(), 1, "the call still happened");
    }

    #[tokio::test]
    async fn dimensions_are_asked_for_and_checked_on_the_way_back() {
        let transport = scripted(vec![reply(vectors(&[&[1.0, 2.0, 3.0]]))]);

        match embedder(Arc::clone(&transport))
            .embed(EmbedRequest::one("text-embedding-004", "a").with_dimensions(2))
            .await
        {
            Err(Error::Unsupported(message)) => {
                assert!(message.contains("does not resize"), "{message}");
            }
            other => panic!("silently wrong width is the failure: {other:?}"),
        }

        let sent = transport.sent.lock().unwrap();
        let body: Value = serde_json::from_slice(&sent[0].1).unwrap();
        assert_eq!(body["requests"][0]["outputDimensionality"], 2);
    }

    #[tokio::test]
    async fn every_vector_carries_a_model_even_though_the_reply_names_none() {
        let transport = scripted(vec![reply(vectors(&[&[1.0], &[2.0]]))]);
        let answer = embedder(transport)
            .embed(EmbedRequest::new(
                "text-embedding-004".into(),
                vec!["a".into(), "b".into()],
            ))
            .await
            .unwrap();

        for vector in &answer.vectors {
            assert_eq!(vector.model.as_str(), "text-embedding-004");
        }
        assert_eq!(answer.dimensions(), Some(1));
    }

    #[tokio::test]
    async fn a_rejected_credential_is_denied_rather_than_unknown() {
        let transport = scripted(vec![Ok(HttpResponse::new(401, b"no".to_vec()))]);
        let answer = embedder(transport)
            .validate(&"text-embedding-004".into())
            .await;
        assert!(answer.is_denied(), "{answer}");
    }

    #[tokio::test]
    async fn a_model_that_is_not_there_is_denied_and_a_bad_moment_is_not() {
        let missing = scripted(vec![Ok(HttpResponse::new(404, b"nope".to_vec()))]);
        assert!(embedder(missing)
            .validate(&"not-a-model".into())
            .await
            .is_denied());

        let flaky = scripted(vec![Err(Error::Transient("the network went away".into()))]);
        assert!(embedder(flaky)
            .validate(&"text-embedding-004".into())
            .await
            .is_unknown());
    }

    #[tokio::test]
    async fn an_empty_batch_never_reaches_the_wire() {
        let transport = scripted(vec![]);
        let answer = embedder(Arc::clone(&transport))
            .embed(EmbedRequest::new("text-embedding-004".into(), vec![]))
            .await;

        assert!(
            matches!(answer, Err(Error::InvalidRequest(_))),
            "{answer:?}"
        );
        assert!(transport.sent.lock().unwrap().is_empty());
    }
}
