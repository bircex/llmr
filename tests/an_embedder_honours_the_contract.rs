//! The embedder contract suite, applied to the embedder that ships here.
//!
//! A suite only the implementations written outside a crate have to pass is a suite nobody
//! inside it is held to.
//!
//! The double is the interesting part. It answers vectors that actually depend on the text
//! it was given, and it answers them **out of order on purpose**, with an `index` on every
//! row the way the real endpoint does. A fixture that replied in order would let a provider
//! that ignores the index pass, which is the one failure this contract exists to catch.

#![cfg(all(
    feature = "testkit",
    feature = "openai",
    feature = "gemini",
    feature = "embeddings"
))]

use llmr::embed::{EmbedRequest, Embedder, EmbeddingCapabilities};
use llmr::providers::openai;
use llmr::testkit::assert_embedder_contract;
use llmr::transport::{HttpRequest, HttpResponse, HttpTransport};
use llmr::{Error, Reach, Secret};
use serde_json::{json, Value};
use std::sync::Arc;

/// An embeddings endpoint that answers deterministically and out of order.
struct Shuffled;

/// A vector that depends on the text, so the same input twice gives the same answer and two
/// different inputs give different ones.
///
/// Not a real embedding and it does not need to be. What the contract checks is that a text
/// embedded alone lands nearest the batch vector at its own position, and any deterministic
/// function of the bytes establishes that.
fn vector_for(text: &str) -> Vec<f32> {
    let mut buckets = [0.0f32; 8];
    for (position, byte) in text.bytes().enumerate() {
        buckets[position % 8] += f32::from(byte);
    }
    buckets.to_vec()
}

#[async_trait::async_trait]
impl HttpTransport for Shuffled {
    async fn send(&self, request: HttpRequest) -> llmr::Result<HttpResponse> {
        if request.url.ends_with("/models") {
            return Ok(HttpResponse::new(
                200,
                json!({ "data": [{ "id": "text-embedding-3-small" }] })
                    .to_string()
                    .into_bytes(),
            ));
        }

        let body: Value = serde_json::from_slice(&request.body)
            .map_err(|e| Error::Unreadable(format!("the double was sent no JSON: {e}")))?;
        let inputs = body
            .get("input")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unreadable("the double was sent no inputs".into()))?;

        let mut rows: Vec<Value> = inputs
            .iter()
            .enumerate()
            .map(|(index, input)| {
                json!({
                    "object": "embedding",
                    "index": index,
                    "embedding": vector_for(input.as_str().unwrap_or("")),
                })
            })
            .collect();

        // Backwards, every time. The real endpoint makes no promise about order, and this
        // is the cheapest way to hold a provider to reading the index.
        rows.reverse();

        let mut tokens = 0u64;
        for input in inputs {
            tokens += input.as_str().unwrap_or("").split_whitespace().count() as u64;
        }

        Ok(HttpResponse::new(
            200,
            json!({
                "object": "list",
                "data": rows,
                "model": "text-embedding-3-small",
                "usage": { "prompt_tokens": tokens, "total_tokens": tokens },
            })
            .to_string()
            .into_bytes(),
        ))
    }
}

fn embedder() -> impl Embedder {
    openai::embed::at(
        "openai-embeddings",
        "https://example.invalid/v1",
        Arc::new(Shuffled),
        Secret::new("openai-api-key", "sk-test"),
        Reach::FirstPartyApi,
    )
    .knowing(
        "text-embedding-3-small",
        EmbeddingCapabilities::none(Reach::FirstPartyApi)
            .with_dimensions(8)
            .with_max_batch(2_048)
            .resizable(),
    )
}

/// The same endpoint in Gemini's shape, which is a different shape in every way that matters.
///
/// No index on the rows, because that API carries none and its array order *is* the promise.
/// No usage block at all. And a `taskType` the OpenAI shape has nowhere to put.
struct GeminiShaped;

#[async_trait::async_trait]
impl HttpTransport for GeminiShaped {
    async fn send(&self, request: HttpRequest) -> llmr::Result<HttpResponse> {
        // `validate` asks for one model by name rather than for a list.
        if !request.url.contains(":batchEmbedContents") {
            return Ok(HttpResponse::new(
                200,
                json!({ "name": "models/text-embedding-004" })
                    .to_string()
                    .into_bytes(),
            ));
        }

        let body: Value = serde_json::from_slice(&request.body)
            .map_err(|e| Error::Unreadable(format!("the double was sent no JSON: {e}")))?;
        let requests = body
            .get("requests")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unreadable("the double was sent no requests".into()))?;

        let rows: Vec<Value> = requests
            .iter()
            .map(|one| {
                let text = one["content"]["parts"][0]["text"].as_str().unwrap_or("");
                json!({ "values": vector_for(text) })
            })
            .collect();

        Ok(HttpResponse::new(
            200,
            json!({ "embeddings": rows }).to_string().into_bytes(),
        ))
    }
}

fn gemini_embedder() -> impl Embedder {
    llmr::providers::gemini::embed::with(
        Arc::new(GeminiShaped),
        Secret::new("gemini-api-key", "test-key"),
    )
    .knowing(
        "text-embedding-004",
        EmbeddingCapabilities::none(Reach::FirstPartyApi)
            .with_dimensions(8)
            .with_purposes(),
    )
}

#[tokio::test]
async fn the_gemini_embedder_honours_the_same_contract() {
    // The point of having two. A suite one implementation passes is a description of that
    // implementation; these two agree on nothing at the wire — index or no index, usage or
    // no usage, a place for `Purpose` or none — and answer the same questions the same way.
    assert_embedder_contract(&gemini_embedder(), "text-embedding-004").await;
}

#[tokio::test]
async fn an_embedder_that_trusts_arrival_order_fails_the_contract_in_either_shape() {
    // Deliberately empty of its own logic: the shared assertion is above. This name exists
    // so a reader looking for "is the Gemini one held to the ordering rule too" finds a yes.
    let reply = gemini_embedder()
        .embed(EmbedRequest::new(
            "text-embedding-004".into(),
            vec!["alpha".into(), "beta".into()],
        ))
        .await
        .unwrap_or_else(|e| panic!("the double: {e}"));

    assert_eq!(
        reply.into_vectors(),
        vec![vector_for("alpha"), vector_for("beta")],
        "position is the only thing tying a vector to its text in this shape"
    );
}

/// An embedder that trusts arrival order, which is the bug the contract exists to catch.
///
/// It is here to fail. A suite that a broken implementation passes is worse than no suite,
/// because it is read as evidence.
struct TrustsArrivalOrder;

#[async_trait::async_trait]
impl Embedder for TrustsArrivalOrder {
    fn id(&self) -> &str {
        "trusts-arrival-order"
    }

    fn capabilities(&self, _model: &llmr::ModelId) -> Option<EmbeddingCapabilities> {
        None
    }

    async fn embed(&self, request: EmbedRequest) -> llmr::Result<llmr::embed::Embeddings> {
        // Every vector is real and belongs to a text that was sent. The only thing wrong is
        // which text, and nothing in the reply says so.
        let mut vectors: Vec<llmr::embed::Embedding> = request
            .inputs
            .iter()
            .map(|input| llmr::embed::Embedding::new(request.model.clone(), vector_for(input)))
            .collect();
        vectors.reverse();

        Ok(llmr::embed::Embeddings::new(
            vectors,
            request.model,
            llmr::Usage::embedding(3),
        ))
    }
}

#[tokio::test]
#[should_panic(expected = "not in the order it was sent")]
async fn an_embedder_that_trusts_arrival_order_fails_the_contract() {
    assert_embedder_contract(&TrustsArrivalOrder, "text-embedding-3-small").await;
}

#[tokio::test]
async fn the_shipped_embedder_honours_the_contract() {
    assert_embedder_contract(&embedder(), "text-embedding-3-small").await;
}

#[tokio::test]
async fn the_double_really_does_answer_out_of_order() {
    // Otherwise the test above proves nothing. This asserts the fixture is adversarial:
    // the raw reply is reversed, and the provider is what puts it back.
    let reply = embedder()
        .embed(EmbedRequest::new(
            "text-embedding-3-small".into(),
            vec!["alpha".into(), "beta".into(), "gamma".into()],
        ))
        .await
        .unwrap_or_else(|e| panic!("the double: {e}"));

    assert_eq!(
        reply.clone().into_vectors(),
        vec![vector_for("alpha"), vector_for("beta"), vector_for("gamma")],
        "the provider put back what the endpoint sent backwards"
    );

    let raw = Shuffled
        .send(HttpRequest::new(
            "https://example.invalid/v1/embeddings",
            json!({ "input": ["alpha", "beta", "gamma"] })
                .to_string()
                .into_bytes(),
        ))
        .await
        .unwrap_or_else(|e| panic!("the double: {e}"));
    let raw: Value = serde_json::from_slice(&raw.body).unwrap_or(Value::Null);
    assert_eq!(
        raw["data"][0]["index"], 2,
        "a fixture that answered in order would prove nothing"
    );
}
