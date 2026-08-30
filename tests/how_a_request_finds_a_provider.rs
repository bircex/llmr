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
