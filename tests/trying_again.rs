//! What a retry policy does, and the four things it must never do.
//!
//! No clock. The policy waits through a [`Delay`] the test supplies, which records what it
//! was asked to wait and returns at once — so what these check is the decision rather than
//! the sleeping, and the suite stays fast enough that nobody is tempted to delete it.

use llmr::chat::message::{Message, Role, StopReason};
use llmr::chat::request::ChatRequest;
use llmr::chat::response::ChatResponse;
use llmr::model::{ModelCapabilities, ModelId, Reach};
use llmr::provider::Provider;
use llmr::retry::{Delay, Retry};
use llmr::router::{Requirements, Route, Router};
use llmr::{Error, Usage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Records what it was asked to wait, and never actually waits.
#[derive(Default)]
struct Recording(Mutex<Vec<Duration>>);

impl Recording {
    fn waits(&self) -> Vec<Duration> {
        self.0.lock().map(|w| w.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Delay for Recording {
    async fn sleep(&self, how_long: Duration) {
        if let Ok(mut seen) = self.0.lock() {
            seen.push(how_long);
        }
    }
}

/// Fails in a scripted way for the first `failures` calls, then answers.
struct Flaky {
    id: String,
    model: String,
    how: Box<dyn Fn() -> Error + Send + Sync>,
    failures: usize,
    calls: AtomicUsize,
}

impl Flaky {
    fn new(
        id: &str,
        failures: usize,
        how: impl Fn() -> Error + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            model: "m".into(),
            how: Box::new(how),
            failures,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for Flaky {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        (model.as_str() == self.model).then(|| ModelCapabilities::none(Reach::FirstPartyApi))
    }

    async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
        let seen = self.calls.fetch_add(1, Ordering::SeqCst);
        if seen < self.failures {
            return Err((self.how)());
        }
        Ok(ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![llmr::ContentBlock::Text(format!("from {}", self.id))],
            },
            StopReason::EndTurn,
            Usage::absent().with_output(1),
            request.model,
        ))
    }
}

fn request() -> ChatRequest {
    ChatRequest::new("m", vec![Message::user("hi")])
}

fn policy(attempts: u32, delay: Arc<Recording>) -> Retry {
    Retry::with_delay(attempts, delay).with_base(Duration::from_millis(100))
}

#[tokio::test]
async fn a_rate_limit_waits_exactly_as_long_as_the_provider_asked() {
    // Not a computed backoff. The provider is saying when the limit clears, and a local
    // timer that fires sooner turns one rate limit into two.
    let clock = Arc::new(Recording::default());
    let provider = Flaky::new("flaky", 1, || Error::RateLimited {
        retry_after: Some(Duration::from_secs(42)),
    });

    let router = Router::new(vec![Route::new(provider.clone(), "m")])
        .retrying(policy(3, clock.clone()).with_ceiling(Duration::from_millis(1)));

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .expect("the second attempt answers");

    assert_eq!(routed.attempts, 2);
    assert_eq!(
        clock.waits(),
        vec![Duration::from_secs(42)],
        "the ceiling must not shorten a wait the provider named"
    );
}

#[tokio::test]
async fn the_caller_can_see_how_many_attempts_a_reply_cost() {
    // A reply that took three attempts cost three, and nothing else about a successful
    // reply says so.
    let clock = Arc::new(Recording::default());
    let provider = Flaky::new("flaky", 2, || Error::Transient("503".into()));

    let routed = Router::new(vec![Route::new(provider.clone(), "m")])
        .retrying(policy(4, clock.clone()))
        .chat(request(), Requirements::default())
        .await
        .expect("the third attempt answers");

    assert_eq!(routed.attempts, 3);
    assert_eq!(provider.calls(), 3);
    assert_eq!(clock.waits().len(), 2, "two failures, two waits");

    // And each retry left a line saying it happened, rather than a call paid for twice in
    // silence.
    assert_eq!(routed.fell_through.len(), 2);
    assert!(
        routed.fell_through[0].why.contains("attempt 1 of 4"),
        "{:?}",
        routed.fell_through[0]
    );
}

#[tokio::test]
async fn a_refusal_is_never_retried() {
    // It is an answer. Asking the same model again is the same shopping-around a refusal
    // stops between routes, done against one provider instead of several.
    let clock = Arc::new(Recording::default());
    let provider = Flaky::new("flaky", 99, || Error::Refused {
        category: Some("policy".into()),
    });

    let outcome = Router::new(vec![Route::new(provider.clone(), "m")])
        .retrying(policy(5, clock.clone()))
        .chat(request(), Requirements::default())
        .await;

    assert!(matches!(outcome, Err(Error::Refused { .. })), "{outcome:?}");
    assert_eq!(provider.calls(), 1, "asked once and only once");
    assert!(clock.waits().is_empty(), "nothing to wait for");
}

#[tokio::test]
async fn a_rejected_credential_is_never_retried() {
    // It returns the same answer the second time, and the second attempt is a request the
    // provider counts against you for nothing.
    let clock = Arc::new(Recording::default());
    let provider = Flaky::new("flaky", 99, || Error::Auth("invalid key".into()));

    let outcome = Router::new(vec![Route::new(provider.clone(), "m")])
        .retrying(policy(5, clock.clone()))
        .chat(request(), Requirements::default())
        .await;

    assert!(matches!(outcome, Err(Error::Auth(_))), "{outcome:?}");
    assert_eq!(provider.calls(), 1);
    assert!(clock.waits().is_empty());
}

#[tokio::test]
async fn a_timeout_is_not_repeated_unless_the_caller_said_so() {
    // The deadline passed; the work may not have. A second attempt can buy two answers to
    // one question, and only the caller knows whether that is acceptable.
    let clock = Arc::new(Recording::default());
    let timing_out = || Error::Timeout {
        elapsed: Duration::from_secs(60),
    };

    let cautious = Flaky::new("cautious", 1, timing_out);
    let _ = Router::new(vec![Route::new(cautious.clone(), "m")])
        .retrying(policy(4, clock.clone()))
        .chat(request(), Requirements::default())
        .await;
    assert_eq!(cautious.calls(), 1, "not repeated by default");

    let willing = Flaky::new("willing", 1, timing_out);
    let routed = Router::new(vec![Route::new(willing.clone(), "m")])
        .retrying(policy(4, clock.clone()).repeating_timeouts())
        .chat(request(), Requirements::default())
        .await
        .expect("the second attempt answers");
    assert_eq!(willing.calls(), 2, "repeated once asked for");
    assert_eq!(routed.attempts, 2);
}

#[tokio::test]
async fn a_route_is_exhausted_before_the_next_one_is_tried() {
    // A rate limit is usually the same account whichever route you take. Falling through on
    // the first 429 spends a second provider's budget to learn what waiting would have said.
    let clock = Arc::new(Recording::default());
    let first = Flaky::new("first", 1, || Error::Transient("503".into()));
    let second = Flaky::new("second", 0, || Error::Transient("unused".into()));

    let routed = Router::new(vec![
        Route::new(first.clone(), "m"),
        Route::new(second.clone(), "m"),
    ])
    .retrying(policy(3, clock.clone()))
    .chat(request(), Requirements::default())
    .await
    .expect("the first route answers on its second attempt");

    assert_eq!(routed.route, "first/m");
    assert_eq!(first.calls(), 2);
    assert_eq!(second.calls(), 0, "the fallback was never needed");
}

#[tokio::test]
async fn a_route_that_runs_out_of_attempts_falls_through_to_the_next() {
    let clock = Arc::new(Recording::default());
    let first = Flaky::new("first", 99, || Error::Transient("503".into()));
    let second = Flaky::new("second", 0, || Error::Transient("unused".into()));

    let routed = Router::new(vec![
        Route::new(first.clone(), "m"),
        Route::new(second.clone(), "m"),
    ])
    .retrying(policy(2, clock.clone()))
    .chat(request(), Requirements::default())
    .await
    .expect("the second route answers");

    assert_eq!(routed.route, "second/m");
    assert_eq!(first.calls(), 2, "both its attempts were spent");
    assert_eq!(
        routed.attempts, 3,
        "two on the first route, one on the second"
    );
}

#[tokio::test]
async fn a_router_with_no_policy_calls_once_and_says_so() {
    // The default. Adding this module must not have changed what a router without a policy
    // does.
    let provider = Flaky::new("flaky", 1, || Error::Transient("503".into()));
    let outcome = Router::new(vec![Route::new(provider.clone(), "m")])
        .chat(request(), Requirements::default())
        .await;

    assert!(outcome.is_err());
    assert_eq!(provider.calls(), 1);
}
