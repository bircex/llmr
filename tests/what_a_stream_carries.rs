//! What each protocol makes of a server sent event stream.
//!
//! Recorded frames rather than a server, chunked at deliberately awkward boundaries, because
//! the failures worth catching here are all about where a chunk happens to end.
//!
//! The property every test in this file is circling: **a streamed call and a whole one must
//! produce the same reply**. Two ways to ask the same question that disagree are worse than
//! one way, and the disagreement is invisible until somebody compares two cost reports.

#![cfg(all(feature = "anthropic", feature = "openai"))]

use llmr::chat::stream::Transcript;
use llmr::providers::anthropic;
use llmr::providers::openai;
use llmr::transport::{ByteStream, HttpRequest, HttpResponse, HttpTransport};
use llmr::{
    ChatRequest, ContentBlock, Message, Provider, Reach, Registry, Secret, StopReason,
    UsageCoverage,
};
use std::sync::{Arc, Mutex};

/// Hands back a scripted stream, split exactly where the test says.
struct Streamed {
    chunks: Vec<Vec<u8>>,
    fail_after: Option<usize>,
    sent: Mutex<Vec<HttpRequest>>,
}

impl Streamed {
    /// A body split into chunks of `size` bytes, so frame boundaries and chunk boundaries
    /// do not line up. A parser that assumes one chunk is one frame passes only when they
    /// happen to agree, which on a real network is never.
    fn chopped(body: &str, size: usize) -> Arc<Self> {
        Arc::new(Self {
            chunks: body.as_bytes().chunks(size).map(<[u8]>::to_vec).collect(),
            fail_after: None,
            sent: Mutex::new(Vec::new()),
        })
    }

    /// The same, but the connection drops after `n` chunks.
    fn cut_off(body: &str, size: usize, n: usize) -> Arc<Self> {
        let mut it = Self::chopped(body, size);
        Arc::get_mut(&mut it).expect("sole owner").fail_after = Some(n);
        it
    }

    fn body(&self) -> serde_json::Value {
        let sent = self.sent.lock().expect("not poisoned");
        let first = sent.first().expect("something was sent");
        serde_json::from_slice(&first.body).unwrap_or(serde_json::Value::Null)
    }
}

#[async_trait::async_trait]
impl HttpTransport for Streamed {
    async fn send(&self, request: HttpRequest) -> llmr::Result<HttpResponse> {
        self.sent.lock().expect("not poisoned").push(request);
        Ok(HttpResponse::new(200, Vec::new()))
    }

    async fn send_streaming(&self, request: HttpRequest) -> llmr::Result<ByteStream> {
        self.sent.lock().expect("not poisoned").push(request);
        let chunks = self.chunks.clone();
        let fail_after = self.fail_after;
        Ok(Box::pin(Scripted {
            chunks: chunks.into_iter(),
            served: 0,
            fail_after,
        }))
    }
}

struct Scripted {
    chunks: std::vec::IntoIter<Vec<u8>>,
    served: usize,
    fail_after: Option<usize>,
}

impl futures_core::Stream for Scripted {
    type Item = llmr::Result<Vec<u8>>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.fail_after == Some(self.served) {
            return std::task::Poll::Ready(Some(Err(llmr::Error::Transient(
                "the connection went away".into(),
            ))));
        }
        self.served += 1;
        std::task::Poll::Ready(self.chunks.next().map(Ok))
    }
}

fn request() -> ChatRequest {
    ChatRequest::new("m", vec![Message::user("hi")])
}

async fn collect(provider: &impl Provider) -> (llmr::ChatResponse, llmr::Result<()>) {
    let mut transcript = Transcript::new("m");
    let outcome = match provider.stream(request()).await {
        Ok(stream) => transcript.drain(stream).await,
        Err(e) => Err(e),
    };
    (transcript.finish(), outcome)
}

// ---- Anthropic -------------------------------------------------------------------------

/// A reply with reasoning, a signature, text and a tool call in it.
const ANTHROPIC_STREAM: &str = "\
event: message_start
data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":900,\"cache_creation_input_tokens\":0}}}

event: content_block_start
data: {\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

event: content_block_delta
data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"two plus \"}}

event: content_block_delta
data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"two\"}}

event: content_block_delta
data: {\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-abc\"}}

event: content_block_stop
data: {\"index\":0}

event: content_block_start
data: {\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

event: content_block_delta
data: {\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Fo\"}}

event: content_block_delta
data: {\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"ur.\"}}

event: content_block_stop
data: {\"index\":1}

event: message_delta
data: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}

event: message_stop
data: {\"type\":\"message_stop\"}

";

fn anthropic_provider(transport: Arc<Streamed>) -> anthropic::api::Anthropic {
    anthropic::api::with(
        transport,
        Secret::new("key", "sk-test"),
        Arc::new(Registry::empty("anthropic", Reach::FirstPartyApi)),
    )
}

#[tokio::test]
async fn anthropic_asks_for_a_stream_rather_than_a_whole_reply() {
    let transport = Streamed::chopped(ANTHROPIC_STREAM, 64);
    let _ = anthropic_provider(Arc::clone(&transport))
        .stream(request())
        .await;
    assert_eq!(transport.body()["stream"], true);
}

#[tokio::test]
async fn anthropic_assembles_reasoning_text_and_the_signature() {
    // The signature is the one that matters. Assembled without it, this conversation is
    // rejected on the turn after this one, a long way from the mistake.
    let transport = Streamed::chopped(ANTHROPIC_STREAM, 17);
    let (reply, outcome) = collect(&anthropic_provider(transport)).await;
    assert!(outcome.is_ok(), "{outcome:?}");

    assert_eq!(
        reply.message.content[0],
        ContentBlock::Thinking {
            text: "two plus two".into(),
            signature: Some("sig-abc".into()),
        }
    );
    assert_eq!(reply.text(), "Four.");
    assert_eq!(reply.stop_reason, StopReason::EndTurn);
    assert_eq!(reply.model.as_str(), "claude-sonnet-5");
}

#[tokio::test]
async fn anthropic_merges_the_usage_from_both_ends_of_the_stream() {
    // The prompt count arrives in the first frame and the output count in the last. Keeping
    // only one of them reports a call that cost half what it did.
    let transport = Streamed::chopped(ANTHROPIC_STREAM, 128);
    let (reply, _) = collect(&anthropic_provider(transport)).await;
    assert_eq!(reply.usage.input_tokens, Some(12));
    assert_eq!(reply.usage.cache_read_tokens, Some(900));
    assert_eq!(reply.usage.output_tokens, Some(4));
}

#[tokio::test]
async fn a_chunk_boundary_inside_a_frame_changes_nothing() {
    // Chunks do not respect frames, and a parser that assumes they do works against a fast
    // local server and fails against a real one.
    let mut texts = Vec::new();
    for size in [1, 3, 7, 64, 4096] {
        let transport = Streamed::chopped(ANTHROPIC_STREAM, size);
        let (reply, outcome) = collect(&anthropic_provider(transport)).await;
        assert!(outcome.is_ok(), "chunked at {size}: {outcome:?}");
        texts.push((size, reply.text(), reply.usage));
    }
    let (_, first_text, first_usage) = texts[0].clone();
    for (size, text, usage) in texts {
        assert_eq!(text, first_text, "chunked at {size} read differently");
        assert_eq!(usage, first_usage, "chunked at {size} counted differently");
    }
}

#[tokio::test]
async fn a_stream_that_dies_partway_keeps_what_arrived_and_says_it_did_not_finish() {
    // The three facts a caller needs, none inferred from the others: what arrived, that the
    // turn did not finish, and why.
    let transport = Streamed::cut_off(ANTHROPIC_STREAM, 200, 2);
    let (reply, outcome) = collect(&anthropic_provider(transport)).await;

    let why = outcome.expect_err("the connection was cut");
    assert!(why.to_string().contains("went away"), "{why}");
    assert_eq!(reply.stop_reason, StopReason::Interrupted);
    assert!(
        !reply.is_complete(),
        "a cut off reply must not read as done"
    );
    assert!(
        !reply.message.content.is_empty(),
        "what arrived before the cut is still ours"
    );
    // Partial, not Absent and not Exact. The prompt count really did arrive in the opening
    // frame, so claiming nothing was reported would be as wrong as claiming everything was.
    assert_eq!(reply.usage.coverage(), UsageCoverage::Partial);
    assert_eq!(
        reply.usage.input_tokens,
        Some(12),
        "this much really arrived"
    );
    assert_eq!(
        reply.usage.output_tokens, None,
        "the output count never arrived, and None is the only honest answer. A zero here \
         would price everything the model produced at nothing"
    );
}

// ---- The OpenAI shape ------------------------------------------------------------------

const OPENAI_STREAM: &str = "\
data: {\"model\":\"gpt-5.3\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}

data: {\"model\":\"gpt-5.3\",\"choices\":[{\"delta\":{\"content\":\"Fo\"}}]}

data: {\"model\":\"gpt-5.3\",\"choices\":[{\"delta\":{\"content\":\"ur.\"}}]}

data: {\"model\":\"gpt-5.3\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}

data: {\"model\":\"gpt-5.3\",\"choices\":[],\"usage\":{\"prompt_tokens\":912,\"completion_tokens\":4,\"prompt_tokens_details\":{\"cached_tokens\":900}}}

data: [DONE]

";

fn openai_provider(transport: Arc<Streamed>) -> openai::api::OpenAiCompatible {
    openai::api::at(
        "openai",
        "https://example.test/v1",
        transport,
        Secret::new("key", "sk-test"),
        Reach::FirstPartyApi,
        Arc::new(Registry::empty("openai", Reach::FirstPartyApi)),
    )
}

#[tokio::test]
async fn the_openai_shape_asks_for_usage_it_would_otherwise_not_get() {
    // Without `include_usage` this shape streams no token counts at all, and a call that
    // reports nothing becomes a free one in every report that adds it up.
    let transport = Streamed::chopped(OPENAI_STREAM, 64);
    let _ = openai_provider(Arc::clone(&transport))
        .stream(request())
        .await;
    let body = transport.body();
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn the_openai_shape_subtracts_the_cached_part_in_a_stream_too() {
    // The adjustment the whole provider is documented for. If it happened for a whole call
    // and not for a streamed one, the same conversation would report two different input
    // counts depending on how it was read.
    let transport = Streamed::chopped(OPENAI_STREAM, 40);
    let (reply, outcome) = collect(&openai_provider(transport)).await;
    assert!(outcome.is_ok(), "{outcome:?}");

    assert_eq!(reply.text(), "Four.");
    assert_eq!(reply.stop_reason, StopReason::EndTurn);
    assert_eq!(
        reply.usage.input_tokens,
        Some(12),
        "912 total less 900 cached"
    );
    assert_eq!(reply.usage.cache_read_tokens, Some(900));
    assert_eq!(reply.usage.output_tokens, Some(4));
    assert_eq!(reply.model.as_str(), "gpt-5.3");
}

#[tokio::test]
async fn the_done_sentinel_is_not_read_as_a_broken_frame() {
    // It is the one frame in this shape that is not JSON. Treating it as unreadable would
    // turn every successful stream into a failure at the last moment.
    let transport = Streamed::chopped(OPENAI_STREAM, 9);
    let (_, outcome) = collect(&openai_provider(transport)).await;
    assert!(outcome.is_ok(), "{outcome:?}");
}

#[tokio::test]
async fn a_frame_that_is_neither_json_nor_the_sentinel_is_an_error() {
    // Silently dropping it would produce a reply missing a piece, with nothing saying which.
    let transport = Streamed::chopped("data: {not json at all\n\n", 64);
    let (_, outcome) = collect(&openai_provider(transport)).await;
    assert!(
        matches!(outcome, Err(llmr::Error::Unreadable(_))),
        "{outcome:?}"
    );
}

// ---- Both ------------------------------------------------------------------------------

#[cfg(feature = "cli")]
#[tokio::test]
async fn a_reach_that_cannot_stream_says_so_rather_than_failing_at_the_call() {
    // The default path. A command line tool cannot stream, and the guarantee is that asking
    // it to still produces the same reply rather than a refusal.
    let cli = llmr::providers::anthropic::cli::provider(std::time::Duration::from_secs(1))
        .serving(["claude-sonnet-5"]);
    let caps = cli
        .capabilities(&llmr::ModelId::from("claude-sonnet-5"))
        .expect("a model it serves");
    assert!(
        !caps.streaming,
        "a tool that prints one JSON document at the end cannot stream, and must say so \
         rather than failing at the call"
    );
}
