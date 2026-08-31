//! The Claude Code command line tool.
//!
//! Signs in on this machine and sends every prompt to Anthropic, which is why its reach is
//! [`crate::Reach::LocalCli`] rather than self hosted: the credential is local and the data
//! is not.

use crate::providers::cli::{Envelope, LocalCli, UsageNames};
use std::time::Duration;

/// The program this preset runs.
pub const PROGRAM: &str = "claude";

/// What `claude --output-format json` prints.
///
/// **Checked against a recorded run**, not against the documentation. `claude 2.1.196`, on
/// 2026-08-31, printed an envelope kept in this repository at
/// `tests/recorded/claude-code.json`, and `tests/what_a_command_line_tool_prints.rs` holds
/// this preset to it.
///
/// The answer is at `/result` and the usage at `/usage`, spelled the way Anthropic spells it
/// everywhere else. The recorded run settles the question that matters about those names:
/// `input_tokens` there was 4685 with `cache_creation_input_tokens` at 20208 beside it, so
/// it is the **uncached remainder** and not the whole prompt, which is what
/// [`crate::Usage::input_tokens`] means. Getting that backwards produces a number that looks
/// right and is not.
///
/// `/stop_reason` is read because the recorded run has one. Before that it was not, and
/// every reply from this tool came back [`crate::StopReason::Other`], which is not
/// [complete](crate::StopReason::is_complete): a caller asking whether an answer finished was
/// told "no" for every call it ever made.
///
/// The envelope also carries `total_cost_usd`, which nothing here reads. There is nowhere in
/// [`crate::ChatResponse`] to put a cost a tool worked out itself, and inventing one is a
/// larger decision than a preset.
pub fn envelope() -> Envelope {
    Envelope::at("/result")
        .with_stop_reason("/stop_reason")
        .with_usage("/usage", UsageNames::anthropic())
}

/// A provider that runs the Claude Code tool.
///
/// JSON mode, deliberately. In plain mode the whole document is the answer and the call
/// reports as unmeasured, which every cost report reads as free.
///
/// It knows no models until you name them. A command line tool cannot be asked what it
/// serves, so a provider that answered for every name would turn a typo into a real model.
///
/// [`llmr::Provider::validate`](crate::Provider::validate) probes with `--version`, which
/// establishes that the tool is installed and nothing about the login inside it. That is all
/// this tool answers for free, and a `Ready` from here should be read as no more than that.
///
/// ```no_run
/// use llmr::providers::anthropic::cli;
/// use llmr::{ChatRequest, Message, Provider};
/// use std::time::Duration;
///
/// # async fn example() -> llmr::Result<()> {
/// let claude = cli::provider(Duration::from_secs(300))
///     .serving(["claude-sonnet-5"]);
///
/// let reply = claude
///     .chat(ChatRequest::new("claude-sonnet-5", vec![Message::user("Hello")]))
///     .await?;
/// # Ok(())
/// # }
/// ```
pub fn provider(timeout: Duration) -> LocalCli {
    LocalCli::new(
        "claude-cli",
        PROGRAM,
        ["-p", "--output-format", "json"],
        timeout,
    )
    .reading(envelope())
    .with_model_flag("--model")
    .with_probe(["--version"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preset_does_not_claim_to_know_how_the_account_is_billed() {
        // The same program signed in one way is a flat fee and signed in another is metered
        // against an API key. A preset that guessed would write a metered call down as
        // costing nothing, which is the zero `Usage::absent` exists to prevent.
        use crate::Provider as _;
        assert_eq!(provider(Duration::from_secs(60)).subscription(), None);
        assert_eq!(
            provider(Duration::from_secs(60))
                .billed_by("claude-max")
                .subscription(),
            Some("claude-max")
        );
    }

    #[test]
    fn the_preset_is_probed() {
        // A preset that shipped without one would answer unknown for every user who never
        // read this far, which is the same as not having the method at all.
        //
        // This lives here rather than in `providers::cli` because it is a fact about this
        // preset, not about the runner. The runner's tests are about the runner.
        assert!(provider(Duration::from_secs(60)).probe.is_some());
    }
}
