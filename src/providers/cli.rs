//! A vendor command line tool, used as a provider.
//!
//! # Why this is a provider and not a client
//!
//! Most gateways treat a command line tool as something you point *at* them, by setting a
//! base URL. This is the other direction: the tool runs here, as a subprocess, using the
//! login it already has, and answers a request like any other provider.
//!
//! That is worth having because it is the one reach where the credential never enters your
//! configuration. Nothing here reads an API key. Whoever ran the tool's own sign in owns
//! the account, and this crate never sees it.
//!
//! # What it cannot do, said out loud
//!
//! A command line tool exposes far less than an API. Tools, structured output and prompt
//! caching are not reachable through one, and most report no token counts at all. Rather
//! than discovering that when a request comes back ignoring half of what you asked,
//! [`Provider::capabilities`] says so before you send anything, and [`crate::Usage`] comes
//! back absent rather than zero.
//!
//! Absent is the important one. A subscription tool has no per call price, and writing zero
//! would turn an unknown cost into a free one in every report that adds it up.

use crate::error::{Error, Result};
use crate::message::{ContentBlock, Message, Role, StopReason};
use crate::model::{ModelCapabilities, ModelId, Reach};
use crate::provider::Provider;
use crate::request::ChatRequest;
use crate::response::ChatResponse;
use crate::usage::Usage;
use async_trait::async_trait;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// A local command line tool that answers prompts.
///
/// Immutable once built. Each call spawns its own process, so there is nothing shared
/// between concurrent calls and nothing to lock.
///
/// ```no_run
/// use llmr::providers::cli::LocalCli;
/// use llmr::{ChatRequest, Message, Provider};
/// use std::time::Duration;
///
/// # async fn example() -> llmr::Result<()> {
/// let claude = LocalCli::new("claude-cli", "claude", ["-p"], Duration::from_secs(300));
/// let reply = claude
///     .chat(ChatRequest::new("claude-sonnet-5", vec![Message::user("Hello")]))
///     .await?;
/// println!("{}", reply.text());
/// # Ok(())
/// # }
/// ```
pub struct LocalCli {
    id: String,
    program: String,
    args: Vec<String>,
    timeout: Duration,
    model_flag: Option<String>,
    serves: std::collections::BTreeSet<String>,
}

impl LocalCli {
    /// A tool that takes a prompt on standard input and writes the reply to standard output.
    ///
    /// The id goes into every record beside the calls it made, so choose something that
    /// says which tool this is.
    pub fn new(
        id: impl Into<String>,
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
        timeout: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout,
            model_flag: None,
            serves: std::collections::BTreeSet::new(),
        }
    }

    /// Names the models this tool can be asked for.
    ///
    /// Without this, [`Provider::capabilities`] answers `None` for everything, which reads
    /// as "this provider does not know" and is the honest answer: a command line tool has
    /// no way to be asked what it serves.
    ///
    /// What it does **not** do is describe them. A model named here answers with everything
    /// switched off, because the limit is the reach rather than the model. The same model
    /// behind an API may well take tools and cache prompts; through a command line tool it
    /// cannot, and saying otherwise would be a capability a caller could not use.
    #[must_use]
    pub fn serving(mut self, models: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.serves = models.into_iter().map(Into::into).collect();
        self
    }

    /// Passes the requested model to the tool through this flag, as `--flag model`.
    ///
    /// Without it the model in a request is ignored, because a tool that has no way to be
    /// told which model to use will run whatever it is configured for. The reply still
    /// reports the model you asked for, and that would be a lie the caller cannot detect,
    /// so leaving this unset is worth doing deliberately.
    #[must_use]
    pub fn with_model_flag(mut self, flag: impl Into<String>) -> Self {
        self.model_flag = Some(flag.into());
        self
    }

    /// The whole conversation as one prompt.
    ///
    /// A command line tool takes text, so the structure has to be flattened. Turns are
    /// labelled so the model can still tell who said what.
    fn prompt(request: &ChatRequest) -> String {
        let mut out = String::new();
        if let Some(system) = &request.system {
            out.push_str(system);
            out.push_str("\n\n");
        }
        for message in &request.messages {
            let who = match message.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            let text = message.text();
            if !text.is_empty() {
                out.push_str(who);
                out.push_str(": ");
                out.push_str(&text);
                out.push_str("\n\n");
            }
        }
        out.trim_end().to_string()
    }
}

#[async_trait]
impl Provider for LocalCli {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        // A model nobody named is unknown, and that is a different answer from a model this
        // tool serves and can do nothing special with. A provider that claimed to know
        // every name would tell a caller that a typo was a real model.
        if !self.serves.contains(model.as_str()) {
            return None;
        }

        // Everything off, and the window left at zero because it is genuinely unknown. A
        // plausible number here would be a guess the caller has no way to identify as one.
        Some(ModelCapabilities::none(Reach::LocalCli))
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.args);
        if let (Some(flag), model) = (&self.model_flag, request.model.as_str()) {
            command.arg(flag).arg(model);
        }

        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Error::Unsupported(format!(
                    "{} is not on the path, so nothing ran. This is not an empty answer",
                    self.program
                )),
                _ => Error::Transient(format!("starting {}: {e}", self.program)),
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            let prompt = Self::prompt(&request);
            stdin
                .write_all(prompt.as_bytes())
                .await
                .map_err(|e| Error::Transient(format!("writing the prompt: {e}")))?;
            // Dropped here so the tool sees end of input. A tool waiting on a pipe that
            // never closes is the deadlock this provider would otherwise have.
            drop(stdin);
        }

        let finished = tokio::time::timeout(self.timeout, child.wait_with_output()).await;

        let output = match finished {
            // `kill_on_drop` means the child is killed when the handle is dropped, which
            // happens as this scope ends. Nothing is left running.
            Err(_) => {
                return Err(Error::Timeout {
                    elapsed: self.timeout,
                })
            }
            Ok(Err(e)) => return Err(Error::Transient(format!("running {}: {e}", self.program))),
            Ok(Ok(output)) => output,
        };

        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Transient(format!(
                "{} exited with {}: {}",
                self.program,
                output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "a signal".into()),
                said.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("nothing")
            )));
        }

        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            return Err(Error::Unreadable(format!(
                "{} exited cleanly and printed nothing",
                self.program
            )));
        }

        Ok(ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text(text)],
            },
            // A command line tool does not say why it stopped. Reported as unknown rather
            // than as a completed turn, because a truncated reply would otherwise look
            // finished.
            stop_reason: StopReason::Other,
            // Not a refusal and not a completed turn. A command line tool does not say, and
            // this records that rather than leaving the reason blank.
            stop_details: Some("a command line tool does not report why it stopped".into()),
            // Nothing was measured. Not zero.
            usage: Usage::absent(),
            model: request.model.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ChatRequest {
        ChatRequest::new(
            "any-model",
            vec![Message::user("what is 2 + 2"), Message::assistant("4")],
        )
        .with_system("Answer briefly.")
    }

    #[test]
    fn the_prompt_says_who_said_what() {
        let prompt = LocalCli::prompt(&request());
        assert!(prompt.starts_with("Answer briefly."));
        assert!(prompt.contains("User: what is 2 + 2"));
        assert!(prompt.contains("Assistant: 4"));
    }

    #[test]
    fn a_tool_that_was_not_told_what_it_serves_knows_nothing() {
        // The honest answer. A command line tool cannot be asked what it serves, so a
        // provider that answered for every name would turn a typo into a real model.
        let cli = LocalCli::new("t", "true", [] as [&str; 0], Duration::from_secs(1));
        assert_eq!(cli.capabilities(&"anything".into()), None);
    }

    #[test]
    fn a_named_model_answers_with_everything_off() {
        // Named means known. It does not mean capable: the limit here is the reach, not the
        // model, and the same model behind an API may well take tools.
        let cli = LocalCli::new("t", "true", [] as [&str; 0], Duration::from_secs(1))
            .serving(["claude-sonnet-5"]);

        let caps = cli.capabilities(&"claude-sonnet-5".into());
        assert_eq!(caps.map(|c| c.tools), Some(false));
        assert_eq!(caps.map(|c| c.prompt_caching), Some(false));
        assert_eq!(caps.map(|c| c.reach), Some(Reach::LocalCli));

        assert_eq!(cli.capabilities(&"a-typo".into()), None);
    }

    #[tokio::test]
    async fn a_missing_tool_is_an_error_rather_than_an_empty_answer() {
        let cli = LocalCli::new(
            "missing",
            "llmr-definitely-not-a-real-program",
            [] as [&str; 0],
            Duration::from_secs(5),
        );
        let refused = cli.chat(request()).await;
        let message = refused.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("not on the path"), "{message}");
        assert!(message.contains("not an empty answer"), "{message}");
    }

    #[tokio::test]
    async fn a_tool_that_prints_nothing_is_unreadable_rather_than_silent_success() {
        let cli = LocalCli::new("quiet", "true", [] as [&str; 0], Duration::from_secs(5));
        let refused = cli.chat(request()).await;
        assert!(matches!(refused, Err(Error::Unreadable(_))), "{refused:?}");
    }

    #[tokio::test]
    async fn a_reply_reports_no_usage_rather_than_zero() {
        let cli = LocalCli::new("echo", "cat", [] as [&str; 0], Duration::from_secs(5));
        let reply = cli.chat(request()).await;
        let usage = reply.map(|r| r.usage).unwrap_or_default();
        assert_eq!(usage.coverage(), crate::UsageCoverage::Absent);
    }
}
