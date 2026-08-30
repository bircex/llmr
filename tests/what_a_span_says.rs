//! What a span carries, and the two things it must never carry.
//!
//! Only runs with the `tracing` feature on. The subscriber here is written by hand rather
//! than pulled from `tracing-subscriber`: it has to record every field value as text so the
//! test can assert about all of them, and that is less code than configuring a general one.

#![cfg(all(feature = "tracing", feature = "anthropic"))]

use llmr::chat::message::{Message, Role, StopReason};
use llmr::chat::request::ChatRequest;
use llmr::chat::response::ChatResponse;
use llmr::model::{ModelCapabilities, ModelId, Reach};
use llmr::provider::Provider;
use llmr::router::{Requirements, Route, Router};
use llmr::Usage;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};

/// The secret the test watches for. If this string reaches a span, something recorded a
/// prompt, and every program that upgraded would be logging its users' text.
const PROMPT: &str = "the-private-thing-the-user-typed";

/// And this one, for the credential.
const KEY: &str = "sk-do-not-log-me";

/// Records every field of every span and event, as text.
#[derive(Default)]
struct Recording {
    fields: Mutex<Vec<(String, String)>>,
    next: AtomicU64,
}

impl Recording {
    fn saw(&self, name: &str, value: String) {
        if let Ok(mut fields) = self.fields.lock() {
            fields.push((name.to_string(), value));
        }
    }

    fn all(&self) -> Vec<(String, String)> {
        self.fields.lock().map(|f| f.clone()).unwrap_or_default()
    }

    fn named(&self, name: &str) -> Option<String> {
        self.all()
            .into_iter()
            .find(|(n, v)| n == name && !v.is_empty())
            .map(|(_, v)| v)
    }
}

struct Sink<'a>(&'a Recording);

impl Visit for Sink<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.saw(field.name(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.saw(field.name(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.saw(field.name(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.saw(field.name(), value.to_string());
    }
}

/// A subscriber sharing one recording, so the test can read what it saw afterwards.
#[derive(Clone)]
struct Watching(Arc<Recording>);

impl tracing::Subscriber for Watching {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::Id {
        span.record(&mut Sink(&self.0));
        tracing::Id::from_u64(self.0.next.fetch_add(1, Ordering::SeqCst) + 1)
    }
    fn record(&self, _: &tracing::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut Sink(&self.0));
    }
    fn record_follows_from(&self, _: &tracing::Id, _: &tracing::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        event.record(&mut Sink(&self.0));
    }
    fn enter(&self, _: &tracing::Id) {}
    fn exit(&self, _: &tracing::Id) {}
}

/// A provider that answers, and whose id is not a secret.
struct Stub {
    fails_first: bool,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for Stub {
    fn id(&self) -> &str {
        "stub"
    }
    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        (model.as_str() == "m").then(|| ModelCapabilities::none(Reach::SelfHosted))
    }
    async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if self.fails_first && first {
            return Err(llmr::Error::Transient("503".into()));
        }
        Ok(ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![llmr::ContentBlock::Text("an answer".into())],
            },
            StopReason::EndTurn,
            Usage::absent().with_input(10).with_output(2),
            request.model,
        ))
    }
}

fn stub(fails_first: bool) -> Arc<Stub> {
    Arc::new(Stub {
        fails_first,
        calls: std::sync::atomic::AtomicUsize::new(0),
    })
}

/// Runs one routed call under a recording subscriber and hands back everything it saw.
async fn spans_from(routes: Vec<Route>) -> Arc<Recording> {
    let recording = Arc::new(Recording::default());
    let guard = tracing::subscriber::set_default(Watching(recording.clone()));

    // The prompt and the key both go in. Neither may come out.
    let request = ChatRequest::new("m", vec![Message::user(PROMPT)]).with_system(PROMPT);
    let _secret = llmr::Secret::new("api-key", KEY);

    let _ = Router::new(routes)
        .chat(request, Requirements::default())
        .await;

    drop(guard);
    recording
}

#[tokio::test]
async fn a_span_carries_the_facts_a_report_needs() {
    let seen = spans_from(vec![Route::new(stub(false), "m")]).await;

    assert_eq!(seen.named("model").as_deref(), Some("m"));
    assert_eq!(seen.named("route").as_deref(), Some("stub/m"));
    assert_eq!(seen.named("usage_coverage").as_deref(), Some("partial"));
    assert_eq!(seen.named("attempts").as_deref(), Some("1"));
}

#[tokio::test]
async fn a_span_never_carries_the_prompt_or_the_key() {
    // The rule the whole feature is built around. `Secret` does not print, and there is no
    // argument on any function in `observe` that could hold a message — this is the test
    // that says so out loud, so removing that property fails here rather than in somebody's
    // log aggregator.
    let seen = spans_from(vec![Route::new(stub(true), "m")]).await;

    let recorded: HashMap<_, _> = seen.all().into_iter().collect();
    assert!(!recorded.is_empty(), "nothing was recorded at all");

    for (name, value) in seen.all() {
        assert!(
            !value.contains(PROMPT),
            "field {name:?} carried the prompt: {value:?}"
        );
        assert!(
            !value.contains(KEY),
            "field {name:?} carried the credential: {value:?}"
        );
    }
}

#[tokio::test]
async fn a_reply_that_was_not_the_first_route_says_so() {
    // The whole reason this feature is worth having. Nothing failed, and something is going
    // wrong, and today that fact lives in a struct field nobody reads.
    let seen = spans_from(vec![
        Route::new(stub(true), "unknown-model"),
        Route::new(stub(false), "m"),
    ])
    .await;

    assert_eq!(seen.named("fell_through").as_deref(), Some("1"));
    let messages: Vec<_> = seen
        .all()
        .into_iter()
        .filter(|(n, _)| n == "message")
        .map(|(_, v)| v)
        .collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("not by the first route")),
        "{messages:?}"
    );
}
