//! What the Bedrock protocol sends, and what it deliberately does not.
//!
//! The interesting claims here are about what it *reuses* and what it *refuses to carry*:
//! Claude through Amazon must send the same JSON as Claude direct, and must not report the
//! same reach.

#![cfg(feature = "bedrock")]

use llmr::providers::{anthropic, bedrock};
use llmr::transport::{HttpRequest, HttpResponse, HttpTransport};
use llmr::{ChatRequest, Effort, Message, Provider, Reach, Registry, Secret, Thinking};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct Recorded {
    reply: HttpResponse,
    sent: Mutex<Vec<HttpRequest>>,
}

impl Recorded {
    fn replying(body: Value) -> Arc<Self> {
        Arc::new(Self {
            reply: HttpResponse::new(200, serde_json::to_vec(&body).unwrap_or_default()),
            sent: Mutex::new(Vec::new()),
        })
    }
    fn body(&self) -> Value {
        let sent = self.sent.lock().expect("not poisoned");
        serde_json::from_slice(&sent.first().expect("something was sent").body)
            .unwrap_or(Value::Null)
    }
    fn url(&self) -> String {
        let sent = self.sent.lock().expect("not poisoned");
        sent.first().map(|r| r.url.clone()).unwrap_or_default()
    }
    fn headers(&self) -> Vec<(String, String)> {
        let sent = self.sent.lock().expect("not poisoned");
        sent.first().map(|r| r.headers.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl HttpTransport for Recorded {
    async fn send(&self, request: HttpRequest) -> llmr::Result<HttpResponse> {
        self.sent.lock().expect("not poisoned").push(request);
        Ok(self.reply.clone())
    }
}

fn reply() -> Value {
    json!({
        "model": "claude-sonnet-5",
        "stop_reason": "end_turn",
        "content": [{ "type": "text", "text": "Four." }],
        "usage": { "input_tokens": 12, "output_tokens": 4 },
    })
}

fn request() -> ChatRequest {
    ChatRequest::new(
        "anthropic.claude-sonnet-5-v1:0",
        vec![Message::user("two plus two")],
    )
}

fn provider(transport: Arc<Recorded>) -> bedrock::api::Bedrock {
    bedrock::api::anthropic_family(
        "eu-west-1",
        transport,
        Arc::new(Registry::empty("bedrock", Reach::CloudPartner)),
    )
}

#[tokio::test]
async fn the_region_and_the_model_are_both_in_the_address() {
    let transport = Recorded::replying(reply());
    let _ = provider(Arc::clone(&transport)).chat(request()).await;

    assert_eq!(
        transport.url(),
        "https://bedrock-runtime.eu-west-1.amazonaws.com/model/anthropic.claude-sonnet-5-v1:0/invoke"
    );
}

#[tokio::test]
async fn a_region_qualified_model_id_survives_untouched() {
    // Cross region profiles look like `eu.anthropic.claude-...`. `ModelId` is a string for
    // exactly this reason: anything that parsed model names would reject half of these.
    let transport = Recorded::replying(reply());
    let _ = provider(Arc::clone(&transport))
        .chat(ChatRequest::new(
            "eu.anthropic.claude-sonnet-5-v1:0",
            vec![Message::user("hi")],
        ))
        .await;

    assert!(
        transport
            .url()
            .ends_with("/model/eu.anthropic.claude-sonnet-5-v1:0/invoke"),
        "{}",
        transport.url()
    );
}

#[tokio::test]
async fn the_model_moves_from_the_body_to_the_path_and_a_version_takes_its_place() {
    let transport = Recorded::replying(reply());
    let _ = provider(Arc::clone(&transport)).chat(request()).await;

    let body = transport.body();
    // Bedrock rejects a body that also names a model.
    assert!(body.get("model").is_none(), "{body}");
    assert_eq!(body["anthropic_version"], "bedrock-2023-05-31");
}

#[tokio::test]
async fn no_credential_is_attached_because_the_transport_signs() {
    // A bearer token beside a SigV4 signature is at best ignored and at worst a request
    // Bedrock rejects. There is deliberately no key argument to attach one from.
    let transport = Recorded::replying(reply());
    let _ = provider(Arc::clone(&transport)).chat(request()).await;

    let names: Vec<String> = transport
        .headers()
        .into_iter()
        .map(|(n, _)| n.to_lowercase())
        .collect();
    assert!(!names.contains(&"authorization".to_string()), "{names:?}");
    assert!(!names.contains(&"x-api-key".to_string()), "{names:?}");
    assert!(names.contains(&"content-type".to_string()), "{names:?}");
}

#[tokio::test]
async fn claude_through_amazon_sends_what_claude_direct_sends() {
    // The reason this protocol reuses the Messages translation rather than copying it. Two
    // copies would disagree the first time one was fixed, and the difference would show up
    // as a behaviour change nobody asked for on one of the two routes.
    let asked = request()
        .with_system("be brief")
        .with_thinking(Thinking::On(Effort::Low));

    let through_amazon = Recorded::replying(reply());
    let _ = provider(Arc::clone(&through_amazon))
        .chat(asked.clone())
        .await;

    let direct = Recorded::replying(reply());
    let _ = anthropic::api::with(
        Arc::clone(&direct) as Arc<dyn HttpTransport>,
        Secret::new("key", "sk-test"),
        Arc::new(Registry::empty("anthropic", Reach::FirstPartyApi)),
    )
    .chat(asked)
    .await;

    let mut theirs = direct.body();
    let mut ours = through_amazon.body();
    // The two documented differences, and nothing else.
    if let Some(o) = theirs.as_object_mut() {
        o.remove("model");
    }
    if let Some(o) = ours.as_object_mut() {
        o.remove("anthropic_version");
    }
    assert_eq!(ours, theirs, "the two routes must send the same request");
}

#[tokio::test]
async fn the_reach_is_the_partner_rather_than_the_vendor() {
    // The first real use of `CloudPartner`. A program deciding where its data may go has to
    // be told Amazon, whoever trained the model.
    let toml = r#"
provider = "bedrock"
reach = "CloudPartner"

[[model]]
id = "anthropic.claude-sonnet-5-v1:0"
context_window = 200000
max_output = 8192
source = "a recorded fixture"
verified_at = "2026-08-30"
"#;
    let registry = Arc::new(Registry::parse(toml).unwrap_or_else(|e| panic!("{e}")));
    let served = bedrock::api::anthropic_family("eu-west-1", Recorded::replying(reply()), registry);

    let caps = served
        .capabilities(&"anthropic.claude-sonnet-5-v1:0".into())
        .expect("a model it serves");
    assert_eq!(caps.reach, Reach::CloudPartner);
    assert!(!caps.reach.is_on_device());
    assert!(
        !caps.reach.uses_local_credential(),
        "an Amazon credential is not a local one"
    );
}

#[tokio::test]
async fn a_streamed_call_still_answers_even_though_bedrock_frames_differently() {
    // Bedrock streams over its own binary framing, which the shared frame reader cannot
    // read, so `stream_body` stays at its default and the call falls back to one burst.
    // An answer rather than a refusal.
    use llmr::chat::stream::Transcript;

    let served = provider(Recorded::replying(reply()));
    let mut transcript = Transcript::new("anthropic.claude-sonnet-5-v1:0");
    let outcome = match served.stream(request()).await {
        Ok(stream) => transcript.drain(stream).await,
        Err(e) => Err(e),
    };

    assert!(outcome.is_ok(), "{outcome:?}");
    let assembled = transcript.finish();
    assert_eq!(assembled.text(), "Four.");
    assert_eq!(assembled.usage.input_tokens, Some(12));
}
