//! What the router picks, and what it refuses to pick.
//!
//! Every test here is a claim about a decision somebody would otherwise make by hand in
//! every program that talks to more than one model.

use llmr::chat::message::{Message, Role, StopReason};
use llmr::chat::request::ChatRequest;
use llmr::chat::response::ChatResponse;
use llmr::model::{ModelCapabilities, ModelId, Reach};
use llmr::provider::Provider;
use llmr::router::{Requirements, Route, Router};
use llmr::{Error, ToolSchema, Usage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A provider with the capabilities a test gives it, that either answers or fails.
struct Stub {
    id: String,
    caps: ModelCapabilities,
    model: String,
    answer: Option<Error>,
    calls: AtomicUsize,
}

impl Stub {
    fn serving(id: &str, model: &str, caps: ModelCapabilities) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            caps,
            model: model.into(),
            answer: None,
            calls: AtomicUsize::new(0),
        })
    }

    fn failing(id: &str, model: &str, caps: ModelCapabilities, how: Error) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            caps,
            model: model.into(),
            answer: Some(how),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for Stub {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        (model.as_str() == self.model).then_some(self.caps)
    }

    async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.answer {
            Some(Error::Refused { category }) => Err(Error::Refused {
                category: category.clone(),
            }),
            Some(Error::Transient(why)) => Err(Error::Transient(why.clone())),
            // Reproduced rather than flattened, because which variant a provider returns is
            // exactly what the circuit breaker reads.
            Some(Error::InvalidRequest(why)) => Err(Error::InvalidRequest(why.clone())),
            Some(Error::Auth(why)) => Err(Error::Auth(why.clone())),
            Some(Error::RateLimited { retry_after }) => Err(Error::RateLimited {
                retry_after: *retry_after,
            }),
            Some(other) => Err(Error::Transient(other.to_string())),
            None => Ok(ChatResponse::new(
                Message {
                    role: Role::Assistant,
                    content: vec![llmr::ContentBlock::Text(format!("from {}", self.id))],
                },
                StopReason::EndTurn,
                Usage::absent().with_output(1),
                request.model,
            )),
        }
    }
}

fn plain(reach: Reach) -> ModelCapabilities {
    ModelCapabilities::none(reach).with_window(100_000, 4_096)
}

fn request() -> ChatRequest {
    ChatRequest::new("whatever", vec![Message::user("hi")])
}

fn with_tools() -> ChatRequest {
    request().with_tools(vec![ToolSchema::new(
        "search",
        "look it up",
        serde_json::json!({ "type": "object" }),
    )])
}

#[tokio::test]
async fn the_first_route_that_fits_answers() {
    let first = Stub::serving("first", "a", plain(Reach::FirstPartyApi));
    let second = Stub::serving("second", "b", plain(Reach::FirstPartyApi));

    let router = Router::new(vec![
        Route::new(Arc::clone(&first) as Arc<dyn Provider>, "a"),
        Route::new(Arc::clone(&second) as Arc<dyn Provider>, "b"),
    ]);

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .expect("an answer");

    assert_eq!(routed.route, "first/a");
    assert_eq!(
        second.calls(),
        0,
        "the second route was asked unnecessarily"
    );
    assert!(routed.fell_through.is_empty());
}

#[tokio::test]
async fn a_route_that_cannot_do_what_the_request_needs_is_skipped() {
    // Sending tools to a model that does not take them is paying for a reply that ignored
    // half of what you sent, and nothing about the reply says so.
    let toolless = Stub::serving("toolless", "a", plain(Reach::FirstPartyApi));
    let capable = Stub::serving("capable", "b", plain(Reach::FirstPartyApi).with_tools());

    let router = Router::new(vec![
        Route::new(Arc::clone(&toolless) as Arc<dyn Provider>, "a"),
        Route::new(Arc::clone(&capable) as Arc<dyn Provider>, "b"),
    ]);

    let sending = with_tools();
    let routed = router
        .chat(sending.clone(), Requirements::of(&sending))
        .await
        .expect("an answer");

    assert_eq!(routed.route, "capable/b");
    assert_eq!(toolless.calls(), 0);
    assert_eq!(routed.fell_through.len(), 1);
    assert!(routed.fell_through[0].why.contains("tools"));
}

#[tokio::test]
async fn private_data_does_not_leave_the_machine_because_the_local_model_was_busy() {
    // The claim this whole axis exists for. A fallback that ignores the floor is a fallback
    // that sends a customer record to a vendor the first time something is slow, and every
    // log line about it says the call succeeded.
    let local = Stub::failing(
        "ollama",
        "local",
        plain(Reach::SelfHosted),
        Error::Transient("connection refused".into()),
    );
    let hosted = Stub::serving("vendor", "hosted", plain(Reach::FirstPartyApi));

    let router = Router::new(vec![
        Route::new(Arc::clone(&local) as Arc<dyn Provider>, "local"),
        Route::new(Arc::clone(&hosted) as Arc<dyn Provider>, "hosted"),
    ]);

    let refused = router
        .chat(request(), Requirements::default().on_device())
        .await;

    assert!(refused.is_err(), "the data left the machine");
    assert_eq!(
        hosted.calls(),
        0,
        "a hosted provider was tried under an on-device requirement"
    );
    assert_eq!(local.calls(), 1, "the local one was not even attempted");
}

#[tokio::test]
async fn an_unreachable_provider_falls_through_to_the_next() {
    let broken = Stub::failing(
        "broken",
        "a",
        plain(Reach::FirstPartyApi),
        Error::Transient("connection refused".into()),
    );
    let working = Stub::serving("working", "b", plain(Reach::FirstPartyApi));

    let router = Router::new(vec![
        Route::new(Arc::clone(&broken) as Arc<dyn Provider>, "a"),
        Route::new(Arc::clone(&working) as Arc<dyn Provider>, "b"),
    ]);

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .expect("an answer");

    assert_eq!(routed.route, "working/b");
    assert_eq!(
        routed.fell_through.len(),
        1,
        "an answer that came second has to say so, or a provider going bad is invisible"
    );
    assert!(routed.fell_through[0].why.contains("connection refused"));
}

#[tokio::test]
async fn a_refusal_stops_rather_than_being_asked_of_the_next_model() {
    // Asking a second model the same question after the first declined is shopping a policy
    // decision around until something agrees. It is also the behaviour somebody would build
    // by accident, because a refusal arrives looking like any other error.
    let strict = Stub::failing(
        "strict",
        "a",
        plain(Reach::FirstPartyApi),
        Error::Refused {
            category: Some("the request asks for malware".into()),
        },
    );
    let other = Stub::serving("other", "b", plain(Reach::FirstPartyApi));

    let router = Router::new(vec![
        Route::new(Arc::clone(&strict) as Arc<dyn Provider>, "a"),
        Route::new(Arc::clone(&other) as Arc<dyn Provider>, "b"),
    ]);

    let refused = router.chat(request(), Requirements::default()).await;

    assert!(matches!(refused, Err(Error::Refused { .. })));
    assert_eq!(other.calls(), 0, "a refusal was shopped to the next model");
}

#[tokio::test]
async fn no_route_at_all_says_what_each_one_was_missing() {
    // A message that only said "no route available" leaves somebody comparing capability
    // tables by hand.
    let toolless = Stub::serving("toolless", "a", plain(Reach::FirstPartyApi));
    let router = Router::new(vec![Route::new(toolless as Arc<dyn Provider>, "a")]);

    let sending = with_tools();
    let refused = router
        .chat(sending.clone(), Requirements::of(&sending))
        .await;

    let message = refused.err().map(|e| e.to_string()).unwrap_or_default();
    assert!(message.contains("toolless/a"), "{message}");
    assert!(message.contains("tools"), "{message}");
}

#[tokio::test]
async fn the_route_decides_the_model_not_the_request() {
    // The router is choosing, so whatever model id a caller happened to put in the request
    // is replaced. Leaving it would send route B's provider route A's model name.
    let stub = Stub::serving("provider", "the-real-model", plain(Reach::FirstPartyApi));
    let router = Router::new(vec![Route::new(
        Arc::clone(&stub) as Arc<dyn Provider>,
        "the-real-model",
    )]);

    let routed = router
        .chat(
            ChatRequest::new("a-model-nobody-serves", vec![Message::user("hi")]),
            Requirements::default(),
        )
        .await
        .expect("an answer");

    assert_eq!(routed.response.model.as_str(), "the-real-model");
}

#[test]
fn a_route_nothing_can_select_is_reported_rather_than_silently_never_firing() {
    // Usually a typo in a model name. Without this it looks like a fallback that is
    // configured and simply never needed.
    let stub = Stub::serving("provider", "the-real-model", plain(Reach::FirstPartyApi));
    let router = Router::new(vec![
        Route::new(Arc::clone(&stub) as Arc<dyn Provider>, "the-real-model"),
        Route::new(Arc::clone(&stub) as Arc<dyn Provider>, "the-raal-model"),
    ]);

    assert_eq!(router.unusable(), vec!["provider/the-raal-model"]);
}

#[test]
fn requirements_read_from_a_request_do_not_invent_a_privacy_floor() {
    // Whether data may leave the machine is a fact about your program, and a request cannot
    // carry it. Guessing would either block everything or protect nothing.
    let needs = Requirements::of(&with_tools());
    assert!(needs.tools);
    assert!(!needs.must_stay_on_device);
    assert!(Requirements::of(&request()).on_device().must_stay_on_device);
}

/// A provider that reports whatever reachability a test gives it, and counts chat calls.
///
/// Separate from `Stub` so the preflight tests can say what `validate` answers without every
/// other test in this file growing a field it does not use.
struct Checked {
    id: String,
    model: String,
    access: llmr::Access,
    chats: AtomicUsize,
}

impl Checked {
    fn new(id: &str, model: &str, access: llmr::Access) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            model: model.into(),
            access,
            chats: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl Provider for Checked {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        (model.as_str() == self.model).then_some(plain(Reach::FirstPartyApi))
    }

    async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
        self.chats.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![llmr::ContentBlock::Text("answered".into())],
            },
            StopReason::EndTurn,
            Usage::absent().with_output(1),
            request.model,
        ))
    }

    async fn validate(&self, _model: &ModelId) -> llmr::Access {
        self.access.clone()
    }
}

#[tokio::test]
async fn preflight_reports_every_route_in_the_order_they_were_given() {
    // A report that reordered itself would be one nobody could compare against the config
    // file they wrote, which is the only place the order means anything.
    let ready = Checked::new("first", "a-model", llmr::Access::Ready);
    let denied = Checked::new(
        "second",
        "a-model",
        llmr::Access::denied("the key was rejected"),
    );
    let unknown = Checked::new(
        "third",
        "a-model",
        llmr::Access::unknown("the network was down"),
    );

    let router = Router::new(vec![
        Route::new(ready as Arc<dyn Provider>, "a-model"),
        Route::new(denied as Arc<dyn Provider>, "a-model"),
        Route::new(unknown as Arc<dyn Provider>, "a-model"),
    ]);

    let reached = router.preflight().await;
    let named: Vec<(String, String)> = reached
        .iter()
        .map(|(route, access)| (route.clone(), access.as_str().to_string()))
        .collect();

    assert_eq!(
        named,
        vec![
            ("first/a-model".to_string(), "ready".to_string()),
            ("second/a-model".to_string(), "denied".to_string()),
            ("third/a-model".to_string(), "unknown".to_string()),
        ]
    );
}

#[tokio::test]
async fn what_the_provider_said_survives_into_the_report() {
    // "denied" on its own sends somebody reading logs to guess. The reason is the whole
    // value of running this at startup rather than finding out later.
    let denied = Checked::new(
        "vendor",
        "a-model",
        llmr::Access::denied("the key was rejected"),
    );
    let router = Router::new(vec![Route::new(denied as Arc<dyn Provider>, "a-model")]);

    let reached = router.preflight().await;
    let said = reached
        .first()
        .and_then(|(_, access)| access.detail())
        .unwrap_or_default();

    assert_eq!(said, "the key was rejected");
}

#[tokio::test]
async fn preflight_asks_and_does_not_send_anything() {
    // A preflight that cost a call per route would be one people turn off, and then the
    // check exists in the code and not in any running program.
    let provider = Checked::new("vendor", "a-model", llmr::Access::Ready);
    let router = Router::new(vec![Route::new(
        Arc::clone(&provider) as Arc<dyn Provider>,
        "a-model",
    )]);

    let _ = router.preflight().await;

    assert_eq!(provider.chats.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_denied_route_is_reported_and_still_selectable() {
    // Reports, does not prune. A check that removed routes would drop a provider on the
    // strength of one moment, and the decision to stop using one belongs to a person.
    let denied = Checked::new("vendor", "a-model", llmr::Access::denied("no entitlement"));
    let router = Router::new(vec![Route::new(
        Arc::clone(&denied) as Arc<dyn Provider>,
        "a-model",
    )]);

    assert!(router.preflight().await[0].1.is_denied());

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .expect("the route is still there");
    assert_eq!(routed.route, "vendor/a-model");
    assert_eq!(denied.chats.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_provider_that_says_nothing_about_reachability_reports_unknown() {
    // `Stub` does not implement `validate`, so this is the trait's default arriving through
    // the router. Unknown rather than ready: nothing was asked, so nothing is known.
    let stub = Stub::serving("provider", "a-model", plain(Reach::FirstPartyApi));
    let router = Router::new(vec![Route::new(stub as Arc<dyn Provider>, "a-model")]);

    let reached = router.preflight().await;
    assert!(reached[0].1.is_unknown(), "{:?}", reached[0].1);
    assert!(!reached[0].1.is_ready());
}

#[tokio::test]
async fn preflight_and_unusable_answer_two_different_questions() {
    // A typo is a fact about the configuration and a rejected key is a fact about the
    // outside world. Both are startup problems and neither one finds the other.
    let reachable = Checked::new("vendor", "the-real-model", llmr::Access::Ready);
    let router = Router::new(vec![
        Route::new(
            Arc::clone(&reachable) as Arc<dyn Provider>,
            "the-real-model",
        ),
        Route::new(
            Arc::clone(&reachable) as Arc<dyn Provider>,
            "the-raal-model",
        ),
    ]);

    assert_eq!(router.unusable(), vec!["vendor/the-raal-model"]);

    // The typo route reports as reachable, because this provider was asked about a model
    // and answered. That is exactly why both checks are worth running.
    let reached = router.preflight().await;
    assert_eq!(reached.len(), 2);
    assert!(reached.iter().all(|(_, access)| access.is_ready()));
}

#[tokio::test]
async fn a_router_with_no_routes_reports_nothing_rather_than_failing() {
    let router = Router::new(Vec::new());
    assert!(router.preflight().await.is_empty());
}

// ---- Streaming -------------------------------------------------------------------------
//
// The one decision `Router::stream` makes that `Router::chat` does not: a route can be
// replaced right up until the caller has seen something, and never afterwards. Continuing a
// half written answer on a second model produces text nobody wrote, in one voice, with
// nothing downstream able to detect it.

/// What a provider's stream does, for the two cases a router has to tell apart.
enum Streams {
    /// Refuses to open. Nothing reached the caller, so another route may serve this.
    NeverOpens,
    /// Opens, hands over a word, then fails. The caller has seen text.
    BreaksAfterAWord,
    /// Opens and finishes.
    Whole,
}

struct Streamer {
    id: String,
    model: String,
    caps: ModelCapabilities,
    behaviour: Streams,
    opened: AtomicUsize,
}

impl Streamer {
    fn new(id: &str, model: &str, behaviour: Streams) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            model: model.into(),
            caps: plain(Reach::FirstPartyApi).with_streaming(),
            behaviour,
            opened: AtomicUsize::new(0),
        })
    }

    fn opened(&self) -> usize {
        self.opened.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for Streamer {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        (model.as_str() == self.model).then_some(self.caps)
    }

    async fn chat(&self, _request: ChatRequest) -> llmr::Result<ChatResponse> {
        Err(Error::Transient("this stub only streams".into()))
    }

    async fn stream(&self, _request: ChatRequest) -> llmr::Result<llmr::EventStream<'_>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        let events: Vec<llmr::Result<llmr::Event>> = match self.behaviour {
            Streams::NeverOpens => return Err(Error::Transient("nothing is listening".into())),
            Streams::BreaksAfterAWord => vec![
                Ok(llmr::Event::TextDelta(format!("from {} ", self.id))),
                Err(Error::Transient("the connection went away".into())),
            ],
            Streams::Whole => vec![
                Ok(llmr::Event::TextDelta(format!("from {}", self.id))),
                Ok(llmr::Event::Stopped {
                    reason: StopReason::EndTurn,
                    details: None,
                }),
            ],
        };
        Ok(Box::pin(Canned(events.into_iter())))
    }
}

struct Canned(std::vec::IntoIter<llmr::Result<llmr::Event>>);

impl futures_core::Stream for Canned {
    type Item = llmr::Result<llmr::Event>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(self.0.next())
    }
}

/// Everything the stream produced, and how it ended.
async fn read(events: llmr::EventStream<'_>) -> (String, llmr::Result<()>) {
    let mut transcript = llmr::Transcript::new("whatever");
    let outcome = transcript.drain(events).await;
    (transcript.finish().text(), outcome)
}

#[tokio::test]
async fn a_stream_falls_through_a_dead_provider_before_the_first_event() {
    // Nothing reached the caller, so nobody can tell this apart from a call that started on
    // the second route. Falling through is free here and only here.
    let dead = Streamer::new("dead", "m", Streams::NeverOpens);
    let alive = Streamer::new("alive", "m", Streams::Whole);
    let router = Router::new(vec![
        Route::new(dead.clone(), "m"),
        Route::new(alive.clone(), "m"),
    ]);

    let needs = Requirements::default().streaming();
    let (events, routed) = router
        .stream(request(), needs)
        .await
        .unwrap_or_else(|e| panic!("the live route should have served this: {e}"));

    let (text, outcome) = read(events).await;
    assert_eq!(text, "from alive");
    assert!(outcome.is_ok());

    assert_eq!(routed.route, "alive/m");
    assert_eq!(dead.opened(), 1, "the dead route was tried");
    assert_eq!(alive.opened(), 1);

    // And it says so, which is the difference between a router and an ordered try list.
    assert_eq!(routed.fell_through.len(), 1);
    assert_eq!(routed.fell_through[0].route, "dead/m");
    assert!(
        routed.fell_through[0].why.contains("nothing is listening"),
        "{}",
        routed.fell_through[0].why
    );
}

#[tokio::test]
async fn a_stream_that_fails_after_the_first_event_does_not_fall_through() {
    // The rule this method exists for. A second model continuing "from breaks " would
    // produce a sentence neither of them wrote, and the caller has already shown the first
    // half to somebody.
    let breaks = Streamer::new("breaks", "m", Streams::BreaksAfterAWord);
    let spare = Streamer::new("spare", "m", Streams::Whole);
    let router = Router::new(vec![
        Route::new(breaks.clone(), "m"),
        Route::new(spare.clone(), "m"),
    ]);

    let needs = Requirements::default().streaming();
    let (events, routed) = router
        .stream(request(), needs)
        .await
        .unwrap_or_else(|e| panic!("the stream opened, so this is Ok: {e}"));

    assert_eq!(routed.route, "breaks/m");
    assert!(routed.fell_through.is_empty(), "nothing had failed yet");

    let (text, outcome) = read(events).await;
    assert_eq!(text, "from breaks ", "what arrived is still yours");
    assert!(
        outcome.is_err(),
        "the failure arrives inside the stream and stays there"
    );
    assert_eq!(
        spare.opened(),
        0,
        "the spare route must never be asked to finish somebody else's sentence"
    );
}

#[tokio::test]
async fn a_route_that_only_pretends_to_stream_is_skipped_when_streaming_is_required() {
    // `Provider::stream` has a default that answers all at once at the end. That is a real
    // answer, and it is not one to route to when a person is watching a screen.
    let pretender = Stub::serving("pretender", "m", plain(Reach::FirstPartyApi));
    let real = Streamer::new("real", "m", Streams::Whole);
    let router = Router::new(vec![
        Route::new(pretender.clone(), "m"),
        Route::new(real.clone(), "m"),
    ]);

    let (_, routed) = router
        .stream(request(), Requirements::default().streaming())
        .await
        .unwrap_or_else(|e| panic!("the real one should have served this: {e}"));

    assert_eq!(routed.route, "real/m");
    assert_eq!(pretender.calls(), 0, "it was never asked");
    assert_eq!(routed.fell_through[0].why, "cannot do streaming");
}

#[tokio::test]
async fn a_refused_stream_stops_rather_than_being_asked_of_the_next_model() {
    // The same rule `chat` follows, and it has to be the same rule: a refusal is an answer
    // about the work. Shopping it around until something agrees is what you get by accident.
    let refuser = Stub::failing(
        "refuser",
        "m",
        plain(Reach::FirstPartyApi).with_streaming(),
        Error::Refused {
            category: Some("policy".into()),
        },
    );
    let spare = Streamer::new("spare", "m", Streams::Whole);
    let router = Router::new(vec![
        Route::new(refuser.clone(), "m"),
        Route::new(spare.clone(), "m"),
    ]);

    let outcome = router
        .stream(request(), Requirements::default().streaming())
        .await;

    assert!(matches!(outcome, Err(Error::Refused { .. })));
    assert_eq!(spare.opened(), 0);
}

// ---- Memory ----------------------------------------------------------------------------
//
// A router that starts at route 0 on every call is an ordered `try` list. These are the
// claims that make it something else.

use llmr::Breaker;
use std::time::Duration;

/// A breaker whose waits are short enough for a test to sit through.
fn quick() -> Breaker {
    Breaker::new(
        Duration::from_millis(80),
        Duration::from_millis(200),
        Duration::from_millis(300),
    )
}

#[tokio::test]
async fn a_route_that_keeps_failing_stops_being_tried_first() {
    let dead = Stub::failing(
        "dead",
        "m",
        plain(Reach::FirstPartyApi),
        Error::Transient("503".into()),
    );
    let alive = Stub::serving("alive", "m", plain(Reach::FirstPartyApi));
    let router = Router::new(vec![
        Route::new(dead.clone(), "m"),
        Route::new(alive.clone(), "m"),
    ])
    .breaking(quick());

    for _ in 0..4 {
        let routed = router
            .chat(request(), Requirements::default())
            .await
            .unwrap_or_else(|e| panic!("the live route answers: {e}"));
        assert_eq!(routed.route, "alive/m");
    }

    assert_eq!(
        dead.calls(),
        1,
        "tried once, and then left alone rather than waited on four times"
    );
    assert_eq!(alive.calls(), 4);
}

#[tokio::test]
async fn a_skipped_route_says_why_it_was_skipped() {
    // A router that will not say why it avoided something cannot be debugged at three in
    // the morning.
    let dead = Stub::failing(
        "dead",
        "m",
        plain(Reach::FirstPartyApi),
        Error::Transient("503".into()),
    );
    let alive = Stub::serving("alive", "m", plain(Reach::FirstPartyApi));
    let router = Router::new(vec![
        Route::new(dead.clone(), "m"),
        Route::new(alive.clone(), "m"),
    ])
    .breaking(quick());

    let _ = router.chat(request(), Requirements::default()).await;
    let routed = router
        .chat(request(), Requirements::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let skipped = &routed.fell_through[0];
    assert_eq!(skipped.route, "dead/m");
    assert!(
        skipped.why.contains("circuit open") && skipped.why.contains("1 failures"),
        "{}",
        skipped.why
    );

    // And the same fact is readable without making a request, for a dashboard.
    let resting = router.resting();
    assert_eq!(resting.len(), 1);
    assert_eq!(resting[0].0, "dead/m");
}

#[tokio::test]
async fn a_route_starts_being_tried_again_on_its_own() {
    // Nothing reopens a circuit. Time does, which is why there is no half open state and
    // nothing to call.
    let flaky = Stub::failing(
        "flaky",
        "m",
        plain(Reach::FirstPartyApi),
        Error::Transient("503".into()),
    );
    let alive = Stub::serving("alive", "m", plain(Reach::FirstPartyApi));
    let router = Router::new(vec![
        Route::new(flaky.clone(), "m"),
        Route::new(alive.clone(), "m"),
    ])
    .breaking(quick());

    let _ = router.chat(request(), Requirements::default()).await;
    assert_eq!(flaky.calls(), 1);

    let _ = router.chat(request(), Requirements::default()).await;
    assert_eq!(flaky.calls(), 1, "still resting");

    tokio::time::sleep(Duration::from_millis(120)).await;

    let _ = router.chat(request(), Requirements::default()).await;
    assert_eq!(flaky.calls(), 2, "tried again without anybody asking");
}

#[tokio::test]
async fn a_refusal_never_takes_a_route_out_of_the_router() {
    // The model answered, about the work. Closing a circuit here would remove a working
    // provider for having said no to one question.
    let refuser = Stub::failing(
        "refuser",
        "m",
        plain(Reach::FirstPartyApi),
        Error::Refused {
            category: Some("policy".into()),
        },
    );
    let router = Router::new(vec![Route::new(refuser.clone(), "m")]).breaking(quick());

    for _ in 0..3 {
        assert!(matches!(
            router.chat(request(), Requirements::default()).await,
            Err(Error::Refused { .. })
        ));
    }

    assert_eq!(refuser.calls(), 3, "asked every time");
    assert!(router.resting().is_empty());
}

#[tokio::test]
async fn a_malformed_request_never_takes_a_route_out_of_the_router() {
    // It will be malformed on the next route too, and on the next request. Nothing about
    // the provider is wrong.
    let picky = Stub::failing(
        "picky",
        "m",
        plain(Reach::FirstPartyApi),
        Error::InvalidRequest("no such parameter".into()),
    );
    let router = Router::new(vec![Route::new(picky.clone(), "m")]).breaking(quick());

    for _ in 0..3 {
        let _ = router.chat(request(), Requirements::default()).await;
    }
    assert_eq!(picky.calls(), 3);
    assert!(router.resting().is_empty());
}

#[tokio::test]
async fn a_router_with_every_route_resting_says_that_rather_than_unsupported() {
    // The difference decides whether somebody fixes their configuration or waits. An
    // `Unsupported` here would send a person looking for a capability that is not missing.
    let dead = Stub::failing(
        "dead",
        "m",
        plain(Reach::FirstPartyApi),
        Error::Transient("503".into()),
    );
    let router = Router::new(vec![Route::new(dead.clone(), "m")]).breaking(quick());

    let _ = router.chat(request(), Requirements::default()).await;
    let again = router.chat(request(), Requirements::default()).await;

    match again {
        Err(Error::Transient(why)) => assert!(why.contains("circuit open"), "{why}"),
        other => panic!("expected a transient failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_route_that_answers_forgets_that_it_ever_failed() {
    // Otherwise the next failure backs off as though the run of successes in between had
    // not happened.
    let sometimes = Arc::new(Flaky {
        fail_first: AtomicUsize::new(1),
        calls: AtomicUsize::new(0),
    });
    let router = Router::new(vec![Route::new(sometimes.clone(), "m")]).breaking(quick());

    let _ = router.chat(request(), Requirements::default()).await;
    assert_eq!(router.resting().len(), 1);

    tokio::time::sleep(Duration::from_millis(120)).await;
    let ok = router.chat(request(), Requirements::default()).await;
    assert!(ok.is_ok(), "it answers now");
    assert!(router.resting().is_empty(), "and the record is cleared");
}

/// Fails the first `fail_first` calls, then answers.
struct Flaky {
    fail_first: AtomicUsize,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for Flaky {
    fn id(&self) -> &str {
        "flaky"
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        (model.as_str() == "m").then_some(plain(Reach::FirstPartyApi))
    }

    async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_first.load(Ordering::SeqCst) > 0 {
            self.fail_first.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::Transient("not yet".into()));
        }
        Ok(ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![llmr::ContentBlock::Text("ok".into())],
            },
            StopReason::EndTurn,
            Usage::absent().with_output(1),
            request.model,
        ))
    }
}

#[tokio::test]
async fn without_a_policy_nothing_is_remembered() {
    // The behaviour every existing caller has. Not trying a provider is a decision with
    // consequences a library cannot weigh, so it is handed in like a retry policy is.
    let dead = Stub::failing(
        "dead",
        "m",
        plain(Reach::FirstPartyApi),
        Error::Transient("503".into()),
    );
    let alive = Stub::serving("alive", "m", plain(Reach::FirstPartyApi));
    let router = Router::new(vec![
        Route::new(dead.clone(), "m"),
        Route::new(alive.clone(), "m"),
    ]);

    for _ in 0..3 {
        let _ = router.chat(request(), Requirements::default()).await;
    }
    assert_eq!(dead.calls(), 3, "tried every time, as before");
    assert!(router.resting().is_empty());
}

// ---- Preflight, acting on what it learned ----------------------------------------------

#[tokio::test]
async fn a_route_denied_at_startup_is_not_tried_first_afterwards() {
    // Without this, `preflight` answered `Access` per route and nothing read it: a route
    // that said Denied at startup was still tried first in every request. Half of a feature.
    let denied = Checked::new("denied", "m", llmr::Access::denied("the key was rejected"));
    let ready = Checked::new("ready", "m", llmr::Access::Ready);
    let router = Router::new(vec![
        Route::new(denied.clone(), "m"),
        Route::new(ready.clone(), "m"),
    ])
    .breaking(quick());

    let reached = router.preflight().await;
    assert!(reached[0].1.is_denied());

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(routed.route, "ready/m");
    assert_eq!(
        denied.chats.load(Ordering::SeqCst),
        0,
        "it answered the question at startup and was not asked again"
    );

    // And it says which kind of skip this was. "Circuit open after 0 failures" would send
    // somebody looking for a failure that never happened.
    assert!(
        routed.fell_through[0].why.contains("denied at preflight"),
        "{}",
        routed.fell_through[0].why
    );
}

#[tokio::test]
async fn an_unknown_at_startup_never_takes_a_route_out_of_the_router() {
    // The whole reason `Access` has three variants rather than two. Treating an Unknown as
    // denied would remove a working provider for a network blip that had cleared before
    // anybody read the log.
    let unknown = Checked::new("unknown", "m", llmr::Access::unknown("no way to ask"));
    let router = Router::new(vec![Route::new(unknown.clone(), "m")]).breaking(quick());

    let reached = router.preflight().await;
    assert!(reached[0].1.is_unknown());
    assert!(
        router.resting().is_empty(),
        "ask again later means ask again"
    );

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(routed.route, "unknown/m");
}

#[tokio::test]
async fn a_denial_at_startup_ends_by_itself_like_any_other_rest() {
    // Rested rather than dropped. A key gets fixed while the program is running, and a
    // router that had removed the route would never find out.
    let denied = Checked::new("denied", "m", llmr::Access::denied("the key was rejected"));
    let router = Router::new(vec![Route::new(denied.clone(), "m")]).breaking(Breaker::new(
        Duration::from_millis(80),
        Duration::from_millis(200),
        Duration::from_millis(80),
    ));

    let _ = router.preflight().await;
    assert_eq!(router.resting().len(), 1);

    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(router.resting().is_empty());

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(routed.route, "denied/m");
}

#[tokio::test]
async fn preflight_without_a_breaker_still_only_reports() {
    // The behaviour every existing caller has. Nothing about calling preflight should start
    // changing which routes are tried unless the router was told it may.
    let denied = Checked::new("denied", "m", llmr::Access::denied("the key was rejected"));
    let router = Router::new(vec![Route::new(denied.clone(), "m")]);

    let reached = router.preflight().await;
    assert!(reached[0].1.is_denied());
    assert!(router.resting().is_empty());

    let _ = router.chat(request(), Requirements::default()).await;
    assert_eq!(denied.chats.load(Ordering::SeqCst), 1, "tried, as before");
}

// ---- Order -----------------------------------------------------------------------------
//
// The crate knew what every route charged and never consulted any of it when choosing one.

use llmr::{Order, PriceBook};

/// A book pricing one model, at these rates per million.
fn priced(model: &str, input: &str, output: &str) -> Arc<PriceBook> {
    let toml = format!(
        r#"
id             = "test"
provider       = "test"
effective_from = "2026-08-01"
source         = "a fixture"
verified_at    = "2026-08-31"
currency       = "USD"

[[price]]
model  = "{model}"
input  = "{input}"
output = "{output}"
"#
    );
    Arc::new(PriceBook::parse(&toml).unwrap_or_else(|e| panic!("the fixture book: {e}")))
}

#[tokio::test]
async fn cheapest_picks_by_price_rather_than_by_the_order_you_wrote() {
    let dear = Stub::serving("dear", "m", plain(Reach::FirstPartyApi));
    let cheap = Stub::serving("cheap", "m", plain(Reach::FirstPartyApi));

    let router = Router::new(vec![
        Route::new(dear.clone(), "m").priced_by(priced("m", "15.00", "75.00")),
        Route::new(cheap.clone(), "m").priced_by(priced("m", "1.00", "5.00")),
    ])
    .ordering(Order::Cheapest);

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(routed.route, "cheap/m");
    assert_eq!(dear.calls(), 0);
}

#[tokio::test]
async fn an_unpriced_route_is_never_chosen_because_it_looked_free() {
    // The trap. `PriceBook::price` answers None for a model it does not list, and the
    // obvious implementation reads None as zero and puts every unpriced route first, which
    // is precisely backwards and confidently so.
    let unpriced = Stub::serving("unpriced", "m", plain(Reach::FirstPartyApi));
    let dear = Stub::serving("dear", "m", plain(Reach::FirstPartyApi));

    let router = Router::new(vec![
        Route::new(unpriced.clone(), "m"),
        Route::new(dear.clone(), "m").priced_by(priced("m", "15.00", "75.00")),
    ])
    .ordering(Order::Cheapest);

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(
        routed.route, "dear/m",
        "the expensive route with a price beats the one nobody can price"
    );
    assert_eq!(unpriced.calls(), 0);
}

#[tokio::test]
async fn a_book_without_a_row_for_the_model_is_unpriced_too() {
    // A route can carry a whole price book and still be unpriced for the model it serves,
    // and that is the same fact as having no book at all.
    let wrong_book = Stub::serving("wrong-book", "m", plain(Reach::FirstPartyApi));
    let priced_route = Stub::serving("priced", "m", plain(Reach::FirstPartyApi));

    let router = Router::new(vec![
        Route::new(wrong_book.clone(), "m").priced_by(priced("something-else", "0.01", "0.01")),
        Route::new(priced_route.clone(), "m").priced_by(priced("m", "15.00", "75.00")),
    ])
    .ordering(Order::Cheapest);

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(routed.route, "priced/m");
}

#[tokio::test]
async fn an_ordering_never_overrules_what_the_request_needs() {
    // Cheapest among the routes that can serve it, not cheapest full stop. A router that
    // reordered past a capability would pay less for a reply that ignored half the request.
    let cheap_but_plain = Stub::serving("cheap", "m", plain(Reach::FirstPartyApi));
    let dear_with_tools = Stub::serving("dear", "m", plain(Reach::FirstPartyApi).with_tools());

    let router = Router::new(vec![
        Route::new(cheap_but_plain.clone(), "m").priced_by(priced("m", "0.10", "0.10")),
        Route::new(dear_with_tools.clone(), "m").priced_by(priced("m", "99.00", "99.00")),
    ])
    .ordering(Order::Cheapest);

    let routed = router
        .chat(with_tools(), Requirements::of(&with_tools()))
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(routed.route, "dear/m");
    assert_eq!(cheap_but_plain.calls(), 0);
}

#[tokio::test]
async fn healthiest_puts_the_route_that_has_been_failing_last() {
    let bad = Stub::failing(
        "bad",
        "m",
        plain(Reach::FirstPartyApi),
        Error::Transient("503".into()),
    );
    let good = Stub::serving("good", "m", plain(Reach::FirstPartyApi));

    // No breaker. The count is kept anyway, so this works on its own.
    let router = Router::new(vec![
        Route::new(bad.clone(), "m"),
        Route::new(good.clone(), "m"),
    ])
    .ordering(Order::Healthiest);

    for _ in 0..3 {
        let routed = router
            .chat(request(), Requirements::default())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(routed.route, "good/m");
    }

    assert_eq!(
        bad.calls(),
        1,
        "tried once, and then sorted below the route that works"
    );
}

#[tokio::test]
async fn an_ordering_that_cannot_tell_two_routes_apart_keeps_the_order_you_wrote() {
    // A stable sort, so a router where nothing has failed and nothing is priced behaves
    // exactly like the one people already have.
    let first = Stub::serving("first", "m", plain(Reach::FirstPartyApi));
    let second = Stub::serving("second", "m", plain(Reach::FirstPartyApi));

    for order in [Order::Cheapest, Order::Healthiest, Order::AsListed] {
        let router = Router::new(vec![
            Route::new(first.clone(), "m"),
            Route::new(second.clone(), "m"),
        ])
        .ordering(order);

        let routed = router
            .chat(request(), Requirements::default())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(routed.route, "first/m", "under {order:?}");
    }
}

// ---- Deadline --------------------------------------------------------------------------
//
// A transport can have a timeout and a retry policy can have delays. Nothing bounded the
// sum, so a caller waiting on an agent had no way to say "answer or fail within twenty
// seconds".

use llmr::{Delay, Retry};

/// A wait that refuses to happen, so a test can assert nothing waited.
struct NeverWaits;

#[async_trait::async_trait]
impl Delay for NeverWaits {
    async fn sleep(&self, how_long: Duration) {
        panic!("the deadline should have stopped this before waiting {how_long:?}");
    }
}

/// A provider that takes this long to fail.
struct Slow {
    id: String,
    takes: Duration,
    calls: AtomicUsize,
}

impl Slow {
    fn new(id: &str, takes: Duration) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            takes,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl Provider for Slow {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        (model.as_str() == "m").then_some(plain(Reach::FirstPartyApi))
    }

    async fn chat(&self, _request: ChatRequest) -> llmr::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.takes).await;
        Err(Error::Transient("no".into()))
    }
}

#[tokio::test]
async fn a_routed_call_stops_when_the_deadline_is_spent_rather_than_trying_the_next_route() {
    let first = Slow::new("first", Duration::from_millis(120));
    let second = Slow::new("second", Duration::from_millis(120));
    let third = Slow::new("third", Duration::from_millis(120));

    let router = Router::new(vec![
        Route::new(first.clone(), "m"),
        Route::new(second.clone(), "m"),
        Route::new(third.clone(), "m"),
    ])
    .within_deadline(Duration::from_millis(200));

    let started = std::time::Instant::now();
    let outcome = router.chat(request(), Requirements::default()).await;
    let took = started.elapsed();

    match outcome {
        Err(Error::Timeout { elapsed }) => assert!(elapsed >= Duration::from_millis(200)),
        other => panic!("expected a timeout, got {other:?}"),
    }

    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        third.calls.load(Ordering::SeqCst),
        0,
        "there was no time left to start it in"
    );
    assert!(
        took < Duration::from_millis(400),
        "it stopped rather than spending a third timeout: {took:?}"
    );
}

#[tokio::test]
async fn giving_up_on_time_is_told_apart_from_giving_up_on_failures() {
    // Two different fixes. One is a provider to look at, the other is a deadline to raise,
    // and a caller that cannot tell them apart goes looking in the wrong place.
    let routes = || {
        vec![
            Route::new(Slow::new("slow", Duration::from_millis(120)), "m"),
            Route::new(Slow::new("spare", Duration::from_millis(120)), "m"),
        ]
    };

    let out_of_time = Router::new(routes())
        .within_deadline(Duration::from_millis(60))
        .chat(request(), Requirements::default())
        .await;
    assert!(
        matches!(out_of_time, Err(Error::Timeout { .. })),
        "{out_of_time:?}"
    );

    // The same routes failing the same way, with time to spare. The error is the provider's
    // own, not a deadline, and the two are not the same problem.
    let everything_failed = Router::new(routes())
        .within_deadline(Duration::from_secs(30))
        .chat(request(), Requirements::default())
        .await;
    assert!(
        matches!(everything_failed, Err(Error::Transient(_))),
        "{everything_failed:?}"
    );
}

#[tokio::test]
async fn the_deadline_beats_the_retry_policy_when_they_disagree() {
    // A policy that says three attempts is a maximum, not a promise. Waiting into the
    // deadline spends what is left on a call that has nowhere to arrive.
    let failing = Stub::failing(
        "failing",
        "m",
        plain(Reach::FirstPartyApi),
        Error::Transient("503".into()),
    );
    let router = Router::new(vec![Route::new(failing.clone(), "m")])
        .retrying(Retry::with_delay(3, Arc::new(NeverWaits)).with_base(Duration::from_millis(500)))
        .within_deadline(Duration::from_millis(100));

    // The delay panics if it is ever asked to wait, so reaching the end of this at all is
    // the assertion: the router refused a 500ms wait it had 100ms of deadline left for.
    let outcome = router.chat(request(), Requirements::default()).await;

    assert!(matches!(outcome, Err(Error::Timeout { .. })), "{outcome:?}");
    assert_eq!(failing.calls(), 1, "the second attempt never started");
}

#[tokio::test]
async fn a_deadline_that_is_never_reached_changes_nothing() {
    let alive = Stub::serving("alive", "m", plain(Reach::FirstPartyApi));
    let router =
        Router::new(vec![Route::new(alive.clone(), "m")]).within_deadline(Duration::from_secs(30));

    let routed = router
        .chat(request(), Requirements::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(routed.route, "alive/m");
    assert!(routed.fell_through.is_empty());
}

// ---- Budget ----------------------------------------------------------------------------
//
// The ledger recorded what a run cost and nothing stopped it. A bot spends money with
// nobody watching, and a loop that retries is found out about on the invoice.

use llmr::{Budget, Micros, Transcript, Usage as U};

/// A provider that answers with a measured usage, so the budget has something to charge.
struct Metered {
    id: String,
    output: u64,
    calls: AtomicUsize,
}

impl Metered {
    fn new(id: &str, output: u64) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            output,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl Provider for Metered {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        (model.as_str() == "m").then_some(plain(Reach::FirstPartyApi))
    }

    async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![llmr::ContentBlock::Text("answered".into())],
            },
            StopReason::EndTurn,
            U::absent()
                .with_input(0)
                .with_cache_read(0)
                .with_cache_write(0)
                .with_output(self.output),
            request.model,
        ))
    }
}

/// One dollar in and ten out, per million.
fn dollar_book() -> Arc<PriceBook> {
    priced("m", "1.00", "10.00")
}

#[tokio::test]
async fn a_run_stops_when_the_cap_is_spent() {
    // A million output tokens at ten dollars a million is ten dollars a call, against a cap
    // of twenty five.
    let provider = Metered::new("metered", 1_000_000);
    let router = Router::new(vec![
        Route::new(provider.clone(), "m").priced_by(dollar_book())
    ])
    .within(Budget::of(Micros(25_000_000), "USD"));

    for _ in 0..2 {
        assert!(router
            .chat(request(), Requirements::default())
            .await
            .is_ok());
    }

    let spending = router
        .spending()
        .unwrap_or_else(|| panic!("there is a budget"));
    assert_eq!(spending.spent, Micros(20_000_000));
    assert_eq!(spending.remaining, Micros(5_000_000));
    assert!(spending.is_exact());

    // The third would take it over, and the reply alone says so before anything is sent.
    let sending = request().with_max_tokens(1_000_000);
    let refused = router.chat(sending, Requirements::default()).await;
    match refused {
        Err(Error::OverBudget(why)) => assert!(why.contains("only 5.000000 is left"), "{why}"),
        other => panic!("expected a budget refusal, got {other:?}"),
    }
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "nothing was sent, so nothing was billed"
    );
}

#[tokio::test]
async fn a_spent_budget_refuses_everything() {
    let provider = Metered::new("metered", 1_000_000);
    let router = Router::new(vec![
        Route::new(provider.clone(), "m").priced_by(dollar_book())
    ])
    .within(Budget::of(Micros(5_000_000), "USD"));

    assert!(router
        .chat(request(), Requirements::default())
        .await
        .is_ok());
    assert_eq!(router.spending().map(|s| s.remaining), Some(Micros(0)));

    let refused = router.chat(request(), Requirements::default()).await;
    match refused {
        Err(Error::OverBudget(why)) => assert!(why.contains("is spent"), "{why}"),
        other => panic!("expected a budget refusal, got {other:?}"),
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_unpriced_route_is_refused_under_a_budget_rather_than_run_blind() {
    // A cap that cannot be checked is not a cap. This is the setting under which "the run
    // spent at most five dollars" is a sentence somebody should believe.
    let unpriced = Metered::new("unpriced", 1_000);
    let router = Router::new(vec![Route::new(unpriced.clone(), "m")])
        .within(Budget::of(Micros(5_000_000), "USD"));

    let refused = router.chat(request(), Requirements::default()).await;
    match refused {
        Err(Error::OverBudget(why)) => {
            assert!(why.contains("cannot be measured against a budget"), "{why}");
        }
        other => panic!("expected a budget refusal, got {other:?}"),
    }
    assert_eq!(unpriced.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_unpriced_route_can_be_allowed_out_loud_and_makes_the_figure_a_floor() {
    let unpriced = Metered::new("unpriced", 1_000);
    let router = Router::new(vec![Route::new(unpriced.clone(), "m")])
        .within(Budget::of(Micros(5_000_000), "USD").allowing_unpriced());

    assert!(router
        .chat(request(), Requirements::default())
        .await
        .is_ok());

    let spending = router
        .spending()
        .unwrap_or_else(|| panic!("there is a budget"));
    assert_eq!(spending.spent, Micros(0), "nobody could price it");
    assert_eq!(
        spending.unmeasured, 1,
        "counted rather than ignored, so the figure reads as the floor it is"
    );
    assert!(!spending.is_exact());
    assert_eq!(unpriced.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_route_priced_in_another_currency_is_refused_rather_than_converted() {
    // There is no exchange rate in this crate and there should not be one: a rate has a date
    // and a source exactly like a price does.
    let euros = Arc::new(
        PriceBook::parse(
            r#"
id             = "eur"
provider       = "test"
effective_from = "2026-08-01"
source         = "a fixture"
verified_at    = "2026-08-31"
currency       = "EUR"

[[price]]
model  = "m"
input  = "1.00"
output = "10.00"
"#,
        )
        .unwrap_or_else(|e| panic!("{e}")),
    );

    let provider = Metered::new("euros", 1_000);
    let router = Router::new(vec![Route::new(provider.clone(), "m").priced_by(euros)])
        .within(Budget::of(Micros(5_000_000), "USD"));

    match router.chat(request(), Requirements::default()).await {
        Err(Error::OverBudget(why)) => assert!(why.contains("no exchange rate"), "{why}"),
        other => panic!("expected a budget refusal, got {other:?}"),
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_streamed_call_is_counted_as_unmeasured_until_somebody_settles_it() {
    // Its usage arrives in a transcript this router never sees, so the honest record is
    // "one call I could not price" rather than a silent zero.
    let real = Streamer::new("real", "m", Streams::Whole);
    let router = Router::new(vec![Route::new(real.clone(), "m").priced_by(dollar_book())])
        .within(Budget::of(Micros(25_000_000), "USD"));

    let (events, routed) = router
        .stream(request(), Requirements::default().streaming())
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    let before = router
        .spending()
        .unwrap_or_else(|| panic!("there is a budget"));
    assert!(!before.is_exact(), "nothing has priced this yet");
    assert_eq!(before.unmeasured, 1);

    let mut transcript = Transcript::new("m");
    let _ = transcript.drain(events).await;
    let reply = transcript.finish();

    // What the caller has after reading the stream, priced against the same book the budget
    // would have used.
    let charged = router.charge(
        &routed.route,
        &reply.model,
        &U::absent()
            .with_input(0)
            .with_cache_read(0)
            .with_cache_write(0)
            .with_output(1_000_000),
    );

    assert!(charged);
    let after = router
        .spending()
        .unwrap_or_else(|| panic!("there is a budget"));
    assert_eq!(after.spent, Micros(10_000_000));
    assert!(after.is_exact(), "settled");
}

#[tokio::test]
async fn a_budget_refusal_never_takes_a_route_out_of_the_router() {
    // Nothing was sent, so nothing about the provider was learned. Closing a circuit here
    // would punish a working provider for the caller running out of money.
    let provider = Metered::new("metered", 1_000_000);
    let router = Router::new(vec![
        Route::new(provider.clone(), "m").priced_by(dollar_book())
    ])
    .within(Budget::of(Micros(5_000_000), "USD"))
    .breaking(quick());

    let _ = router.chat(request(), Requirements::default()).await;
    let _ = router.chat(request(), Requirements::default()).await;

    assert!(router.resting().is_empty());
}

#[tokio::test]
async fn without_a_budget_nothing_is_capped_or_counted() {
    let provider = Metered::new("metered", 1_000_000);
    let router = Router::new(vec![
        Route::new(provider.clone(), "m").priced_by(dollar_book())
    ]);

    for _ in 0..5 {
        assert!(router
            .chat(request(), Requirements::default())
            .await
            .is_ok());
    }
    assert_eq!(router.spending(), None);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 5);
}
