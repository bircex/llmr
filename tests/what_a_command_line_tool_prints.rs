//! Each command line preset, against what the tool really printed.
//!
//! # Why a recording and not a hand written fixture
//!
//! A preset is four claims about a program somebody else wrote: what to run, where the answer
//! is in the JSON, what the usage fields are called, and what the probe proves. A fixture
//! written here to match the preset proves the preset matches itself.
//!
//! So the envelopes in `tests/recorded/` came off a real tool, and these tests drive the
//! preset through a runner that replays one. What that catches is a pointer into a shape the
//! tool does not have, which is the whole failure mode: the numbers arrive, they look
//! plausible, and every cost report built on them is wrong with nothing able to tell.
//!
//! # What is recorded, and what is still missing
//!
//! | Preset | Recorded | Program |
//! |---|---|---|
//! | `anthropic::cli` | yes, 2026-08-31 | `claude 2.1.196` |
//! | `openai::cli` | **no** | `codex` |
//!
//! A preset with no recording is a preset whose field names nobody has checked. Adding one
//! takes a machine with the tool on it and two commands:
//!
//! ```sh
//! echo "say ok" | codex exec --json
//! codex --version
//! ```
//!
//! Paste the output into `tests/recorded/`, add a case below, and fix whatever it turns out
//! the preset was reading wrongly.

#![cfg(feature = "cli")]

use async_trait::async_trait;
use llmr::providers::cli::{ProcessOutput, ProcessRunner};
use llmr::{ChatRequest, Message, Provider, StopReason, UsageCoverage};
use std::sync::Arc;
use std::time::Duration;

/// Prints a recorded envelope instead of starting a process.
///
/// Everything else about the provider is real: the prompt assembly, the JSON parsing, the
/// pointers, the usage names. Only the program is replaced, because the program is the part
/// that cannot be in a repository.
struct Replaying {
    envelope: &'static str,
}

#[async_trait]
impl ProcessRunner for Replaying {
    async fn run(
        &self,
        _program: &str,
        _args: &[String],
        _stdin: &str,
        _timeout: Duration,
    ) -> llmr::Result<ProcessOutput> {
        Ok(ProcessOutput::new(
            Some(0),
            self.envelope.as_bytes().to_vec(),
        ))
    }
}

fn replaying(envelope: &'static str) -> Arc<Replaying> {
    Arc::new(Replaying { envelope })
}

const CLAUDE_CODE: &str = include_str!("recorded/claude-code.json");

fn ask() -> ChatRequest {
    ChatRequest::new("claude-sonnet-5", vec![Message::user("say ok")])
}

// ---- Claude Code -----------------------------------------------------------------------

#[tokio::test]
async fn the_claude_code_preset_reads_the_answer_a_real_run_printed() {
    let claude = llmr::providers::anthropic::cli::provider(Duration::from_secs(60))
        .with_runner(replaying(CLAUDE_CODE))
        .serving(["claude-sonnet-5"]);

    let reply = claude
        .chat(ask())
        .await
        .unwrap_or_else(|e| panic!("the recorded envelope should read: {e}"));

    assert_eq!(reply.text(), "Ok.");
}

#[tokio::test]
async fn the_claude_code_usage_names_are_the_ones_a_real_run_used() {
    // The claim that cannot be made against anything but a recording, and the one that is
    // worth the most: every cost report this crate produces for this tool rests on it.
    let claude = llmr::providers::anthropic::cli::provider(Duration::from_secs(60))
        .with_runner(replaying(CLAUDE_CODE))
        .serving(["claude-sonnet-5"]);

    let reply = claude.chat(ask()).await.unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(
        reply.usage.coverage(),
        UsageCoverage::Exact,
        "a field this preset reads by name was not there under that name: {:?}",
        reply.usage
    );
    assert_eq!(reply.usage.input_tokens, Some(4_685));
    assert_eq!(reply.usage.cache_read_tokens, Some(0));
    assert_eq!(reply.usage.cache_write_tokens, Some(20_208));
    assert_eq!(reply.usage.output_tokens, Some(6));
}

#[tokio::test]
async fn the_claude_code_input_count_is_the_uncached_remainder_and_not_the_whole_prompt() {
    // The one that produces a number which looks right and is not. `Usage::input_tokens`
    // means the part of the prompt that was not cached, and the recording settles which of
    // the two this tool prints: 4,685 input beside 20,208 written to cache is a remainder.
    // A total would have been 24,893 and would have looked entirely reasonable.
    let claude = llmr::providers::anthropic::cli::provider(Duration::from_secs(60))
        .with_runner(replaying(CLAUDE_CODE))
        .serving(["claude-sonnet-5"]);

    let reply = claude.chat(ask()).await.unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(
        reply.usage.prompt_tokens(),
        Some(24_893),
        "the whole prompt is the remainder plus what was cached, and only one of those is \
         `input_tokens`"
    );
    assert!(reply.usage.input_tokens < reply.usage.prompt_tokens());
}

#[tokio::test]
async fn the_claude_code_preset_reads_the_stop_reason_the_tool_actually_prints() {
    // Before the recording this preset reported `Other` for every reply, on the grounds that
    // a command line tool does not say why it stopped. This one does, and `Other` is not
    // complete, so a caller asking whether an answer finished was told "no" every time.
    let claude = llmr::providers::anthropic::cli::provider(Duration::from_secs(60))
        .with_runner(replaying(CLAUDE_CODE))
        .serving(["claude-sonnet-5"]);

    let reply = claude.chat(ask()).await.unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(reply.stop_reason, StopReason::EndTurn);
    assert!(reply.stop_reason.is_complete());
}

#[tokio::test]
async fn a_tool_that_says_nothing_about_stopping_is_unknown_rather_than_finished() {
    // The other half of the same rule. A preset with no `with_stop_reason`, or a tool whose
    // envelope has no such field, must not report a completed turn it knows nothing about.
    let quiet =
        llmr::providers::cli::LocalCli::new("quiet", "quiet", ["--json"], Duration::from_secs(60))
            .reading(llmr::providers::cli::Envelope::at("/result"))
            .with_runner(replaying(r#"{"result": "hello"}"#))
            .serving(["a-model"]);

    let reply = quiet
        .chat(ChatRequest::new("a-model", vec![Message::user("hi")]))
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(reply.stop_reason, StopReason::Other);
    assert!(!reply.stop_reason.is_complete());
    assert!(
        reply.stop_details.is_some(),
        "and it says why it does not know"
    );
}

#[tokio::test]
async fn a_stop_reason_this_crate_has_not_seen_stays_unknown() {
    // Never mapped to the nearest one. Guessing reports a truncated reply as a finished
    // answer, which is the failure the variant exists to prevent.
    let odd =
        llmr::providers::cli::LocalCli::new("odd", "odd", ["--json"], Duration::from_secs(60))
            .reading(llmr::providers::cli::Envelope::at("/result").with_stop_reason("/stop_reason"))
            .with_runner(replaying(
                r#"{"result": "hello", "stop_reason": "something_new"}"#,
            ))
            .serving(["a-model"]);

    let reply = odd
        .chat(ChatRequest::new("a-model", vec![Message::user("hi")]))
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    assert_eq!(reply.stop_reason, StopReason::Other);
    assert!(!reply.stop_reason.is_complete());
}

// ---- The contract suite, through a scripted runner --------------------------------------

#[cfg(feature = "testkit")]
#[tokio::test]
async fn the_claude_code_preset_honours_the_provider_contract() {
    // The suite every provider written outside this crate is held to, applied to a preset
    // driven by a real recording. Without a runner this cannot run at all, which is why the
    // presets were never in it.
    let claude = llmr::providers::anthropic::cli::provider(Duration::from_secs(60))
        .with_runner(replaying(CLAUDE_CODE))
        .serving(["claude-sonnet-5"]);

    llmr::testkit::assert_provider_contract(&claude, "claude-sonnet-5").await;
}

#[cfg(feature = "testkit")]
#[tokio::test]
async fn the_codex_preset_honours_the_provider_contract() {
    // Held to the same suite, and deliberately **not** against a recording, because there is
    // no recorded run of `codex` in this repository. What this proves is that the preset is
    // well formed. What it does not prove, and what nothing here can, is that
    // `providers::openai::cli::envelope` points at the right fields.
    //
    // That is the open half of the issue this file came from, and it takes a machine with
    // the tool on it rather than more code.
    let codex = llmr::providers::openai::cli::provider(Duration::from_secs(60))
        .with_runner(replaying(
            r#"{"output": "Ok.", "usage": {"input_tokens": 12, "cached_input_tokens": 0, "output_tokens": 3}}"#,
        ))
        .serving(["gpt-5"]);

    llmr::testkit::assert_provider_contract(&codex, "gpt-5").await;
}

/// A program that runs and exits non zero, which is what a tool that is signed out does.
struct Refusing;

#[async_trait]
impl ProcessRunner for Refusing {
    async fn run(
        &self,
        _program: &str,
        _args: &[String],
        _stdin: &str,
        _timeout: Duration,
    ) -> llmr::Result<ProcessOutput> {
        Ok(ProcessOutput::new(Some(1), Vec::new()).with_stderr(b"not logged in".to_vec()))
    }
}

#[tokio::test]
async fn every_shipped_preset_has_a_probe_that_can_say_no() {
    // Two of the four things a preset is made of, checkable without the tool on the machine:
    // it names a program, and it can be asked whether that program works. A preset with no
    // probe answers `Unknown` whatever happens, which is the same as not having `validate`
    // at all, and "not logged in" then reaches nobody until the first real request.
    //
    // Scripted rather than real, deliberately. A test that ran `--version` would pass or
    // fail on what happens to be installed on the machine running it.
    let presets = [
        (
            "claude",
            llmr::providers::anthropic::cli::PROGRAM,
            llmr::providers::anthropic::cli::provider(Duration::from_secs(60)),
        ),
        (
            "codex",
            llmr::providers::openai::cli::PROGRAM,
            llmr::providers::openai::cli::provider(Duration::from_secs(60)),
        ),
    ];

    for (name, program, preset) in presets {
        assert_eq!(
            program, name,
            "the preset should run the tool it is named for"
        );

        let access = preset
            .with_runner(Arc::new(Refusing))
            .validate(&"anything".into())
            .await;
        assert!(
            access.is_denied(),
            "{name}: a probe that exits non zero has to be Denied. Unknown reads as \"ask \
             again later\", so a router keeps the route and nobody is ever told to log in: \
             {access:?}"
        );
    }
}
