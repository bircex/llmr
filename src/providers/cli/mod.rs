//! Vendor command line tools, used as providers.
//!
//! # One runner, many tools
//!
//! [`LocalCli`] does everything a subprocess needs: spawn, write the prompt, close stdin,
//! wait with a deadline, kill on drop, read stdout, tell a missing binary from a silent one.
//! A vendor supplies only what differs, which is the program name, its arguments, and the
//! shape of what it prints.
//!
//! Those live in one module each, beside this one. Adding a tool is a preset, not a client.
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

pub mod claude;
pub mod codex;

use crate::chat::message::{ContentBlock, Message, Role, StopReason};
use crate::chat::request::ChatRequest;
use crate::chat::response::ChatResponse;
use crate::cost::usage::Usage;
use crate::error::{Error, Result};
use crate::model::{ModelCapabilities, ModelId, Reach};
use crate::provider::Provider;
use async_trait::async_trait;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// What a finished process left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessOutput {
    /// `None` when the process was killed by a signal rather than exiting.
    pub exit_code: Option<i32>,
    /// What it printed.
    pub stdout: Vec<u8>,
    /// What it complained about.
    pub stderr: Vec<u8>,
}

impl ProcessOutput {
    /// A result with nothing on standard error.
    pub fn new(exit_code: Option<i32>, stdout: Vec<u8>) -> Self {
        Self {
            exit_code,
            stdout,
            stderr: Vec::new(),
        }
    }

    /// Records what it wrote to standard error.
    #[must_use]
    pub fn with_stderr(mut self, stderr: Vec<u8>) -> Self {
        self.stderr = stderr;
        self
    }
}

/// The one thing in this provider that starts a process.
///
/// A trait for the same reason the HTTP provider has one: everything above it, the argument
/// building, the prompt assembly and the failure classification, is then testable without
/// the tool being installed, without a login and without spend.
///
/// It is also what makes this provider testable on a machine that has no `cat`. The first
/// version of these tests ran real commands and would have failed on Windows for a reason
/// that has nothing to do with the code under test.
#[async_trait]
pub trait ProcessRunner: Send + Sync {
    /// Runs a program with a prompt on standard input and waits for it.
    ///
    /// # Errors
    ///
    /// A failure to start or to wait. A process that ran and exited non zero is `Ok`, with
    /// the code in [`ProcessOutput::exit_code`], because reading that is the caller's job.
    async fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: &str,
        timeout: Duration,
    ) -> Result<ProcessOutput>;
}

/// Runs a real process.
pub struct Spawning;

#[async_trait]
impl ProcessRunner for Spawning {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: &str,
        timeout: Duration,
    ) -> Result<ProcessOutput> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Error::Unsupported(format!(
                    "{program} is not on the path, so nothing ran. This is not an empty answer"
                )),
                _ => Error::Transient(format!("starting {program}: {e}")),
            })?;

        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(stdin.as_bytes())
                .await
                .map_err(|e| Error::Transient(format!("writing the prompt: {e}")))?;
            // Dropped here so the tool sees end of input. A tool waiting on a pipe that
            // never closes is the deadlock this provider would otherwise have.
            drop(pipe);
        }

        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            // `kill_on_drop` means the child is killed as this scope ends. Nothing is left
            // running.
            Err(_) => Err(Error::Timeout { elapsed: timeout }),
            Ok(Err(e)) => Err(Error::Transient(format!("running {program}: {e}"))),
            Ok(Ok(output)) => Ok(ProcessOutput {
                exit_code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            }),
        }
    }
}

/// How to read a JSON envelope, for a tool that prints one instead of plain text.
///
/// Most vendor tools have a mode that reports structured output, and it carries more than
/// the answer. Reading it is what makes the difference between a provider that reports no
/// usage because the tool does not measure, and one that reports none because nobody
/// looked.
///
/// The fields are JSON pointers, so a nested envelope is `"/data/result"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    answer: String,
    usage: Option<String>,
    names: UsageNames,
}

/// What a tool calls each usage field inside its envelope.
///
/// Spelled out rather than guessed. A provider that tried several likely names would read
/// the wrong number the first time two of them appeared together, and would do it silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageNames {
    /// Prompt tokens not served from cache.
    pub input: String,
    /// Prompt tokens served from cache.
    pub cache_read: String,
    /// Prompt tokens written to cache.
    pub cache_write: String,
    /// Tokens produced.
    pub output: String,
}

impl UsageNames {
    /// The spelling Anthropic uses, in its API and in its command line tool.
    pub fn anthropic() -> Self {
        Self {
            input: "input_tokens".into(),
            cache_read: "cache_read_input_tokens".into(),
            cache_write: "cache_creation_input_tokens".into(),
            output: "output_tokens".into(),
        }
    }
}

impl Envelope {
    /// An envelope whose answer is at this pointer and which reports no usage.
    pub fn at(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
            usage: None,
            names: UsageNames::anthropic(),
        }
    }

    /// Reads usage from an object at this pointer, under these names.
    #[must_use]
    pub fn with_usage(mut self, pointer: impl Into<String>, names: UsageNames) -> Self {
        self.usage = Some(pointer.into());
        self.names = names;
        self
    }
}

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
    runner: Arc<dyn ProcessRunner>,
    envelope: Option<Envelope>,
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
            runner: Arc::new(Spawning),
            envelope: None,
        }
    }

    /// Reads the tool's output as a JSON envelope rather than as the answer itself.
    ///
    /// Without this, everything the tool printed is the answer, which is right for a tool
    /// in plain mode and wrong for one in JSON mode: the caller would be handed a document
    /// where they expected a sentence.
    #[must_use]
    pub fn reading(mut self, envelope: Envelope) -> Self {
        self.envelope = Some(envelope);
        self
    }

    /// Runs through this instead of starting a real process.
    ///
    /// For tests, and for anything that needs to run the tool somewhere other than here.
    #[must_use]
    pub fn with_runner(mut self, runner: Arc<dyn ProcessRunner>) -> Self {
        self.runner = runner;
        self
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
        // Refused rather than dropped.
        //
        // `capabilities` already says this reach carries none of these, and a provider that
        // took the request anyway would prove the capability list is decoration. The reply
        // would arrive looking normal, having ignored half of what was asked, and the
        // caller would be billed for it.
        let unmet = request
            .needs()
            .unmet_by(&ModelCapabilities::none(Reach::LocalCli));
        if !unmet.is_empty() {
            return Err(Error::Unsupported(format!(
                "{} cannot carry {} through a command line tool. `capabilities` says so \
                 before you send; this is the same answer at the point of sending, rather \
                 than a reply that quietly did without them",
                self.id,
                unmet.join(" or ")
            )));
        }

        let mut args = self.args.clone();
        if let Some(flag) = &self.model_flag {
            args.push(flag.clone());
            args.push(request.model.0.clone());
        }

        let output = self
            .runner
            .run(&self.program, &args, &Self::prompt(&request), self.timeout)
            .await?;

        if output.exit_code != Some(0) {
            let said = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Transient(format!(
                "{} exited with {}: {}",
                self.program,
                output
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "a signal".into()),
                said.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("nothing")
            )));
        }

        let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if printed.is_empty() {
            return Err(Error::Unreadable(format!(
                "{} exited cleanly and printed nothing",
                self.program
            )));
        }

        let (text, usage) = match &self.envelope {
            None => (printed, Usage::absent()),
            Some(envelope) => {
                let parsed: serde_json::Value = serde_json::from_str(&printed).map_err(|e| {
                    Error::Unreadable(format!(
                        "{} was configured to print JSON and printed something else: {e}",
                        self.program
                    ))
                })?;

                let answer = parsed
                    .pointer(&envelope.answer)
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        Error::Unreadable(format!(
                            "{} printed an envelope with no answer at {}",
                            self.program, envelope.answer
                        ))
                    })?
                    .trim()
                    .to_string();

                // Absent when the envelope carries none. Read when it does: a tool that
                // measures and is not read reports as unmeasured, which is a cost report
                // that says free.
                let usage = match envelope.usage.as_deref().and_then(|p| parsed.pointer(p)) {
                    None => Usage::absent(),
                    Some(reported) => {
                        let field =
                            |name: &str| reported.get(name).and_then(serde_json::Value::as_u64);
                        let mut usage = Usage::absent();
                        if let Some(n) = field(&envelope.names.input) {
                            usage = usage.with_input(n);
                        }
                        if let Some(n) = field(&envelope.names.cache_read) {
                            usage = usage.with_cache_read(n);
                        }
                        if let Some(n) = field(&envelope.names.cache_write) {
                            usage = usage.with_cache_write(n);
                        }
                        if let Some(n) = field(&envelope.names.output) {
                            usage = usage.with_output(n);
                        }
                        usage
                    }
                };

                (answer, usage)
            }
        };

        if text.is_empty() {
            return Err(Error::Unreadable(format!(
                "{} printed an envelope whose answer was empty",
                self.program
            )));
        }

        Ok(ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text(text)],
            },
            // A command line tool does not say why it stopped. Reported as unknown rather
            // than as a completed turn, because a truncated reply would otherwise look
            // finished.
            StopReason::Other,
            usage,
            request.model.clone(),
        )
        .with_stop_details("a command line tool does not report why it stopped"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool that answers whatever the test says, without a process anywhere.
    ///
    /// The first version of these tests ran `cat` and `true`. They passed here and would
    /// have failed on Windows, for a reason that has nothing to do with the code they were
    /// checking.
    struct Scripted(std::sync::Mutex<Result<ProcessOutput>>);

    impl Scripted {
        fn new(output: Result<ProcessOutput>) -> Arc<Self> {
            Arc::new(Self(std::sync::Mutex::new(output)))
        }
    }

    #[async_trait]
    impl ProcessRunner for Scripted {
        async fn run(
            &self,
            _program: &str,
            _args: &[String],
            _stdin: &str,
            _timeout: Duration,
        ) -> Result<ProcessOutput> {
            match &*self
                .0
                .lock()
                .map_err(|_| Error::Transient("poisoned".into()))?
            {
                Ok(output) => Ok(output.clone()),
                Err(e) => Err(Error::Transient(e.to_string())),
            }
        }
    }

    /// Records what it was asked to run, so a test can assert on the command line.
    #[derive(Default)]
    struct Recording(std::sync::Mutex<Vec<(String, Vec<String>, String)>>);

    #[async_trait]
    impl ProcessRunner for Recording {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            stdin: &str,
            _timeout: Duration,
        ) -> Result<ProcessOutput> {
            if let Ok(mut seen) = self.0.lock() {
                seen.push((program.to_string(), args.to_vec(), stdin.to_string()));
            }
            Ok(ProcessOutput::new(Some(0), b"an answer".to_vec()))
        }
    }

    fn cli(runner: Arc<dyn ProcessRunner>) -> LocalCli {
        LocalCli::new(
            "test-cli",
            "a-tool",
            [] as [&str; 0],
            Duration::from_secs(5),
        )
        .with_runner(runner)
    }

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
        assert_eq!(
            cli(Arc::new(Recording::default())).capabilities(&"anything".into()),
            None
        );
    }

    #[test]
    fn a_named_model_answers_with_everything_off() {
        // Named means known. It does not mean capable: the limit here is the reach, not the
        // model, and the same model behind an API may well take tools.
        let cli = cli(Arc::new(Recording::default())).serving(["claude-sonnet-5"]);

        let caps = cli.capabilities(&"claude-sonnet-5".into());
        assert_eq!(caps.map(|c| c.tools), Some(false));
        assert_eq!(caps.map(|c| c.prompt_caching), Some(false));
        assert_eq!(caps.map(|c| c.reach), Some(Reach::LocalCli));

        assert_eq!(cli.capabilities(&"a-typo".into()), None);
    }

    #[tokio::test]
    async fn the_model_reaches_the_command_line_when_a_flag_was_named() {
        // Without a flag the model in a request is ignored, and the reply still reports the
        // model that was asked for. That is a lie a caller cannot detect, so the flag is
        // worth checking.
        let runner = Arc::new(Recording::default());
        let cli = cli(runner.clone()).with_model_flag("--model");
        let _ = cli.chat(request()).await;

        let seen = runner.0.lock().map(|s| s.clone()).unwrap_or_default();
        let (program, args, stdin) = seen.first().cloned().unwrap_or_default();
        assert_eq!(program, "a-tool");
        assert_eq!(args, vec!["--model".to_string(), "any-model".to_string()]);
        assert!(stdin.contains("what is 2 + 2"));
    }

    #[tokio::test]
    async fn a_missing_tool_is_an_error_rather_than_an_empty_answer() {
        let cli = cli(Scripted::new(Err(Error::Unsupported(
            "a-tool is not on the path, so nothing ran. This is not an empty answer".into(),
        ))));
        let message = cli
            .chat(request())
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(message.contains("not an empty answer"), "{message}");
    }

    #[tokio::test]
    async fn a_tool_that_prints_nothing_is_unreadable_rather_than_silent_success() {
        let cli = cli(Scripted::new(Ok(ProcessOutput::new(Some(0), Vec::new()))));
        let refused = cli.chat(request()).await;
        assert!(matches!(refused, Err(Error::Unreadable(_))), "{refused:?}");
    }

    #[tokio::test]
    async fn a_tool_that_failed_says_what_it_complained_about() {
        let cli = cli(Scripted::new(Ok(
            ProcessOutput::new(Some(2), Vec::new()).with_stderr(b"not logged in\n".to_vec())
        )));
        let message = cli
            .chat(request())
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(message.contains("not logged in"), "{message}");
    }

    #[tokio::test]
    async fn a_request_this_reach_cannot_carry_is_refused_rather_than_stripped() {
        // The failure the whole capability list exists to prevent, arriving through the
        // provider that has the fewest capabilities of any.
        let cli = cli(Scripted::new(Ok(ProcessOutput::new(
            Some(0),
            b"an answer".to_vec(),
        ))));

        let with_tools = request().with_tools(vec![crate::ToolSchema {
            name: "read_file".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({ "type": "object" }),
        }]);

        let refused = cli.chat(with_tools).await;
        let message = refused.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("tools"), "{message}");
        assert!(message.contains("quietly did without"), "{message}");
    }

    #[tokio::test]
    async fn a_plain_request_still_goes_through() {
        // The other half. A refusal that fired on everything would make this provider
        // useless rather than honest.
        let cli = cli(Scripted::new(Ok(ProcessOutput::new(
            Some(0),
            b"an answer".to_vec(),
        ))));
        assert!(cli.chat(request()).await.is_ok());
    }

    fn an_envelope(text: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": "result",
            "result": text,
            "usage": {
                "input_tokens": 10,
                "cache_creation_input_tokens": 26_193,
                "cache_read_input_tokens": 0,
                "output_tokens": 7
            }
        }))
        .unwrap_or_default()
    }

    #[tokio::test]
    async fn an_envelope_yields_the_answer_rather_than_the_document() {
        // Without this the caller is handed a JSON document where a sentence was expected,
        // and everything downstream treats it as prose.
        let cli = cli(Scripted::new(Ok(ProcessOutput::new(
            Some(0),
            an_envelope("Four."),
        ))))
        .reading(claude::envelope());

        let reply = cli.chat(request()).await.expect("a reply");
        assert_eq!(reply.text(), "Four.");
    }

    #[tokio::test]
    async fn usage_the_tool_reported_is_read_rather_than_thrown_away() {
        // A tool that measures and is not read reports as unmeasured, and a cost report
        // built on that says the call was free.
        let cli = cli(Scripted::new(Ok(ProcessOutput::new(
            Some(0),
            an_envelope("Four."),
        ))))
        .reading(claude::envelope());

        let usage = cli
            .chat(request())
            .await
            .map(|r| r.usage)
            .unwrap_or_default();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.cache_write_tokens, Some(26_193));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(usage.coverage(), crate::UsageCoverage::Exact);
    }

    #[tokio::test]
    async fn a_tool_that_promised_json_and_printed_prose_is_unreadable() {
        let cli = cli(Scripted::new(Ok(ProcessOutput::new(
            Some(0),
            b"just some words".to_vec(),
        ))))
        .reading(claude::envelope());

        let refused = cli.chat(request()).await;
        assert!(matches!(refused, Err(Error::Unreadable(_))), "{refused:?}");
    }

    #[tokio::test]
    async fn an_envelope_with_no_usage_in_it_reports_absent() {
        // The other half. Reading an envelope must not invent numbers for a tool that
        // measured nothing.
        let bare =
            serde_json::to_vec(&serde_json::json!({ "result": "Four." })).unwrap_or_default();
        let cli =
            cli(Scripted::new(Ok(ProcessOutput::new(Some(0), bare)))).reading(claude::envelope());

        let usage = cli
            .chat(request())
            .await
            .map(|r| r.usage)
            .unwrap_or_default();
        assert_eq!(usage.coverage(), crate::UsageCoverage::Absent);
    }

    #[tokio::test]
    async fn a_reply_reports_no_usage_rather_than_zero() {
        let cli = cli(Scripted::new(Ok(ProcessOutput::new(
            Some(0),
            b"four".to_vec(),
        ))));
        let usage = cli
            .chat(request())
            .await
            .map(|r| r.usage)
            .unwrap_or_default();
        assert_eq!(usage.coverage(), crate::UsageCoverage::Absent);
    }
}
