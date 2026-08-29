//! The contract suite, applied to the providers that ship here.
//!
//! A suite only the providers written outside a crate have to pass is a suite nobody inside
//! it is held to. These run against recorded replies rather than live endpoints, so they
//! check the shape of what a provider returns rather than whether a vendor is up.

#![cfg(all(feature = "testkit", feature = "anthropic", feature = "openai"))]

use llmr::providers::api::anthropic;
use llmr::providers::api::openai;
use llmr::registry::{Entry, Registry};
use llmr::testkit::{assert_a_bad_credential_is_denied, assert_provider_contract};
use llmr::transport::{HttpRequest, HttpResponse, HttpTransport};
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

/// Answers every request the way a vendor answers a key it does not accept.
///
/// The suite cannot build this for itself, which is why the bad credential check is a second
/// entry point rather than part of the main one.
struct Rejects;

#[async_trait::async_trait]
impl HttpTransport for Rejects {
    async fn send(&self, _request: HttpRequest) -> llmr::Result<HttpResponse> {
        Ok(HttpResponse::new(
            401,
            br#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.to_vec(),
        ))
    }
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
    let provider = anthropic::with(
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
async fn the_anthropic_provider_denies_a_key_the_vendor_rejects() {
    let provider = anthropic::with(
        Arc::new(Rejects),
        Secret::new("key", "sk-not-a-real-key"),
        holding("claude-sonnet-5", Reach::FirstPartyApi),
    );

    assert_a_bad_credential_is_denied(&provider, "claude-sonnet-5").await;
}

#[tokio::test]
async fn the_openai_shaped_provider_honours_the_contract() {
    let provider = openai::at(
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
async fn the_openai_shaped_provider_denies_a_key_the_endpoint_rejects() {
    let provider = openai::at(
        "test-endpoint",
        "https://example.invalid/v1",
        Arc::new(Rejects),
        Secret::new("key", "sk-not-a-real-key"),
        Reach::FirstPartyApi,
        holding("gpt-test", Reach::FirstPartyApi),
    );

    assert_a_bad_credential_is_denied(&provider, "gpt-test").await;
}

#[tokio::test]
#[cfg(feature = "cli")]
async fn the_local_command_line_provider_honours_the_contract() {
    // Through a scripted runner rather than a real command. `cat` would work here and not
    // on Windows, and a suite that fails on one platform for a reason unrelated to the code
    // is a suite people learn to ignore.
    use llmr::providers::cli::{LocalCli, ProcessOutput, ProcessRunner};
    use std::time::Duration;

    struct Answers;

    #[async_trait::async_trait]
    impl ProcessRunner for Answers {
        async fn run(
            &self,
            _program: &str,
            _args: &[String],
            _stdin: &str,
            _timeout: Duration,
        ) -> llmr::Result<ProcessOutput> {
            Ok(ProcessOutput::new(Some(0), b"ok".to_vec()))
        }
    }

    let provider = LocalCli::new(
        "scripted-cli",
        "a-tool",
        [] as [&str; 0],
        Duration::from_secs(10),
    )
    .with_runner(Arc::new(Answers))
    .serving(["any-model"]);

    assert_provider_contract(&provider, "any-model").await;
}

#[tokio::test]
#[cfg(feature = "cli")]
async fn the_local_command_line_provider_denies_a_tool_that_is_signed_out() {
    // The command line equivalent of a rejected key. The tool is installed, it runs, and it
    // exits saying the login is no good, which is exactly the failure that otherwise waits
    // until somebody is sitting in front of a request.
    use llmr::providers::cli::{LocalCli, ProcessOutput, ProcessRunner};
    use std::time::Duration;

    struct SignedOut;

    #[async_trait::async_trait]
    impl ProcessRunner for SignedOut {
        async fn run(
            &self,
            _program: &str,
            _args: &[String],
            _stdin: &str,
            _timeout: Duration,
        ) -> llmr::Result<ProcessOutput> {
            Ok(ProcessOutput::new(Some(1), Vec::new())
                .with_stderr(b"not logged in: run `a-tool login`\n".to_vec()))
        }
    }

    let provider = LocalCli::new(
        "scripted-cli",
        "a-tool",
        [] as [&str; 0],
        Duration::from_secs(10),
    )
    .with_runner(Arc::new(SignedOut))
    .with_probe(["--version"])
    .serving(["any-model"]);

    assert_a_bad_credential_is_denied(&provider, "any-model").await;
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
