//! One provider, many callers, at the same time.
//!
//! A provider is shared. `chat` takes `&self`, so anything a provider holds is reachable
//! from every task at once. Two things can go wrong and neither shows up in a single
//! threaded test:
//!
//! * A lock held across an await deadlocks. The lint `await_holding_lock` is denied across
//!   the crate to stop that being written, and this is the check that the whole path holds
//!   under real tasks rather than only where the lint could see.
//! * A lock held around the call serialises it. That is not a bug that fails, it is a bug
//!   that makes a hundred concurrent calls take a hundred times as long, and nobody notices
//!   until production.
//!
//! Every test here has a timeout. A deadlock does not fail an assertion, it hangs, and a
//! hanging test is a test that gets killed by CI with no explanation.

#![cfg(feature = "anthropic")]

use modelreach::http::{HttpRequest, HttpResponse, HttpTransport};
use modelreach::providers::anthropic::Anthropic;
use modelreach::{ChatRequest, Message, Provider, Reach, Registry, Secret};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Counts calls and waits a little, so overlapping calls actually overlap.
///
/// The counter is an atomic rather than a mutex on purpose. This helper is not the thing
/// under test, and a mutex here would be a lock in the path that could hide or cause the
/// very problem the tests are looking for.
struct Slow {
    started: AtomicUsize,
    finished: AtomicUsize,
    delay: Duration,
}

impl Slow {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            delay,
        })
    }
}

#[async_trait::async_trait]
impl HttpTransport for Slow {
    async fn send(&self, _request: HttpRequest) -> modelreach::Result<HttpResponse> {
        self.started.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.finished.fetch_add(1, Ordering::SeqCst);
        Ok(HttpResponse::new(
            200,
            serde_json::to_vec(&serde_json::json!({
                "model": "claude-sonnet-5",
                "stop_reason": "end_turn",
                "content": [{ "type": "text", "text": "ok" }],
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            }))
            .unwrap_or_default(),
        ))
    }
}

fn provider(transport: Arc<Slow>) -> Arc<Anthropic> {
    Arc::new(Anthropic::new(
        transport,
        Secret::new("key", "sk-test"),
        Arc::new(Registry::empty("anthropic", Reach::FirstPartyApi)),
    ))
}

fn a_request() -> ChatRequest {
    ChatRequest::new("claude-sonnet-5", vec![Message::user("hi")])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hundred_calls_through_one_provider_all_finish() {
    let transport = Slow::new(Duration::from_millis(5));
    let shared = provider(Arc::clone(&transport));

    let calls: Vec<_> = (0..100)
        .map(|_| {
            let provider = Arc::clone(&shared);
            tokio::spawn(async move { provider.chat(a_request()).await })
        })
        .collect();

    // The timeout is the test. A deadlock does not fail an assertion, it hangs, and this
    // turns a hang into a message.
    let all = tokio::time::timeout(Duration::from_secs(30), async {
        let mut ok = 0;
        for call in calls {
            if call.await.is_ok_and(|r| r.is_ok()) {
                ok += 1;
            }
        }
        ok
    })
    .await;

    assert_eq!(
        all,
        Ok(100),
        "a hundred concurrent calls did not all finish, which is what a lock held across \
         an await looks like from outside"
    );
    assert_eq!(transport.finished.load(Ordering::SeqCst), 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_overlap_rather_than_queueing_behind_each_other() {
    // The failure this catches does not hang. A provider that took a lock around the whole
    // call would still answer every request, and would answer them one at a time, so fifty
    // calls of fifty milliseconds each would take two and a half seconds instead of fifty
    // milliseconds. Nothing fails, it is just slow, and nobody notices until production.
    let transport = Slow::new(Duration::from_millis(50));
    let shared = provider(Arc::clone(&transport));

    let started = std::time::Instant::now();
    let calls: Vec<_> = (0..50)
        .map(|_| {
            let provider = Arc::clone(&shared);
            tokio::spawn(async move { provider.chat(a_request()).await })
        })
        .collect();
    for call in calls {
        let _ = call.await;
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(1_000),
        "fifty calls of fifty milliseconds took {elapsed:?}. They ran one after another, \
         so something in the provider is serialising them"
    );
    assert_eq!(transport.finished.load(Ordering::SeqCst), 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_can_be_sent_between_threads() {
    // `Provider: Send + Sync` is in the trait bounds, and this is the check that a concrete
    // provider actually satisfies them once it holds a transport and a registry. It is a
    // compile time property, so the value of the test is that it fails to build rather than
    // fails to run.
    fn assert_shareable<T: Send + Sync + 'static>(_: &T) {}

    let shared = provider(Slow::new(Duration::from_millis(1)));
    assert_shareable(&*shared);

    let moved = Arc::clone(&shared);
    let elsewhere = tokio::spawn(async move { moved.chat(a_request()).await });
    assert!(elsewhere.await.is_ok_and(|r| r.is_ok()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_call_failing_does_not_take_the_others_with_it() {
    // A shared provider must not carry a failure from one call into another. This is the
    // shape a poisoned lock would produce: the first panic makes every later call fail.
    struct SometimesBroken {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl HttpTransport for SometimesBroken {
        async fn send(&self, _request: HttpRequest) -> modelreach::Result<HttpResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            if n.is_multiple_of(2) {
                return Ok(HttpResponse::new(500, b"upstream fell over".to_vec()));
            }
            Ok(HttpResponse::new(
                200,
                serde_json::to_vec(&serde_json::json!({
                    "model": "claude-sonnet-5",
                    "stop_reason": "end_turn",
                    "content": [{ "type": "text", "text": "ok" }]
                }))
                .unwrap_or_default(),
            ))
        }
    }

    let shared = Arc::new(Anthropic::new(
        Arc::new(SometimesBroken {
            calls: AtomicUsize::new(0),
        }),
        Secret::new("key", "sk-test"),
        Arc::new(Registry::empty("anthropic", Reach::FirstPartyApi)),
    ));

    let calls: Vec<_> = (0..20)
        .map(|_| {
            let provider = Arc::clone(&shared);
            tokio::spawn(async move { provider.chat(a_request()).await })
        })
        .collect();

    let mut succeeded = 0;
    let mut failed = 0;
    for call in calls {
        match call.await {
            Ok(Ok(_)) => succeeded += 1,
            Ok(Err(_)) => failed += 1,
            Err(join) => panic!("a task did not finish: {join}"),
        }
    }

    assert_eq!(succeeded + failed, 20);
    assert!(
        succeeded > 0,
        "every call failed after the first one did, which is what a poisoned lock looks like"
    );
}
