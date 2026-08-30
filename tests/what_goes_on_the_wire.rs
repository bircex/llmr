//! What each provider actually sends, and what it makes of what comes back.
//!
//! These run against a recorded transport rather than a server, so what they check is the
//! request that would go out. A test against a live endpoint checks that the endpoint is up.
//!
//! Gated on the providers it exercises, so `cargo test --no-default-features` builds rather
//! than failing on an import of something that was configured out.

#![cfg(all(feature = "anthropic", feature = "openai"))]

use llmr::providers::anthropic;
use llmr::providers::openai;
use llmr::transport::{HttpRequest, HttpResponse, HttpTransport};
use llmr::{
    ChatRequest, ContentBlock, Effort, Error, Message, ModelCapabilities, Provider, Reach,
    Registry, Secret, StopReason, Thinking, ToolSchema, UsageCoverage,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// Keeps what was sent and hands back what the test scripted.
///
/// The mutex is only touched between calls, never across an await inside the provider, so
/// it cannot be the thing that deadlocks. That is a property of the provider under test
/// rather than of this helper.
struct Recorded {
    reply: HttpResponse,
    sent: Mutex<Vec<HttpRequest>>,
}

impl Recorded {
    fn replying(status: u16, body: Value) -> Arc<Self> {
        Arc::new(Self {
            reply: HttpResponse::new(status, serde_json::to_vec(&body).unwrap_or_default()),
            sent: Mutex::new(Vec::new()),
        })
    }

    fn body(&self) -> Value {
        let sent = self.sent.lock().expect("not poisoned");
        let first = sent.first().expect("something was sent");
        serde_json::from_slice(&first.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> Option<String> {
        let sent = self.sent.lock().expect("not poisoned");
        sent.first()?
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    fn url(&self) -> String {
        let sent = self.sent.lock().expect("not poisoned");
        sent.first().map(|r| r.url.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl HttpTransport for Recorded {
    async fn send(&self, request: HttpRequest) -> llmr::Result<HttpResponse> {
        self.sent.lock().expect("not poisoned").push(request);
        Ok(self.reply.clone())
    }
}

// ---- Anthropic -------------------------------------------------------------------------

fn anthropic_reply() -> Value {
    json!({
        "model": "claude-sonnet-5",
        "stop_reason": "end_turn",
        "content": [{ "type": "text", "text": "Hello back." }],
        "usage": {
            "input_tokens": 12,
            "output_tokens": 4,
            "cache_read_input_tokens": 900,
            "cache_creation_input_tokens": 0
        }
    })
}

fn anthropic(transport: Arc<Recorded>) -> anthropic::api::Anthropic {
    anthropic::api::with(
        transport,
        Secret::new("key", "sk-test"),
        Arc::new(Registry::empty("anthropic", Reach::FirstPartyApi)),
    )
}

#[tokio::test]
async fn anthropic_sends_the_key_and_the_api_version() {
    let transport = Recorded::replying(200, anthropic_reply());
    let _ = anthropic(Arc::clone(&transport))
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await;

    assert_eq!(transport.header("x-api-key").as_deref(), Some("sk-test"));
    assert!(transport.header("anthropic-version").is_some());
    assert!(
        transport.url().ends_with("/v1/messages"),
        "{}",
        transport.url()
    );
}

#[tokio::test]
async fn anthropic_always_sends_a_limit_because_the_api_requires_one() {
    let transport = Recorded::replying(200, anthropic_reply());
    let _ = anthropic(Arc::clone(&transport))
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await;

    assert!(
        transport.body()["max_tokens"].is_number(),
        "a request with no limit is one the API refuses"
    );
}

#[tokio::test]
async fn anthropic_keeps_the_signature_on_a_thinking_block() {
    // The property that lets a conversation with reasoning continue. A block replayed
    // without its signature is rejected on arrival.
    let transport = Recorded::replying(200, anthropic_reply());
    let history = vec![
        Message::user("first"),
        Message {
            role: llmr::Role::Assistant,
            content: vec![ContentBlock::Thinking {
                text: "considering".into(),
                signature: Some("sig-abc".into()),
            }],
        },
        Message::user("second"),
    ];

    let _ = anthropic(Arc::clone(&transport))
        .chat(ChatRequest::new("claude-sonnet-5", history))
        .await;

    let sent = transport.body();
    let block = &sent["messages"][1]["content"][0];
    assert_eq!(block["type"], "thinking");
    assert_eq!(block["signature"], "sig-abc");
}

#[tokio::test]
async fn anthropic_puts_a_cache_breakpoint_where_it_was_asked_for() {
    let transport = Recorded::replying(200, anthropic_reply());
    let _ = anthropic(Arc::clone(&transport))
        .chat(
            ChatRequest::new(
                "claude-sonnet-5",
                vec![
                    Message::user("a long preamble"),
                    Message::user("the question"),
                ],
            )
            .with_cache_breakpoint(0),
        )
        .await;

    let sent = transport.body();
    assert_eq!(
        sent["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral",
        "the breakpoint marks the wrong message, and what you are billed for changes"
    );
    assert!(sent["messages"][1]["content"][0]["cache_control"].is_null());
}

#[tokio::test]
async fn anthropic_asks_for_reasoning_with_a_budget_below_the_output_limit() {
    let transport = Recorded::replying(200, anthropic_reply());
    let _ = anthropic(Arc::clone(&transport))
        .chat(
            ChatRequest::new("claude-sonnet-5", vec![Message::user("hi")])
                .with_thinking(Thinking::On(Effort::High))
                .with_max_tokens(1_000),
        )
        .await;

    let sent = transport.body();
    let budget = sent["thinking"]["budget_tokens"].as_u64().unwrap_or(0);
    assert!(
        budget < 1_000,
        "a thinking budget at or above the output limit leaves no room for an answer"
    );
}

#[tokio::test]
async fn anthropic_reports_the_uncached_remainder_without_adjusting_it() {
    let transport = Recorded::replying(200, anthropic_reply());
    let reply = anthropic(transport)
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await
        .expect("a reply");

    // Anthropic already reports the uncached part here, so nothing is subtracted.
    assert_eq!(reply.usage.input_tokens, Some(12));
    assert_eq!(reply.usage.cache_read_tokens, Some(900));
    assert_eq!(reply.usage.prompt_tokens(), Some(912));
    assert_eq!(reply.usage.coverage(), UsageCoverage::Exact);
}

#[tokio::test]
async fn a_reply_with_no_readable_content_is_an_error_not_an_empty_answer() {
    // A 200 with a body this crate cannot read is a failure. Returning an empty message
    // would let a caller carry on with nothing and call it a success.
    let transport = Recorded::replying(200, json!({ "content": [], "stop_reason": "end_turn" }));
    let refused = anthropic(transport)
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await;

    assert!(matches!(refused, Err(Error::Unreadable(_))), "{refused:?}");
}

#[tokio::test]
async fn a_truncated_reply_says_so() {
    let mut body = anthropic_reply();
    body["stop_reason"] = json!("max_tokens");
    let reply = anthropic(Recorded::replying(200, body))
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await
        .expect("a reply");

    assert_eq!(reply.stop_reason, StopReason::MaxTokens);
    assert!(!reply.is_complete(), "a cut off answer arrived with a 200");
}

#[tokio::test]
async fn a_stop_reason_this_crate_has_not_seen_is_kept_as_unknown() {
    // Mapping it to the nearest known reason would report a complete answer for something
    // that may be truncated.
    let mut body = anthropic_reply();
    body["stop_reason"] = json!("some_future_reason");
    let reply = anthropic(Recorded::replying(200, body))
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await
        .expect("a reply");

    assert_eq!(reply.stop_reason, StopReason::Other);
    assert!(!reply.is_complete());
}

#[tokio::test]
async fn a_rejected_key_is_never_reported_as_retryable() {
    let refused = anthropic(Recorded::replying(401, json!({ "error": "bad key" })))
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await;

    let error = refused.expect_err("an error");
    assert!(matches!(error, Error::Auth(_)));
    assert!(
        !error.is_retryable(),
        "retrying a bad key earns a rate limit"
    );
}

// ---- OpenAI shaped ---------------------------------------------------------------------

fn openai_reply() -> Value {
    json!({
        "model": "gpt-test",
        "choices": [{
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": "Hello back." }
        }],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 4,
            "prompt_tokens_details": { "cached_tokens": 900 }
        }
    })
}

fn openai(transport: Arc<Recorded>, reach: Reach) -> openai::api::OpenAiCompatible {
    openai::api::at(
        "test-endpoint",
        "https://example.invalid/v1/",
        transport,
        Secret::new("key", "sk-test"),
        reach,
        Arc::new(Registry::empty("test-endpoint", reach)),
    )
}

#[tokio::test]
async fn the_openai_shape_subtracts_the_cached_part_from_the_prompt() {
    // The one adjustment this provider makes, and the reason two providers are comparable.
    // Here prompt_tokens is the whole prompt including the cached part; this crate's
    // input_tokens is the part that was not cached.
    let reply = openai(
        Recorded::replying(200, openai_reply()),
        Reach::FirstPartyApi,
    )
    .chat(ChatRequest::new("gpt-test", vec![Message::user("hi")]))
    .await
    .expect("a reply");

    assert_eq!(reply.usage.input_tokens, Some(100));
    assert_eq!(reply.usage.cache_read_tokens, Some(900));
    assert_eq!(reply.usage.prompt_tokens(), Some(1_000));
}

#[tokio::test]
async fn a_cache_write_nobody_reported_stays_absent_rather_than_zero() {
    // This protocol has no cache write count. A zero here would price a cache write as
    // free, which is the direction that quietly understates a bill.
    let reply = openai(
        Recorded::replying(200, openai_reply()),
        Reach::FirstPartyApi,
    )
    .chat(ChatRequest::new("gpt-test", vec![Message::user("hi")]))
    .await
    .expect("a reply");

    assert_eq!(reply.usage.cache_write_tokens, None);
    assert_eq!(reply.usage.coverage(), UsageCoverage::Partial);
}

#[tokio::test]
async fn a_base_url_with_a_trailing_slash_reaches_the_same_place() {
    let transport = Recorded::replying(200, openai_reply());
    let _ = openai(Arc::clone(&transport), Reach::FirstPartyApi)
        .chat(ChatRequest::new("gpt-test", vec![Message::user("hi")]))
        .await;

    assert_eq!(
        transport.url(),
        "https://example.invalid/v1/chat/completions"
    );
}

#[tokio::test]
async fn the_system_prompt_becomes_the_first_message() {
    let transport = Recorded::replying(200, openai_reply());
    let _ = openai(Arc::clone(&transport), Reach::FirstPartyApi)
        .chat(
            ChatRequest::new("gpt-test", vec![Message::user("hi")]).with_system("Answer briefly."),
        )
        .await;

    let sent = transport.body();
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(sent["messages"][0]["content"], "Answer briefly.");
}

#[tokio::test]
async fn a_tool_result_becomes_its_own_message() {
    // This protocol carries tool results as top level messages where Anthropic carries them
    // as blocks inside a turn, so one of ours becomes two of theirs.
    let transport = Recorded::replying(200, openai_reply());
    let _ = openai(Arc::clone(&transport), Reach::FirstPartyApi)
        .chat(ChatRequest::new(
            "gpt-test",
            vec![Message {
                role: llmr::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "42".into(),
                    is_error: false,
                }],
            }],
        ))
        .await;

    let sent = transport.body();
    assert_eq!(sent["messages"][0]["role"], "tool");
    assert_eq!(sent["messages"][0]["tool_call_id"], "call-1");
}

#[tokio::test]
async fn a_failed_tool_says_so_in_the_only_place_this_protocol_has() {
    let transport = Recorded::replying(200, openai_reply());
    let _ = openai(Arc::clone(&transport), Reach::FirstPartyApi)
        .chat(ChatRequest::new(
            "gpt-test",
            vec![Message {
                role: llmr::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "no such file".into(),
                    is_error: true,
                }],
            }],
        ))
        .await;

    let sent = transport.body();
    assert!(
        sent["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:"),
        "a model told a tool failed can work around it; one left to infer it cannot"
    );
}

#[tokio::test]
async fn tool_arguments_that_do_not_parse_are_kept_rather_than_dropped() {
    // A model can produce arguments that are not valid JSON. Dropping the call would hide
    // what it tried to do.
    let mut body = openai_reply();
    body["choices"][0]["message"] = json!({
        "role": "assistant",
        "tool_calls": [{
            "id": "call-1",
            "type": "function",
            "function": { "name": "search", "arguments": "{not json" }
        }]
    });

    let reply = openai(Recorded::replying(200, body), Reach::FirstPartyApi)
        .chat(ChatRequest::new("gpt-test", vec![Message::user("hi")]))
        .await
        .expect("a reply");

    match reply.tool_calls().first() {
        Some(ContentBlock::ToolUse { name, input, .. }) => {
            assert_eq!(name, "search");
            assert_eq!(input["raw"], "{not json");
        }
        other => panic!("the call was dropped: {other:?}"),
    }
}

#[tokio::test]
async fn the_reach_is_what_the_caller_said_it_was() {
    // A model on this machine and a hosted API answer the same shape. This provider cannot
    // tell them apart and does not try.
    let local = openai(Recorded::replying(200, openai_reply()), Reach::SelfHosted);
    assert!(local.reach().is_on_device());

    let hosted = openai(
        Recorded::replying(200, openai_reply()),
        Reach::FirstPartyApi,
    );
    assert!(!hosted.reach().is_on_device());
}

#[tokio::test]
async fn a_provider_reports_which_model_actually_served_the_request() {
    // Providers alias names and some fall back under load. Price against this, not against
    // what was asked for.
    let mut body = openai_reply();
    body["model"] = json!("gpt-test-0925");
    let reply = openai(Recorded::replying(200, body), Reach::FirstPartyApi)
        .chat(ChatRequest::new("gpt-test", vec![Message::user("hi")]))
        .await
        .expect("a reply");

    assert_eq!(reply.model.as_str(), "gpt-test-0925");
}

#[tokio::test]
async fn a_tool_schema_reaches_the_wire_in_this_protocols_shape() {
    let transport = Recorded::replying(200, openai_reply());
    let _ = openai(Arc::clone(&transport), Reach::FirstPartyApi)
        .chat(
            ChatRequest::new("gpt-test", vec![Message::user("hi")]).with_tools(vec![
                ToolSchema::new("search", "look it up", json!({ "type": "object" })),
            ]),
        )
        .await;

    let sent = transport.body();
    assert_eq!(sent["tools"][0]["type"], "function");
    assert_eq!(sent["tools"][0]["function"]["name"], "search");
}

// ---- the vocabulary a second consumer asked for ----------------------------------------

#[tokio::test]
async fn no_opinion_about_thinking_sends_nothing_about_it() {
    // The state a bool cannot hold. On a model that reasons by default, sending "off"
    // turns it off and sending nothing leaves it on. A caller who meant neither would get
    // whichever this crate had chosen for them.
    let transport = Recorded::replying(200, anthropic_reply());
    let _ = anthropic(Arc::clone(&transport))
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await;

    assert!(
        transport.body()["thinking"].is_null(),
        "a request with no opinion about reasoning asked for some"
    );
}

#[tokio::test]
async fn the_two_highest_effort_levels_are_reachable() {
    // Vendors expose five levels. With three, the top two are names a caller can write and
    // nothing can act on.
    let mut budgets = Vec::new();
    for effort in [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ] {
        let transport = Recorded::replying(200, anthropic_reply());
        let _ = anthropic(Arc::clone(&transport))
            .chat(
                ChatRequest::new("claude-sonnet-5", vec![Message::user("hi")])
                    .with_thinking(Thinking::On(effort))
                    .with_max_tokens(200_000),
            )
            .await;
        budgets.push(
            transport.body()["thinking"]["budget_tokens"]
                .as_u64()
                .unwrap_or(0),
        );
    }

    let mut sorted = budgets.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        5,
        "two effort levels produced the same budget, so one of them does nothing: {budgets:?}"
    );
}

#[tokio::test]
async fn a_provider_keeps_what_it_was_told_about_why_it_stopped() {
    // Diagnostic text for a person. `StopReason` is what code reads; this is what somebody
    // reads when the stop reason is one this crate does not know.
    let mut body = openai_reply();
    body["choices"][0]["finish_reason"] = json!("content_filter");

    let reply = openai(Recorded::replying(200, body), Reach::FirstPartyApi)
        .chat(ChatRequest::new("gpt-test", vec![Message::user("hi")]))
        .await
        .expect("a reply");

    assert_eq!(reply.stop_reason, StopReason::Refusal);
    assert_eq!(reply.stop_details.as_deref(), Some("content_filter"));
}

#[tokio::test]
async fn a_block_this_crate_does_not_model_survives_the_round_trip() {
    // The first version of the reader dropped anything it did not recognise. A redacted
    // reasoning blob replayed without it is a continuation the provider rejects, and the
    // failure arrives one turn later than the mistake.
    let mut body = anthropic_reply();
    body["content"] = json!([
        { "type": "redacted_thinking", "data": "EroBCkYIBRgCKkBd..." },
        { "type": "text", "text": "Here is the answer." }
    ]);

    let reply = anthropic(Recorded::replying(200, body))
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await
        .expect("a reply");

    let kept = reply
        .message
        .content
        .iter()
        .find(|b| matches!(b, ContentBlock::Opaque { .. }));
    assert!(
        kept.is_some(),
        "the block was dropped: {:?}",
        reply.message.content
    );
    assert_eq!(
        kept.and_then(|b| b.answer_text()),
        None,
        "it is not an answer"
    );

    // And back out again, byte for byte.
    let transport = Recorded::replying(200, anthropic_reply());
    let _ = anthropic(Arc::clone(&transport))
        .chat(ChatRequest::new("claude-sonnet-5", vec![reply.message]))
        .await;

    let sent = transport.body();
    assert_eq!(
        sent["messages"][0]["content"][0]["type"],
        "redacted_thinking"
    );
    assert_eq!(
        sent["messages"][0]["content"][0]["data"], "EroBCkYIBRgCKkBd...",
        "the block was reshaped, so the provider will not recognise it"
    );
}

#[tokio::test]
async fn a_paused_turn_is_reported_as_paused_rather_than_finished() {
    let mut body = anthropic_reply();
    body["stop_reason"] = json!("pause_turn");
    let reply = anthropic(Recorded::replying(200, body))
        .chat(ChatRequest::new(
            "claude-sonnet-5",
            vec![Message::user("hi")],
        ))
        .await
        .expect("a reply");

    assert_eq!(reply.stop_reason, StopReason::PauseTurn);
    assert!(
        !reply.is_complete(),
        "a paused turn read as a finished answer"
    );
}

// ---- Model lists, and the reachability check built on them ------------------------------

fn anthropic_model_list() -> Value {
    // The shape the vendor sends, trimmed to the fields anything here reads.
    json!({
        "data": [
            { "type": "model", "id": "claude-sonnet-5", "display_name": "Claude Sonnet 5" },
            { "type": "model", "id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5" }
        ],
        "has_more": false,
        "first_id": "claude-sonnet-5",
        "last_id": "claude-haiku-4-5"
    })
}

#[tokio::test]
async fn anthropic_asks_for_the_model_list_beside_the_messages_endpoint() {
    let transport = Recorded::replying(200, anthropic_model_list());
    let _ = anthropic(Arc::clone(&transport)).catalogue().await;

    assert!(
        transport.url().ends_with("/v1/models"),
        "{}",
        transport.url()
    );
    // Built from the same base URL as `chat_url`, so neither one grows a second `/v1`.
    assert!(!transport.url().contains("/v1/v1"), "{}", transport.url());
    assert_eq!(transport.header("x-api-key").as_deref(), Some("sk-test"));
}

#[tokio::test]
async fn a_model_list_comes_back_sorted_and_by_id_alone() {
    let transport = Recorded::replying(200, anthropic_model_list());
    let listed = anthropic(transport).catalogue().await.expect("a list");

    assert_eq!(
        listed.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
        vec!["claude-haiku-4-5", "claude-sonnet-5"]
    );
}

#[tokio::test]
async fn a_model_list_with_no_data_array_is_unreadable_rather_than_empty() {
    // An empty list is the vendor saying it serves nothing, which a caller would act on.
    // This is the crate failing to read a reply, and the two must not arrive as one answer.
    let transport = Recorded::replying(200, json!({ "models": [] }));
    let refused = anthropic(transport).catalogue().await;

    assert!(matches!(refused, Err(Error::Unreadable(_))), "{refused:?}");
}

#[tokio::test]
async fn a_model_the_vendor_lists_is_reachable_and_costs_no_call() {
    let transport = Recorded::replying(200, anthropic_model_list());
    let access = anthropic(Arc::clone(&transport))
        .validate(&"claude-sonnet-5".into())
        .await;

    assert!(access.is_ready(), "{access}");
    assert!(
        transport.url().ends_with("/v1/models"),
        "a preflight that reached the messages endpoint would be one that costs money: {}",
        transport.url()
    );
}

#[tokio::test]
async fn a_model_the_vendor_does_not_list_is_denied() {
    let transport = Recorded::replying(200, anthropic_model_list());
    let access = anthropic(transport)
        .validate(&"claude-opus-9000".into())
        .await;

    assert!(access.is_denied(), "{access}");
}

#[tokio::test]
async fn a_key_anthropic_rejects_is_denied_rather_than_unknown() {
    // The variant a caller acts on. Unknown would mean try again later, and this will never
    // clear on its own.
    let transport = Recorded::replying(401, json!({ "error": { "message": "invalid x-api-key" } }));
    let access = anthropic(transport)
        .validate(&"claude-sonnet-5".into())
        .await;

    assert!(access.is_denied(), "{access}");
    assert!(
        access.detail().unwrap_or_default().contains("credential"),
        "{access}"
    );
}

#[tokio::test]
async fn anthropic_being_down_is_unknown_rather_than_denied() {
    // A five minute outage must not read as a configuration problem somebody goes looking
    // for, and must not take the provider out of a router.
    let transport = Recorded::replying(503, json!({ "error": { "message": "overloaded" } }));
    let access = anthropic(transport)
        .validate(&"claude-sonnet-5".into())
        .await;

    assert!(access.is_unknown(), "{access}");
}

#[tokio::test]
async fn an_openai_shaped_endpoint_answers_the_same_three_ways() {
    // The same claims through the other protocol, because a rule that only one provider
    // follows is not a rule this crate has.
    let listing = json!({ "data": [{ "id": "gpt-test" }, { "id": "gpt-other" }] });

    let ready = openai(
        Recorded::replying(200, listing.clone()),
        Reach::FirstPartyApi,
    )
    .validate(&"gpt-test".into())
    .await;
    assert!(ready.is_ready(), "{ready}");

    let denied = openai(Recorded::replying(200, listing), Reach::FirstPartyApi)
        .validate(&"gpt-missing".into())
        .await;
    assert!(denied.is_denied(), "{denied}");

    let unknown = openai(
        Recorded::replying(500, json!({ "error": "boom" })),
        Reach::FirstPartyApi,
    )
    .validate(&"gpt-test".into())
    .await;
    assert!(unknown.is_unknown(), "{unknown}");
}

// ---- Images -----------------------------------------------------------------------------

/// A one pixel PNG, so the bytes on the wire are checkable by hand.
const PIXEL: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

fn with_an_image() -> ChatRequest {
    ChatRequest::new(
        "m",
        vec![Message {
            role: llmr::Role::User,
            content: vec![
                ContentBlock::Text("what is this".into()),
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    source: llmr::ImageSource::Bytes(PIXEL.to_vec()),
                },
            ],
        }],
    )
}

#[tokio::test]
async fn anthropic_sends_an_image_as_base64_with_the_media_type_it_was_given() {
    let transport = Recorded::replying(200, anthropic_reply());
    let _ = anthropic(Arc::clone(&transport))
        .chat(with_an_image())
        .await;

    let blocks = &transport.body()["messages"][0]["content"];
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    // The caller's, not sniffed from the bytes. A provider told the wrong type decodes it
    // wrongly or rejects it, and this crate does not know better than the caller.
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], "iVBORw0KGgo=");
}

#[tokio::test]
async fn the_openai_shape_sends_an_image_as_a_data_url_and_keeps_the_text_beside_it() {
    let transport = Recorded::replying(200, openai_reply());
    let _ = openai(Arc::clone(&transport), Reach::FirstPartyApi)
        .chat(with_an_image())
        .await;

    let content = &transport.body()["messages"][0]["content"];
    assert!(
        content.is_array(),
        "a turn with an image carries parts: {content}"
    );
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is this");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(
        content[1]["image_url"]["url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
}

#[tokio::test]
async fn a_turn_with_no_image_still_carries_plain_text_content() {
    // The smaller endpoints speaking this shape accept a string and nothing else, so the
    // parts array must appear only when there is something that needs it.
    let transport = Recorded::replying(200, openai_reply());
    let _ = openai(Arc::clone(&transport), Reach::FirstPartyApi)
        .chat(ChatRequest::new("m", vec![Message::user("hello")]))
        .await;

    assert_eq!(transport.body()["messages"][0]["content"], "hello");
}

#[test]
fn a_request_carrying_an_image_says_so_before_it_is_sent() {
    // The whole point: found by asking, not by reading a reply that answered a question
    // about an image it never received.
    let needs = with_an_image().needs();
    assert!(needs.images);

    let text_only = ModelCapabilities::none(Reach::LocalCli);
    assert_eq!(needs.unmet_by(&text_only), vec!["images"]);
}
