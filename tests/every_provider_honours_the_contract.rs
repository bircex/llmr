//! The contract suite, applied to the providers that ship here.
//!
//! A suite only the providers written outside a crate have to pass is a suite nobody inside
//! it is held to. These run against recorded replies rather than live endpoints, so they
//! check the shape of what a provider returns rather than whether a vendor is up.

#![cfg(all(feature = "testkit", feature = "anthropic", feature = "openai"))]

use llmr::http::{HttpRequest, HttpResponse, HttpTransport};
use llmr::providers::anthropic::Anthropic;
use llmr::providers::openai::OpenAiCompatible;
use llmr::registry::{Entry, Registry};
use llmr::testkit::assert_provider_contract;
use llmr::{Reach, Secret};
use std::sync::Arc;

/// Answers everything with the same recorded reply.
struct Always(Vec<u8>);

#[async_trait::async_trait]
impl HttpTransport for Always {
    async fn send(&self, _request: HttpRequest) -> llmr::Result<HttpResponse> {
        Ok(HttpResponse::new(200, self.0.clone()))
    }
}

fn always(body: serde_json::Value) -> Arc<Always> {
    Arc::new(Always(serde_json::to_vec(&body).unwrap_or_default()))
}

/// A registry holding exactly one model, so the suite can check that a model the provider
/// knows and one it does not give different answers.
fn holding(model: &str, reach: Reach) -> Arc<Registry> {
    let toml = format!(
        r#"
provider = "test"
reach = "{reach:?}"

[[model]]
id = "{model}"
context_window = 200000
max_output = 8192
tools = true
source = "a recorded fixture"
verified_at = "2026-08-28"
"#
    );
    Arc::new(Registry::parse(&toml).unwrap_or_else(|e| panic!("the fixture registry: {e}")))
}

#[tokio::test]
async fn the_anthropic_provider_honours_the_contract() {
    let provider = Anthropic::new(
        always(serde_json::json!({
            "model": "claude-sonnet-5",
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": "ok" }],
            "usage": { "input_tokens": 5, "output_tokens": 1 }
        })),
        Secret::new("key", "sk-test"),
        holding("claude-sonnet-5", Reach::FirstPartyApi),
    );

    assert_provider_contract(&provider, "claude-sonnet-5").await;
}

#[tokio::test]
async fn the_openai_shaped_provider_honours_the_contract() {
    let provider = OpenAiCompatible::at(
        "test-endpoint",
        "https://example.invalid/v1",
        always(serde_json::json!({
            "model": "gpt-test",
            "choices": [{
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": "ok" }
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 1 }
        })),
        Secret::new("key", "sk-test"),
        Reach::FirstPartyApi,
        holding("gpt-test", Reach::FirstPartyApi),
    );

    assert_provider_contract(&provider, "gpt-test").await;
}

#[tokio::test]
#[cfg(feature = "cli")]
async fn the_local_command_line_provider_honours_the_contract() {
    // `cat` reads the prompt and prints it back. Not a model, and enough to check the
    // shape: a reply arrives, it says which model served it, and usage comes back absent
    // rather than as zeros.
    use llmr::providers::cli::LocalCli;
    use std::time::Duration;

    let provider = LocalCli::new(
        "cat-as-a-model",
        "cat",
        [] as [&str; 0],
        Duration::from_secs(10),
    )
    .serving(["any-model"]);

    assert_provider_contract(&provider, "any-model").await;
}

#[test]
fn a_registry_entry_can_be_built_by_hand() {
    // Someone writing a provider outside this crate needs to be able to build a table
    // without a TOML file. If this stops compiling, the type has been closed to them.
    let entry = Entry {
        id: "their-model".into(),
        context_window: 8_192,
        max_output: 1_024,
        tools: false,
        structured_output: false,
        prompt_caching: false,
        thinking: false,
        source: "their own documentation".into(),
        verified_at: "2026-08-28".into(),
    };
    assert_eq!(entry.id, "their-model");
}
