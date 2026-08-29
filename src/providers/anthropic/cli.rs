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
/// The answer is at `/result` and the usage at `/usage`, spelled the way Anthropic spells it
/// everywhere else.
pub fn envelope() -> Envelope {
    Envelope::at("/result").with_usage("/usage", UsageNames::anthropic())
}

/// A provider that runs the Claude Code tool.
///
/// JSON mode, deliberately. In plain mode the whole document is the answer and the call
/// reports as unmeasured, which every cost report reads as free.
///
/// It knows no models until you name them. A command line tool cannot be asked what it
/// serves, so a provider that answered for every name would turn a typo into a real model.
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
}
