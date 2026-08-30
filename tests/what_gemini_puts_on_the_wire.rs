//! What the Gemini protocol sends, and what it makes of what comes back.
//!
//! Its parts, its roles and its usage fields line up with neither of the other two, and each
//! difference is a chance to lose something quietly. These are the ones worth naming.

#![cfg(feature = "gemini")]

use llmr::providers::gemini;
use llmr::transport::{HttpRequest, HttpResponse, HttpTransport};
use llmr::{
    ChatRequest, ContentBlock, Effort, Message, Provider, Reach, Registry, Secret, StopReason,
    Thinking, ToolSchema, UsageCoverage,
};
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

    fn header(&self, name: &str) -> Option<String> {
        let sent = self.sent.lock().expect("not poisoned");
        sent.first()?
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }
}

#[async_trait::async_trait]
impl HttpTransport for Recorded {
    async fn send(&self, request: HttpRequest) -> llmr::Result<HttpResponse> {
        self.sent.lock().expect("not poisoned").push(request);
        Ok(self.reply.clone())
    }
}

fn provider(transport: Arc<Recorded>) -> gemini::api::Gemini {
    gemini::api::with(
        transport,
        Secret::new("key", "AIza-test"),
        Arc::new(Registry::empty("gemini", Reach::FirstPartyApi)),
    )
}

fn request() -> ChatRequest {
    ChatRequest::new("gemini-3-pro", vec![Message::user("two plus two")])
}

fn reply() -> Value {
    json!({
        "modelVersion": "gemini-3-pro-001",
        "candidates": [{
            "finishReason": "STOP",
            "content": { "role": "model", "parts": [{ "text": "Four." }] },
        }],
        "usageMetadata": {
            "promptTokenCount": 912,
            "cachedContentTokenCount": 900,
            "candidatesTokenCount": 4,
            "thoughtsTokenCount": 11,
        },
    })
}

#[tokio::test]
async fn the_model_goes_in_the_path_and_the_key_in_a_header() {
    let transport = Recorded::replying(reply());
    let _ = provider(Arc::clone(&transport)).chat(request()).await;

    // This API cannot be addressed without the model in the URL, which is why `chat_url`
    // takes one at all.
    assert!(
        transport
            .url()
            .ends_with("/models/gemini-3-pro:generateContent"),
        "{}",
        transport.url()
    );
    assert_eq!(
        transport.header("x-goog-api-key").as_deref(),
        Some("AIza-test")
    );
    // In the path, so not repeated in the body where the two could disagree.
    assert!(
        transport.body().get("model").is_none(),
        "{}",
        transport.body()
    );
}

#[tokio::test]
async fn the_assistant_is_called_model_here() {
    // A turn sent with the role `assistant` is rejected outright by this API.
    let transport = Recorded::replying(reply());
    let conversation = ChatRequest::new(
        "gemini-3-pro",
        vec![
            Message::user("hello"),
            Message {
                role: llmr::Role::Assistant,
                content: vec![ContentBlock::Text("hi".into())],
            },
        ],
    );
    let _ = provider(Arc::clone(&transport)).chat(conversation).await;

    let contents = &transport.body()["contents"];
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[1]["role"], "model");
}

#[tokio::test]
async fn the_system_prompt_is_its_own_field_rather_than_a_turn() {
    let transport = Recorded::replying(reply());
    let _ = provider(Arc::clone(&transport))
        .chat(request().with_system("be brief"))
        .await;

    assert_eq!(
        transport.body()["systemInstruction"]["parts"][0]["text"],
        "be brief"
    );
    assert_eq!(
        transport.body()["contents"].as_array().map(Vec::len),
        Some(1)
    );
}

#[tokio::test]
async fn reasoning_is_asked_for_rather_than_paid_for_and_hidden() {
    // Without `includeThoughts` the reasoning is billed and not returned, which is the worst
    // of both: you pay for it and cannot see what the model was doing when it went wrong.
    let transport = Recorded::replying(reply());
    let _ = provider(Arc::clone(&transport))
        .chat(request().with_thinking(Thinking::On(Effort::High)))
        .await;

    let config = &transport.body()["generationConfig"]["thinkingConfig"];
    assert_eq!(config["includeThoughts"], true);
    assert!(config["thinkingBudget"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn no_opinion_about_thinking_sends_nothing_about_it() {
    // Distinct from asking for none. On some models reasoning is on by default, so saying
    // nothing and saying zero are different requests.
    let transport = Recorded::replying(reply());
    let _ = provider(Arc::clone(&transport)).chat(request()).await;
    assert!(transport.body()["generationConfig"]
        .get("thinkingConfig")
        .is_none());

    let off = Recorded::replying(reply());
    let _ = provider(Arc::clone(&off))
        .chat(request().with_thinking(Thinking::Off))
        .await;
    assert_eq!(
        off.body()["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        0
    );
}

#[tokio::test]
async fn a_thought_part_is_reasoning_rather_than_the_answer() {
    // The failure this split exists to prevent: the model's private working out read as
    // text and shown to somebody as though it had been said.
    let transport = Recorded::replying(json!({
        "modelVersion": "gemini-3-pro-001",
        "candidates": [{
            "finishReason": "STOP",
            "content": { "role": "model", "parts": [
                { "text": "two and two make four", "thought": true },
                { "text": "Four." },
            ]},
        }],
    }));

    let answer = provider(transport).chat(request()).await.expect("a reply");
    assert_eq!(
        answer.text(),
        "Four.",
        "reasoning must not reach the answer"
    );
    assert_eq!(
        answer.message.content[0],
        ContentBlock::Thinking {
            text: "two and two make four".into(),
            // No signature in this API, so there is nothing here that breaks a later turn.
            signature: None,
        }
    );
}

#[tokio::test]
async fn the_cached_part_is_subtracted_and_thinking_tokens_are_billed() {
    let answer = provider(Recorded::replying(reply()))
        .chat(request())
        .await
        .expect("a reply");

    assert_eq!(
        answer.usage.input_tokens,
        Some(12),
        "912 total less 900 cached"
    );
    assert_eq!(answer.usage.cache_read_tokens, Some(900));
    // 4 shown plus 11 thought. Reporting only the visible half underpays every reasoning
    // call, and the tokens are billed either way.
    assert_eq!(answer.usage.output_tokens, Some(15));
    // No cache write count in this API. Absent, not nought.
    assert_eq!(answer.usage.cache_write_tokens, None);
    assert_eq!(answer.usage.coverage(), UsageCoverage::Partial);
    assert_eq!(answer.model.as_str(), "gemini-3-pro-001");
}

#[tokio::test]
async fn a_tool_call_survives_the_round_trip() {
    let transport = Recorded::replying(json!({
        "candidates": [{
            "finishReason": "STOP",
            "content": { "role": "model", "parts": [
                { "functionCall": { "name": "search", "args": { "q": "rust" } } },
            ]},
        }],
    }));

    let with_tools = request().with_tools(vec![ToolSchema::new(
        "search",
        "search the web",
        json!({ "type": "object" }),
    )]);
    let answer = provider(Arc::clone(&transport))
        .chat(with_tools)
        .await
        .expect("a reply");

    assert_eq!(
        transport.body()["tools"][0]["functionDeclarations"][0]["name"],
        "search"
    );
    match &answer.message.content[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(name, "search");
            assert_eq!(input["q"], "rust");
            // No id in the reply, so the name stands in. A result has to be routable.
            assert_eq!(id, "search");
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
}

#[tokio::test]
async fn a_refusal_is_a_refusal_rather_than_a_finished_answer() {
    let transport = Recorded::replying(json!({
        "candidates": [{
            "finishReason": "SAFETY",
            "content": { "role": "model", "parts": [{ "text": "I cannot help with that." }] },
        }],
    }));

    let answer = provider(transport).chat(request()).await.expect("a reply");
    assert_eq!(answer.stop_reason, StopReason::Refusal);
    assert!(!answer.is_complete(), "a refusal is not a finished answer");
}

#[tokio::test]
async fn a_reply_with_nothing_readable_is_an_error_not_an_empty_answer() {
    let transport = Recorded::replying(json!({ "candidates": [] }));
    let refused = provider(transport).chat(request()).await;
    assert!(
        matches!(refused, Err(llmr::Error::Unreadable(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn the_catalogue_strips_the_path_a_caller_never_types() {
    let transport = Recorded::replying(json!({
        "models": [
            { "name": "models/gemini-3-pro" },
            { "name": "models/gemini-3-flash" },
        ],
    }));

    let listed = provider(transport).catalogue().await.expect("a list");
    assert_eq!(
        listed.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
        vec!["gemini-3-flash", "gemini-3-pro"]
    );
}
