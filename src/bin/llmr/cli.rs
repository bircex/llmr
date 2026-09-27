//! Vendor command line tools as providers: Claude Code, Codex and Gemini CLI.
//!
//! The image carries a tested version of each under `/opt/llmr/cli/<tool>`. The management
//! API can install another version onto the data volume, under `<data>/cli/<tool>`, and that
//! copy is used from then on, across restarts, until it is reset.
//!
//! # How a call runs
//!
//! Each call gets a fresh directory under `<data>/run`, which is the tool's home, working
//! directory and temporary directory, and which is removed when the call ends. Nothing one
//! call leaves behind (a session file, a history, an error report) is seen by the next.
//!
//! The tool starts with an empty environment plus what it needs: `PATH`, its home, the proxy
//! and certificate variables llmr itself was given, and the one provider credential it is
//! calling with. It never sees `LLMR_MASTER_KEY`, `LLMR_TOKEN` or another provider's key.
//!
//! Every tool that can act is switched off: reading files, running commands, searching the
//! web, starting sub agents. What is left is a model answering a prompt. The tool's own
//! retries are off too, because retrying and falling back is the route's job, and a tool that
//! retries a 429 for two minutes holds a request that could have moved on.
//!
//! The tool runs in its own process group, and the whole group is killed when the call ends,
//! times out or is abandoned, so a wrapper's children do not outlive it.
//!
//! # API key or subscription
//!
//! Each tool takes either the vendor's API key or the sign in of a subscription:
//!
//! | Tool | Subscription credential | How it reaches the tool |
//! |---|---|---|
//! | Claude Code | the token `claude setup-token` prints, `sk-ant-oat…` | `CLAUDE_CODE_OAUTH_TOKEN` |
//! | Codex | the `auth.json` `codex login` writes | `$CODEX_HOME/auth.json` |
//! | Gemini CLI | the `oauth_creds.json` a Google sign in writes | `~/.gemini/oauth_creds.json` |
//!
//! Codex and Gemini CLI refresh their tokens themselves and write them back to the file, and
//! Codex's refresh token changes every time. So the file is read back after every call, and a
//! new one is sealed into the store in place of the old; a call started with the old refresh
//! token after that would fail. A Codex call that will refresh (its access token has less than
//! six minutes left) runs alone, so two calls cannot spend the same refresh token.

use crate::records::ProviderType;
use async_trait::async_trait;
use llmr::{
    Access, ChatRequest, ChatResponse, ContentBlock, Message, ModelCapabilities, ModelId, Reach,
    Role, Secret, StopReason, Usage,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// A command line tool this build can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    ClaudeCode,
    Codex,
    GeminiCli,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::ClaudeCode, Tool::Codex, Tool::GeminiCli];

    /// The name in URLs, and the provider type that runs it.
    pub fn name(self) -> &'static str {
        match self {
            Tool::ClaudeCode => "claude-code",
            Tool::Codex => "codex",
            Tool::GeminiCli => "gemini-cli",
        }
    }

    pub fn parse(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }

    pub fn title(self) -> &'static str {
        match self {
            Tool::ClaudeCode => "Claude Code",
            Tool::Codex => "Codex",
            Tool::GeminiCli => "Gemini CLI",
        }
    }

    /// The command it installs.
    pub fn program(self) -> &'static str {
        match self {
            Tool::ClaudeCode => "claude",
            Tool::Codex => "codex",
            Tool::GeminiCli => "gemini",
        }
    }

    /// The npm package it comes from. Fixed: an update installs this package and no other.
    pub fn package(self) -> &'static str {
        match self {
            Tool::ClaudeCode => "@anthropic-ai/claude-code",
            Tool::Codex => "@openai/codex",
            Tool::GeminiCli => "@google/gemini-cli",
        }
    }

    pub fn provider_type(self) -> ProviderType {
        match self {
            Tool::ClaudeCode => ProviderType::ClaudeCode,
            Tool::Codex => ProviderType::Codex,
            Tool::GeminiCli => ProviderType::GeminiCli,
        }
    }
}

/// Where the copy in use came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Baked into the image, the version this release was tested with.
    Image,
    /// Installed onto the data volume through the management API.
    Updated,
}

/// The copy of a tool that a call would run.
#[derive(Debug, Clone)]
pub struct Installed {
    pub source: Source,
    pub version: Option<String>,
    pub program: PathBuf,
}

/// The largest output a tool may print. Far above any answer; a tool printing more is
/// broken, and reading it all would be the gateway's memory, not the tool's.
const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

/// How long an update may take. The tools are hundreds of megabytes.
const UPDATE_DEADLINE: Duration = Duration::from_secs(900);

/// How long `--version` or `npm view` may take.
const QUICK_DEADLINE: Duration = Duration::from_secs(30);

/// Variables passed through from llmr's own environment, because a tool behind a proxy or a
/// private certificate authority cannot reach its vendor without them. Nothing else is.
const INHERITED: [&str; 12] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NODE_EXTRA_CA_CERTS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "TZ",
];

/// Codex features that give the model something to act with. All off.
///
/// Written as `-c features.<name>=false` rather than `--disable <name>`: the flag refuses a
/// name the installed version does not know, the setting ignores it, and an update must not
/// turn a renamed feature into a broken provider.
const CODEX_FEATURES_OFF: [&str; 19] = [
    "shell_tool",
    "unified_exec",
    "apps",
    "browser_use",
    "browser_use_external",
    "computer_use",
    "multi_agent",
    "image_generation",
    "view_image",
    "plugins",
    "hooks",
    "code_mode_host",
    "sleep_tool",
    "skill_search",
    "tool_suggest",
    "goals",
    "in_app_browser",
    "workspace_dependencies",
    "memories",
];

/// Gemini CLI's settings for a call: the sign in to use, no tools, one attempt.
///
/// `tools.core` is an allow list, so an empty one also covers tools a later version adds.
fn gemini_settings(subscription: bool) -> String {
    let auth = if subscription {
        "oauth-personal"
    } else {
        "gemini-api-key"
    };
    serde_json::json!({
        "security": { "auth": { "selectedType": auth } },
        "tools": { "core": [] },
        "general": { "maxAttempts": 1, "disableAutoUpdate": true, "disableUpdateNag": true },
        "privacy": { "usageStatisticsEnabled": false },
        "telemetry": { "enabled": false }
    })
    .to_string()
}

/// The system prompt a call gets when its request has none. Without one, Claude Code and
/// Gemini CLI send their own, which is tens of kilobytes of instructions for an agent that
/// edits code: spent from the subscription on every call, and the wrong voice for an answer.
const DEFAULT_SYSTEM: &str = "You are a helpful assistant.";

/// How long before its access token expires a Codex call refreshes it. Measured: 0.157.1
/// refreshes with four minutes left and not with seven. A call inside this window runs alone.
const CODEX_REFRESH_WINDOW_SECS: i64 = 360;

/// A tool's credential: the vendor's API key, or a subscription's sign in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    ApiKey,
    Subscription,
}

impl Auth {
    /// Which kind a credential is, from its shape.
    ///
    /// # Errors
    ///
    /// When it looks like a sign in file and is not the one this tool writes.
    pub fn of(tool: Tool, credential: &str) -> Result<Auth, String> {
        let credential = credential.trim();
        let json = credential.starts_with('{');
        match tool {
            Tool::ClaudeCode if credential.starts_with("sk-ant-oat") => Ok(Auth::Subscription),
            Tool::ClaudeCode if json => Err(
                "a Claude Code credential is an API key, or the token `claude setup-token` prints"
                    .into(),
            ),
            Tool::Codex if json => {
                let doc: Value = serde_json::from_str(credential)
                    .map_err(|e| format!("this is not the auth.json `codex login` writes: {e}"))?;
                let tokens = &doc["tokens"];
                if tokens["access_token"].is_string() && tokens["refresh_token"].is_string() {
                    Ok(Auth::Subscription)
                } else {
                    Err(
                        "this JSON has no tokens.access_token and tokens.refresh_token: paste the \
                         auth.json `codex login` writes"
                            .into(),
                    )
                }
            }
            Tool::GeminiCli if json => {
                let doc: Value = serde_json::from_str(credential).map_err(|e| {
                    format!("this is not the oauth_creds.json a Google sign in writes: {e}")
                })?;
                if doc["refresh_token"].is_string() {
                    Ok(Auth::Subscription)
                } else {
                    Err(
                        "this JSON has no refresh_token: paste the oauth_creds.json a Google \
                         sign in writes"
                            .into(),
                    )
                }
            }
            _ => Ok(Auth::ApiKey),
        }
    }
}

/// A subscription sign in kept in a file the tool rewrites: Codex's and Gemini CLI's.
///
/// One per provider, shared by every call and every rebuild of the gateway, because the file
/// a call hands back can hold a new refresh token that the next call has to use.
pub struct Login {
    provider: String,
    /// Shared by calls; taken alone by a call that is going to refresh.
    turn: tokio::sync::RwLock<()>,
    /// What the store held when this was last in step with it, and the newest file.
    state: std::sync::Mutex<(String, String)>,
}

impl Login {
    fn current(&self) -> String {
        self.state().1.clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, (String, String)> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Replaces the sign in with the one a call refreshed it to, if it is still the one the
    /// call started with. `false` when something replaced it meanwhile.
    fn advance(&self, before: &str, after: &str) -> bool {
        let mut state = self.state();
        if state.1 != before {
            return false;
        }
        state.1 = after.to_string();
        true
    }

    /// Records what the store now holds.
    fn settle(&self, stored: &str) {
        self.state().0 = stored.to_string();
    }
}

/// Whether a Codex `auth.json`'s access token is close enough to expiry that the call will
/// refresh it. Unreadable counts as yes: running alone is the safe side.
fn codex_refresh_due(auth_json: &str, now: i64) -> bool {
    use base64::Engine;
    let exp = serde_json::from_str::<Value>(auth_json)
        .ok()
        .and_then(|doc| doc["tokens"]["access_token"].as_str().map(String::from))
        .and_then(|jwt| jwt.split('.').nth(1).map(String::from))
        .and_then(|payload| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload.trim_end_matches('='))
                .ok()
        })
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|claims| claims["exp"].as_i64());
    exp.is_none_or(|exp| exp - now < CODEX_REFRESH_WINDOW_SECS)
}

/// The file a tool left behind, if it is a sign in worth keeping in place of `before`.
///
/// Codex writes its whole `auth.json`. Gemini CLI writes what Google returned on a refresh,
/// which need not repeat the refresh token, so that is carried over.
fn refreshed(tool: Tool, before: &str, after: &str) -> Option<String> {
    let mut doc: Value = serde_json::from_str(after).ok()?;
    match tool {
        Tool::Codex => {
            doc["tokens"]["refresh_token"].as_str()?;
        }
        Tool::GeminiCli => {
            doc["access_token"].as_str()?;
            if !doc["refresh_token"].is_string() {
                let old: Value = serde_json::from_str(before).ok()?;
                doc["refresh_token"] = old["refresh_token"].clone();
                doc["refresh_token"].as_str()?;
            }
        }
        Tool::ClaudeCode => return None,
    }
    let old: Value = serde_json::from_str(before).ok()?;
    (doc != old).then(|| doc.to_string())
}

/// Where a subscription's sign in file goes inside a call's home.
fn login_file(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Codex => Some(".codex/auth.json"),
        Tool::GeminiCli => Some(".gemini/oauth_creds.json"),
        Tool::ClaudeCode => None,
    }
}

/// Everything one call of a tool needs.
struct Invocation<'a> {
    tool: Tool,
    auth: Auth,
    /// The API key, the Claude Code token, or the sign in file's contents.
    secret: &'a str,
    base_url: Option<&'a str>,
    model: &'a str,
    system: &'a str,
    prompt: &'a str,
    timeout: Duration,
}

/// A finished call, and the sign in file as the tool left it.
#[derive(Debug)]
struct Ran {
    captured: Captured,
    login_after: Option<String>,
}

/// Why an update did not happen.
#[derive(Debug)]
pub enum UpdateError {
    /// Another update is running.
    Busy,
    /// Not `latest` and not a version number.
    BadVersion,
    /// npm or the installed tool failed, with what it said.
    Failed(String),
}

/// The tools, where they are, and how to start them.
pub struct Toolbox {
    /// `/opt/llmr/cli` in the image.
    image: PathBuf,
    /// `<data>/cli`.
    volume: PathBuf,
    /// `<data>/run`, one directory per call.
    runs: PathBuf,
    /// `PATH` for tools and npm, after the tool's own `bin`.
    path: String,
    inherited: Vec<(String, String)>,
    /// `LLMR_NPM_REGISTRY`, for a mirror.
    registry: Option<String>,
    counter: AtomicU64,
    updating: tokio::sync::Mutex<()>,
    /// Calls running at once, across every tool. Each is a process of a few hundred MB.
    slots: tokio::sync::Semaphore,
    concurrency: usize,
    /// Subscription sign ins kept in files, by provider id.
    logins: std::sync::Mutex<std::collections::HashMap<String, Arc<Login>>>,
    /// Where a refreshed sign in is written back. Unset in tests that need no store.
    db: std::sync::OnceLock<crate::store::Db>,
}

/// Calls that may run at once unless `LLMR_CLI_CONCURRENCY` says otherwise.
const DEFAULT_CONCURRENCY: usize = 8;

impl Toolbox {
    #[cfg(test)]
    pub fn new(image: PathBuf, data: &Path) -> Toolbox {
        Toolbox::with_concurrency(image, data, DEFAULT_CONCURRENCY)
    }

    pub fn with_concurrency(image: PathBuf, data: &Path, concurrency: usize) -> Toolbox {
        let concurrency = concurrency.max(1);
        let inherited = INHERITED
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).to_string(), value))
            })
            .collect();
        Toolbox {
            image,
            volume: data.join("cli"),
            runs: data.join("run"),
            path: std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
            inherited,
            registry: std::env::var("LLMR_NPM_REGISTRY")
                .ok()
                .filter(|r| !r.trim().is_empty()),
            counter: AtomicU64::new(0),
            updating: tokio::sync::Mutex::new(()),
            slots: tokio::sync::Semaphore::new(concurrency),
            concurrency,
            logins: std::sync::Mutex::new(std::collections::HashMap::new()),
            db: std::sync::OnceLock::new(),
        }
    }

    /// Gives the toolbox the store that refreshed sign ins are written back to.
    pub fn attach(&self, db: crate::store::Db) {
        let _ = self.db.set(db);
    }

    /// The sign in a provider's calls share, in step with what the store holds.
    ///
    /// The store is the panel's word: when it holds something other than what this was last
    /// in step with, the panel changed the credential, and that replaces whatever the calls
    /// had refreshed to.
    pub fn login(&self, provider: &str, stored: &str) -> Arc<Login> {
        let mut logins = match self.logins.lock() {
            Ok(logins) => logins,
            Err(poisoned) => poisoned.into_inner(),
        };
        let login = logins
            .entry(provider.to_string())
            .or_insert_with(|| {
                Arc::new(Login {
                    provider: provider.to_string(),
                    turn: tokio::sync::RwLock::new(()),
                    state: std::sync::Mutex::new((stored.to_string(), stored.to_string())),
                })
            })
            .clone();
        let mut state = login.state();
        if state.0 != stored {
            *state = (stored.to_string(), stored.to_string());
        }
        drop(state);
        login
    }

    /// Keeps a sign in a call refreshed: in memory for the next call, and in the store. Neither
    /// when it was replaced while the call ran, which is the panel changing the credential.
    async fn keep(&self, login: &Login, before: String, after: String) {
        if !login.advance(&before, &after) {
            tracing::info!(
                provider = %login.provider,
                "a refreshed sign in was not kept: the credential was replaced meanwhile"
            );
            return;
        }
        let Some(db) = self.db.get() else {
            login.settle(&after);
            return;
        };
        let provider = login.provider.clone();
        let written = after.clone();
        match db
            .run_mut(move |store| store.swap_credential(&provider, &before, &written))
            .await
        {
            Ok(true) => login.settle(&after),
            Ok(false) => tracing::info!(
                provider = %login.provider,
                "a refreshed sign in was not written back: the credential was replaced meanwhile"
            ),
            Err(e) => tracing::warn!(
                provider = %login.provider,
                error = %e,
                "a refreshed sign in could not be written back; the next call uses it from memory"
            ),
        }
    }

    /// The toolbox the image is laid out for: tools under `LLMR_CLI_DIR`, `/opt/llmr/cli`
    /// unless set, and `LLMR_CLI_CONCURRENCY` calls at once.
    pub fn from_env(data: &Path) -> Toolbox {
        let image = std::env::var("LLMR_CLI_DIR").unwrap_or_else(|_| "/opt/llmr/cli".into());
        let concurrency = std::env::var("LLMR_CLI_CONCURRENCY")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_CONCURRENCY);
        Toolbox::with_concurrency(PathBuf::from(image), data, concurrency)
    }

    /// Creates the directories, and removes what calls and updates cut short by a stop left.
    ///
    /// # Errors
    ///
    /// When the data directory is not writable.
    pub fn prepare(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.volume)?;
        if self.runs.exists() {
            std::fs::remove_dir_all(&self.runs)?;
        }
        std::fs::create_dir_all(&self.runs)?;
        for entry in std::fs::read_dir(&self.volume)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }

    fn prefix(&self, tool: Tool, source: Source) -> PathBuf {
        match source {
            Source::Image => self.image.join(tool.name()),
            Source::Updated => self.volume.join(tool.name()),
        }
    }

    /// The copy a call would run: the updated one when there is one, else the image's.
    pub fn installed(&self, tool: Tool) -> Option<Installed> {
        [Source::Updated, Source::Image]
            .into_iter()
            .find_map(|source| installed_at(&self.prefix(tool, source), tool, source))
    }

    /// The version the image carries, which this release was tested with.
    pub fn image_version(&self, tool: Tool) -> Option<String> {
        installed_at(&self.prefix(tool, Source::Image), tool, Source::Image).and_then(|i| i.version)
    }

    fn run_dir(&self) -> std::io::Result<RunDir> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let path = self.runs.join(format!("{}-{n}", std::process::id()));
        std::fs::create_dir_all(path.join("tmp"))?;
        Ok(RunDir(path))
    }

    /// The environment every tool and npm start with.
    fn base_env(&self, bin: Option<&Path>, home: &Path) -> Vec<(String, String)> {
        let path = match bin {
            Some(bin) => format!("{}:{}", bin.display(), self.path),
            None => self.path.clone(),
        };
        let mut env = vec![
            ("PATH".to_string(), path),
            ("HOME".to_string(), home.display().to_string()),
            ("TMPDIR".to_string(), home.join("tmp").display().to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
            ("NO_COLOR".to_string(), "1".to_string()),
        ];
        env.extend(self.inherited.iter().cloned());
        env
    }

    /// Runs one call of a tool, with the prompt on standard input.
    async fn call(&self, run: &Invocation<'_>) -> llmr::Result<Ran> {
        let tool = run.tool;
        let installed = self.installed(tool).ok_or_else(|| {
            llmr::Error::Unsupported(format!(
                "{} is not installed; POST /manage/clis/{}/update installs it",
                tool.title(),
                tool.name()
            ))
        })?;
        // Waiting for a slot counts against the call's own time. A call that cannot get one in
        // time is transient, so the route set moves on rather than queueing without end.
        let started = std::time::Instant::now();
        let _slot = match tokio::time::timeout(run.timeout, self.slots.acquire()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) | Err(_) => {
                return Err(llmr::Error::Transient(format!(
                "all {} command line slots stayed busy for {}s; LLMR_CLI_CONCURRENCY sets how many",
                self.concurrency,
                run.timeout.as_secs()
            )))
            }
        };
        let timeout = run.timeout.saturating_sub(started.elapsed());
        let home = self.run_dir().map_err(|e| {
            llmr::Error::Transient(format!("preparing a directory for the call: {e}"))
        })?;
        prepare_home(run, &home.0).map_err(|e| {
            llmr::Error::Transient(format!("preparing a directory for the call: {e}"))
        })?;

        let mut env = self.base_env(installed.program.parent(), &home.0);
        env.extend(tool_env(run, &home.0));
        let args = arguments(run, &home.0);

        let mut command = tokio::process::Command::new(&installed.program);
        command
            .args(&args)
            .current_dir(&home.0)
            .env_clear()
            .envs(env);
        let captured = capture(command, run.prompt.as_bytes(), timeout).await;
        // Read before the directory goes: the tool may have refreshed its sign in.
        let login_after = match (run.auth, login_file(tool)) {
            (Auth::Subscription, Some(file)) => std::fs::read_to_string(home.0.join(file)).ok(),
            _ => None,
        };
        drop(home);
        match captured {
            Ok(captured) => Ok(Ran {
                captured,
                login_after,
            }),
            Err(Failure::NotFound) => Err(llmr::Error::Unsupported(format!(
                "{} could not be started from {}",
                tool.title(),
                installed.program.display()
            ))),
            Err(Failure::Timeout) => Err(llmr::Error::Timeout { elapsed: timeout }),
            Err(Failure::TooLarge) => Err(llmr::Error::Unreadable(format!(
                "{} printed more than {} MB",
                tool.title(),
                OUTPUT_LIMIT / 1024 / 1024
            ))),
            Err(Failure::Io(e)) => Err(llmr::Error::Transient(format!(
                "running {}: {e}",
                tool.title()
            ))),
        }
    }

    /// The newest version npm has of a tool.
    ///
    /// # Errors
    ///
    /// When npm cannot be run or the registry cannot be reached, with what npm said.
    pub async fn latest(&self, tool: Tool) -> Result<String, String> {
        let home = self.run_dir().map_err(|e| e.to_string())?;
        let mut command = tokio::process::Command::new("npm");
        command
            .args(["view", tool.package(), "version"])
            .current_dir(&home.0)
            .env_clear()
            .envs(self.npm_env(&home.0));
        let out = capture(command, b"", QUICK_DEADLINE)
            .await
            .map_err(|f| format!("npm view: {}", f.describe()))?;
        if out.exit_code != Some(0) {
            return Err(format!("npm view: {}", first_line(&out.stderr)));
        }
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if valid_version(&version) {
            Ok(version)
        } else {
            Err(format!("npm view printed {version:?}, not a version"))
        }
    }

    fn npm_env(&self, home: &Path) -> Vec<(String, String)> {
        let mut env = self.base_env(None, home);
        env.push((
            "npm_config_cache".into(),
            home.join("npm-cache").display().to_string(),
        ));
        env.push(("npm_config_update_notifier".into(), "false".into()));
        env.push(("npm_config_fund".into(), "false".into()));
        env.push(("npm_config_audit".into(), "false".into()));
        if let Some(registry) = &self.registry {
            env.push(("npm_config_registry".into(), registry.clone()));
        }
        env
    }

    /// Installs a version of a tool onto the volume and makes it the one calls run.
    ///
    /// Installed beside the current copy, checked by running it, then swapped in with a
    /// rename, so a failed or interrupted update leaves the copy in use untouched.
    ///
    /// # Errors
    ///
    /// When another update is running, the version is not one, or the install or its check
    /// failed.
    pub async fn update(&self, tool: Tool, version: &str) -> Result<Installed, UpdateError> {
        if version != "latest" && !valid_version(version) {
            return Err(UpdateError::BadVersion);
        }
        let _one_at_a_time = self.updating.try_lock().map_err(|_| UpdateError::Busy)?;
        let failed = |e: std::io::Error| UpdateError::Failed(e.to_string());

        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let staging = self.volume.join(format!(".staging-{}-{n}", tool.name()));
        let home = self.run_dir().map_err(failed)?;

        let mut command = tokio::process::Command::new("npm");
        command
            .args([
                "install",
                "--global",
                "--prefix",
                &staging.display().to_string(),
                "--loglevel=error",
                &format!("{}@{version}", tool.package()),
            ])
            .current_dir(&home.0)
            .env_clear()
            .envs(self.npm_env(&home.0));
        let outcome = capture(command, b"", UPDATE_DEADLINE).await;
        drop(home);
        let out = match outcome {
            Ok(out) => out,
            Err(f) => {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Err(UpdateError::Failed(format!(
                    "npm install: {}",
                    f.describe()
                )));
            }
        };
        if out.exit_code != Some(0) {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(UpdateError::Failed(format!(
                "npm install: {}",
                first_line(&out.stderr)
            )));
        }

        // The copy has to start before it replaces one that does.
        let Some(fresh) = installed_at(&staging, tool, Source::Updated) else {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(UpdateError::Failed(format!(
                "npm install finished and {} is not in {}",
                tool.program(),
                staging.display()
            )));
        };
        if let Err(why) = self.starts(tool, &fresh.program).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(UpdateError::Failed(why));
        }

        let target = self.prefix(tool, Source::Updated);
        let old = self.volume.join(format!(".old-{}-{n}", tool.name()));
        if target.exists() {
            tokio::fs::rename(&target, &old).await.map_err(failed)?;
        }
        tokio::fs::rename(&staging, &target).await.map_err(failed)?;
        let _ = tokio::fs::remove_dir_all(&old).await;

        self.installed(tool)
            .ok_or_else(|| UpdateError::Failed("the updated copy disappeared".into()))
    }

    /// Runs `--version` in the same isolation a call gets.
    async fn starts(&self, tool: Tool, program: &Path) -> Result<(), String> {
        let home = self.run_dir().map_err(|e| e.to_string())?;
        let mut command = tokio::process::Command::new(program);
        command
            .arg("--version")
            .current_dir(&home.0)
            .env_clear()
            .envs(self.base_env(program.parent(), &home.0));
        let out = capture(command, b"", QUICK_DEADLINE)
            .await
            .map_err(|f| format!("{} --version: {}", tool.program(), f.describe()))?;
        if out.exit_code == Some(0) {
            Ok(())
        } else {
            Err(format!(
                "{} --version exited with {:?}: {}",
                tool.program(),
                out.exit_code,
                first_line(&out.stderr)
            ))
        }
    }

    /// Removes the updated copy, so calls run the image's again. `false` when there was none.
    ///
    /// # Errors
    ///
    /// When an update is running, or the copy cannot be removed.
    pub async fn reset(&self, tool: Tool) -> Result<bool, UpdateError> {
        let _one_at_a_time = self.updating.try_lock().map_err(|_| UpdateError::Busy)?;
        let target = self.prefix(tool, Source::Updated);
        if !target.exists() {
            return Ok(false);
        }
        // Renamed first, so no call starts from a half removed copy.
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let old = self.volume.join(format!(".old-{}-{n}", tool.name()));
        tokio::fs::rename(&target, &old)
            .await
            .map_err(|e| UpdateError::Failed(e.to_string()))?;
        tokio::fs::remove_dir_all(&old)
            .await
            .map_err(|e| UpdateError::Failed(e.to_string()))?;
        Ok(true)
    }
}

/// A copy under this npm prefix, if its command and its package are both there.
fn installed_at(prefix: &Path, tool: Tool, source: Source) -> Option<Installed> {
    let program = prefix.join("bin").join(tool.program());
    if !program.exists() {
        return None;
    }
    let manifest = prefix
        .join("lib/node_modules")
        .join(tool.package())
        .join("package.json");
    let version = std::fs::read(manifest)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .and_then(|doc| doc["version"].as_str().map(String::from));
    Some(Installed {
        source,
        version,
        program,
    })
}

/// `1.2.3`, or `1.2.3-beta.4`. Nothing an npm spec could read as a URL, a tag or a path.
pub fn valid_version(text: &str) -> bool {
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    numbers.len() == 3
        && numbers
            .iter()
            .all(|n| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
        && pre.is_none_or(|p| {
            !p.is_empty()
                && p.len() <= 64
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
}

/// A model id that can go on a tool's command line: not a flag, one argument.
pub fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && !model.starts_with('-')
        && !model.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// A call's directory, removed when the call is over however it ended.
struct RunDir(PathBuf);

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn prepare_home(run: &Invocation<'_>, home: &Path) -> std::io::Result<()> {
    let subscription = run.auth == Auth::Subscription;
    match run.tool {
        Tool::GeminiCli => {
            std::fs::create_dir_all(home.join(".gemini"))?;
            std::fs::write(
                home.join(".gemini/settings.json"),
                gemini_settings(subscription),
            )?;
        }
        Tool::Codex => std::fs::create_dir_all(home.join(".codex"))?,
        Tool::ClaudeCode => {}
    }
    if let (true, Some(file)) = (subscription, login_file(run.tool)) {
        std::fs::write(home.join(file), run.secret)?;
    }
    // Codex keeps its own instructions: the ChatGPT backend accepts only those, so the system
    // prompt goes into the conversation instead.
    if run.tool != Tool::Codex {
        std::fs::write(home.join("system.md"), run.system)?;
    }
    Ok(())
}

fn tool_env(run: &Invocation<'_>, home: &Path) -> Vec<(String, String)> {
    let subscription = run.auth == Auth::Subscription;
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |name: &str, value: String| env.push((name.to_string(), value));
    match run.tool {
        Tool::ClaudeCode => {
            if subscription {
                set("CLAUDE_CODE_OAUTH_TOKEN", run.secret.to_string());
            } else {
                set("ANTHROPIC_API_KEY", run.secret.to_string());
            }
            set("CLAUDE_CODE_MAX_RETRIES", "0".into());
            set("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into());
            set("DISABLE_AUTOUPDATER", "1".into());
            if let Some(url) = run.base_url {
                set("ANTHROPIC_BASE_URL", url.to_string());
            }
        }
        Tool::Codex => {
            if !subscription {
                set("CODEX_API_KEY", run.secret.to_string());
            }
            set("CODEX_HOME", home.join(".codex").display().to_string());
        }
        Tool::GeminiCli => {
            if subscription {
                if let Some(url) = run.base_url {
                    set("CODE_ASSIST_ENDPOINT", url.to_string());
                }
            } else {
                set("GEMINI_API_KEY", run.secret.to_string());
                if let Some(url) = run.base_url {
                    set("GOOGLE_GEMINI_BASE_URL", url.to_string());
                }
            }
            set(
                "GEMINI_SYSTEM_MD",
                home.join("system.md").display().to_string(),
            );
        }
    }
    env
}

/// The arguments for one non interactive call, with the prompt on standard input.
fn arguments(run: &Invocation<'_>, home: &Path) -> Vec<String> {
    let subscription = run.auth == Auth::Subscription;
    let mut args: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| args.extend(items.iter().map(|s| (*s).to_string()));
    match run.tool {
        Tool::ClaudeCode => {
            // `--tools ""` leaves the model nothing to run. `--bare` also skips hooks, plugins,
            // memory and CLAUDE.md discovery, and reads only an API key; a subscription token
            // needs the full mode, where the empty home has none of those to find anyway.
            push(&[
                "-p",
                "--output-format",
                "json",
                "--tools",
                "",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--exclude-dynamic-system-prompt-sections",
            ]);
            if !subscription {
                push(&["--bare"]);
            }
            let system = home.join("system.md").display().to_string();
            push(&["--system-prompt-file", &system, "--model", run.model]);
        }
        Tool::Codex => {
            push(&[
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--ephemeral",
                "--color",
                "never",
                "--sandbox",
                "read-only",
            ]);
            // A provider of our own, because only a configured provider's retries can be
            // switched off. A subscription with no base URL keeps Codex's own, which is the
            // one that knows the ChatGPT backend.
            let provider = match (subscription, run.base_url) {
                (false, url) => Some(format!(
                    "model_providers.llmr={{name=\"llmr\",base_url=\"{}\",env_key=\"CODEX_API_KEY\",\
                     wire_api=\"responses\",request_max_retries=0,stream_max_retries=0}}",
                    toml_escape(url.unwrap_or(crate::records::OPENAI_BASE_URL))
                )),
                (true, Some(url)) => Some(format!(
                    "model_providers.llmr={{name=\"llmr\",base_url=\"{}\",requires_openai_auth=true,\
                     wire_api=\"responses\",request_max_retries=0,stream_max_retries=0}}",
                    toml_escape(url)
                )),
                (true, None) => None,
            };
            if let Some(provider) = provider {
                push(&["-c", "model_provider=\"llmr\"", "-c", &provider]);
            }
            for feature in CODEX_FEATURES_OFF {
                push(&["-c", &format!("features.{feature}=false")]);
            }
            push(&["-c", "web_search=\"disabled\"", "-m", run.model, "-"]);
        }
        Tool::GeminiCli => {
            push(&[
                "--output-format",
                "json",
                "--skip-trust",
                "--model",
                run.model,
            ]);
        }
    }
    args
}

fn toml_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// What a finished process left behind.
#[derive(Debug)]
pub struct Captured {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug)]
enum Failure {
    NotFound,
    Timeout,
    TooLarge,
    Io(std::io::Error),
}

impl Failure {
    fn describe(&self) -> String {
        match self {
            Failure::NotFound => "not installed".into(),
            Failure::Timeout => "did not finish in time".into(),
            Failure::TooLarge => "printed too much".into(),
            Failure::Io(e) => e.to_string(),
        }
    }
}

/// Kills a process group when dropped: at the end of a call, on a timeout, and when the
/// request that started it is abandoned.
// Read only where there are process groups to kill.
#[cfg_attr(not(unix), allow(dead_code))]
struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|p| i32::try_from(p).ok()) {
            // The child was made the leader of its own group, so its pid names the group.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

async fn read_limited(pipe: Option<impl AsyncRead + Unpin>) -> Result<Vec<u8>, Failure> {
    let mut out = Vec::new();
    if let Some(pipe) = pipe {
        let limit = u64::try_from(OUTPUT_LIMIT).unwrap_or(u64::MAX);
        pipe.take(limit + 1)
            .read_to_end(&mut out)
            .await
            .map_err(Failure::Io)?;
    }
    if out.len() > OUTPUT_LIMIT {
        return Err(Failure::TooLarge);
    }
    Ok(out)
}

/// Starts a command in its own process group, writes `input`, and collects what it prints.
async fn capture(
    mut command: tokio::process::Command,
    input: &[u8],
    timeout: Duration,
) -> Result<Captured, Failure> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Failure::NotFound,
        _ => Failure::Io(e),
    })?;
    let _group = Group(child.id());

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let work = async {
        // Written while the output is read, so a long prompt and a chatty tool cannot wait
        // on each other. A tool that exits without reading it all is not an error here; its
        // exit code says what happened.
        let write = async {
            if let Some(mut pipe) = stdin {
                let _ = pipe.write_all(input).await;
                let _ = pipe.shutdown().await;
            }
        };
        let ((), out, err) = tokio::join!(write, read_limited(stdout), read_limited(stderr));
        let (out, err) = (out?, err?);
        let status = child.wait().await.map_err(Failure::Io)?;
        Ok(Captured {
            exit_code: status.code(),
            stdout: out,
            stderr: err,
        })
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => Err(Failure::Timeout),
    }
}

fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    clip(
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("nothing on standard error"),
    )
}

fn clip(text: &str) -> String {
    const MAX: usize = 400;
    match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

// ----- reading what a tool printed ------------------------------------------------------

/// An answer read from a tool's output.
#[derive(Debug)]
pub struct Answer {
    pub text: String,
    pub usage: Usage,
    pub stop: Option<StopReason>,
    /// The model the tool says answered, when it says exactly one.
    pub model: Option<String>,
}

/// Reads a finished call: the answer, or the failure classified the way an API's status
/// code would be, so a bad key is not retried and a rate limit falls through.
pub fn read(tool: Tool, out: &Captured) -> llmr::Result<Answer> {
    match tool {
        Tool::ClaudeCode => read_claude(out),
        Tool::Codex => read_codex(out),
        Tool::GeminiCli => read_gemini(out),
    }
}

/// A failure with an HTTP status, as the API providers classify one.
fn failure(tool: Tool, status: Option<u64>, said: &str) -> llmr::Error {
    let said = format!("{}: {}", tool.title(), clip(said.trim()));
    match status {
        Some(401 | 403) => llmr::Error::Auth(said),
        Some(404) => llmr::Error::NotFound(said),
        Some(400 | 413 | 422) => llmr::Error::InvalidRequest(said),
        Some(429) => llmr::Error::RateLimited { retry_after: None },
        _ => llmr::Error::Transient(said),
    }
}

/// A tool that printed nothing this module can read.
fn unreadable(tool: Tool, out: &Captured) -> llmr::Error {
    match out.exit_code {
        Some(0) => llmr::Error::Unreadable(format!(
            "{} exited cleanly and printed no answer",
            tool.title()
        )),
        code => llmr::Error::Transient(format!(
            "{} exited with {}: {}",
            tool.title(),
            code.map_or_else(|| "a signal".to_string(), |c| c.to_string()),
            first_line(&out.stderr)
        )),
    }
}

fn count(value: &Value, name: &str) -> Option<u64> {
    value.get(name).and_then(Value::as_u64)
}

/// The one key of an object, when it has exactly one.
fn only_key(value: &Value) -> Option<String> {
    let map = value.as_object()?;
    (map.len() == 1)
        .then(|| map.keys().next().cloned())
        .flatten()
}

/// The vocabulary Claude Code prints, which is Anthropic's.
fn anthropic_stop(reason: &str) -> StopReason {
    match reason {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::StopSequence,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        "pause_turn" => StopReason::PauseTurn,
        "model_context_window_exceeded" => StopReason::ContextWindowExceeded,
        _ => StopReason::Other,
    }
}

fn read_claude(out: &Captured) -> llmr::Result<Answer> {
    let tool = Tool::ClaudeCode;
    let Ok(doc) = serde_json::from_slice::<Value>(out.stdout.trim_ascii()) else {
        return Err(unreadable(tool, out));
    };
    let said = doc["result"].as_str().unwrap_or("no message");
    if doc["is_error"].as_bool() == Some(true) {
        return Err(failure(tool, doc["api_error_status"].as_u64(), said));
    }
    let Some(text) = doc["result"].as_str() else {
        return Err(unreadable(tool, out));
    };
    // Anthropic's names: `input_tokens` is the prompt not served from cache.
    let reported = &doc["usage"];
    let mut usage = Usage::absent();
    if let Some(n) = count(reported, "input_tokens") {
        usage = usage.with_input(n);
    }
    if let Some(n) = count(reported, "cache_read_input_tokens") {
        usage = usage.with_cache_read(n);
    }
    if let Some(n) = count(reported, "cache_creation_input_tokens") {
        usage = usage.with_cache_write(n);
    }
    if let Some(n) = count(reported, "output_tokens") {
        usage = usage.with_output(n);
    }
    Ok(Answer {
        text: text.trim().to_string(),
        usage,
        stop: doc["stop_reason"].as_str().map(anthropic_stop),
        model: only_key(&doc["modelUsage"]),
    })
}

/// The status code inside a message such as `unexpected status 401 Unauthorized` or
/// `last status: 429 Too Many Requests`.
fn status_in(message: &str) -> Option<u64> {
    let mut rest = message;
    while let Some(at) = rest.find("status") {
        rest = &rest[at + "status".len()..];
        let digits: String = rest
            .trim_start_matches([':', ' '])
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.len() == 3 {
            return digits.parse().ok();
        }
    }
    None
}

fn read_codex(out: &Captured) -> llmr::Result<Answer> {
    let tool = Tool::Codex;
    let mut text: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut failed: Option<String> = None;
    let mut last_error: Option<String> = None;

    // One JSON event per line.
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("item.completed") if event["item"]["type"] == "agent_message" => {
                // The turn's last message is its answer.
                if let Some(said) = event["item"]["text"].as_str() {
                    text = Some(said.to_string());
                }
            }
            Some("turn.completed") => {
                // OpenAI's names: `input_tokens` includes the cached part, and
                // `output_tokens` includes reasoning.
                let reported = &event["usage"];
                let input = count(reported, "input_tokens");
                let cached = count(reported, "cached_input_tokens").unwrap_or(0);
                let mut read = Usage::absent();
                if let Some(input) = input {
                    read = read
                        .with_input(input.saturating_sub(cached))
                        .with_cache_read(cached);
                }
                if let Some(n) = count(reported, "output_tokens") {
                    read = read.with_output(n);
                }
                usage = Some(read);
            }
            Some("turn.failed") => {
                failed = Some(
                    event["error"]["message"]
                        .as_str()
                        .unwrap_or("the turn failed")
                        .to_string(),
                );
            }
            Some("error") => {
                last_error = event["message"].as_str().map(String::from);
            }
            _ => {}
        }
    }

    if let Some(message) = failed.or(if usage.is_none() { last_error } else { None }) {
        return Err(failure(tool, status_in(&message), &message));
    }
    match (text, usage) {
        (Some(text), Some(usage)) => Ok(Answer {
            text: text.trim().to_string(),
            usage,
            stop: None,
            model: None,
        }),
        _ => Err(unreadable(tool, out)),
    }
}

/// The JSON object Gemini CLI prints last, on standard error when the call failed.
fn trailing_object(bytes: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(bytes);
    let mut offset = 0;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        if line.starts_with('{') {
            start = Some(offset);
        }
        offset += line.len();
    }
    serde_json::from_str(text.get(start?..)?.trim()).ok()
}

fn read_gemini(out: &Captured) -> llmr::Result<Answer> {
    let tool = Tool::GeminiCli;
    let doc = serde_json::from_slice::<Value>(out.stdout.trim_ascii())
        .ok()
        .or_else(|| trailing_object(&out.stderr));
    let Some(doc) = doc else {
        // A sign in that fails before the JSON output starts says so only in its exit code.
        return Err(match out.exit_code {
            Some(41) => failure(tool, Some(401), &first_line(&out.stderr)),
            Some(42) => failure(tool, Some(400), &first_line(&out.stderr)),
            _ => unreadable(tool, out),
        });
    };

    if let Some(error) = doc.get("error").filter(|e| !e.is_null()) {
        let said = error["message"].as_str().unwrap_or("no message");
        // An HTTP status when the API refused; otherwise one of the CLI's own exit codes,
        // of which 41 is a credential it could not use and 42 is input it could not take.
        let status = match error["code"].as_u64() {
            Some(code @ 100..=599) => Some(code),
            Some(41) => Some(401),
            Some(42) => Some(400),
            _ => None,
        };
        return Err(failure(tool, status, said));
    }
    let Some(text) = doc["response"].as_str() else {
        return Err(unreadable(tool, out));
    };

    // Summed over every model the call used. `prompt` includes the cached part, and
    // thinking is billed as output.
    let models = doc["stats"]["models"].as_object();
    let mut usage = Usage::absent();
    if let Some(models) = models.filter(|m| !m.is_empty()) {
        let (mut prompt, mut cached, mut output) = (0u64, 0u64, 0u64);
        for tokens in models.values().map(|m| &m["tokens"]) {
            prompt += count(tokens, "prompt").unwrap_or(0);
            cached += count(tokens, "cached").unwrap_or(0);
            output +=
                count(tokens, "candidates").unwrap_or(0) + count(tokens, "thoughts").unwrap_or(0);
        }
        usage = usage
            .with_input(prompt.saturating_sub(cached))
            .with_cache_read(cached)
            .with_output(output);
    }
    Ok(Answer {
        text: text.trim().to_string(),
        usage,
        stop: None,
        model: only_key(&doc["stats"]["models"]),
    })
}

// ----- the provider -------------------------------------------------------------------

/// A command line tool answering as a provider.
pub struct CliProvider {
    id: String,
    tool: Tool,
    credential: Credential,
    base_url: Option<String>,
    /// The models the panel named. A tool cannot be asked what it serves.
    serves: BTreeSet<String>,
    timeout: Duration,
    toolbox: Arc<Toolbox>,
}

/// What a provider calls its tool with.
enum Credential {
    /// An API key, or Claude Code's subscription token: neither changes.
    Fixed(Auth, Secret),
    /// A sign in file the tool rewrites.
    File(Arc<Login>),
}

impl CliProvider {
    /// # Errors
    ///
    /// When the credential is a sign in file this tool does not write.
    pub fn new(
        id: String,
        tool: Tool,
        credential: &str,
        base_url: Option<String>,
        serves: BTreeSet<String>,
        timeout: Duration,
        toolbox: Arc<Toolbox>,
    ) -> Result<CliProvider, String> {
        let auth = Auth::of(tool, credential)?;
        let credential = match (auth, login_file(tool)) {
            (Auth::Subscription, Some(_)) => {
                Credential::File(toolbox.login(&id, credential.trim()))
            }
            _ => Credential::Fixed(auth, Secret::new("provider-credential", credential.trim())),
        };
        Ok(CliProvider {
            id,
            tool,
            credential,
            base_url,
            serves,
            timeout,
            toolbox,
        })
    }

    /// Whether this provider is signed in with a subscription rather than an API key.
    pub fn subscription(&self) -> bool {
        match &self.credential {
            Credential::Fixed(auth, _) => *auth == Auth::Subscription,
            Credential::File(_) => true,
        }
    }

    /// Runs one call, with a sign in file taken in turn and kept when the tool refreshed it.
    async fn run(&self, model: &str, system: &str, prompt: &str) -> llmr::Result<Captured> {
        let call = |auth, secret| Invocation {
            tool: self.tool,
            auth,
            secret,
            base_url: self.base_url.as_deref(),
            model,
            system,
            prompt,
            timeout: self.timeout,
        };
        match &self.credential {
            Credential::Fixed(auth, secret) => {
                let secret = secret
                    .expose_str()
                    .map_err(|_| llmr::Error::Auth("the stored credential is not text".into()))?;
                Ok(self.toolbox.call(&call(*auth, secret)).await?.captured)
            }
            Credential::File(login) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
                // A Codex call that will refresh runs alone: its refresh token is spent by the
                // refresh, and a second call holding it would be refused.
                let alone = self.tool == Tool::Codex && codex_refresh_due(&login.current(), now);
                let _turn = if alone {
                    Turn::Alone(login.turn.write().await)
                } else {
                    Turn::Shared(login.turn.read().await)
                };
                // Read after the turn is taken: a call that ran alone may have just replaced it.
                let before = login.current();
                let ran = self
                    .toolbox
                    .call(&call(Auth::Subscription, &before))
                    .await?;
                if let Some(after) = ran
                    .login_after
                    .as_deref()
                    .and_then(|after| refreshed(self.tool, &before, after))
                {
                    self.toolbox.keep(login, before, after).await;
                }
                Ok(ran.captured)
            }
        }
    }
}

/// A call's hold on a sign in: one of many, or the only one.
#[allow(dead_code)]
enum Turn<'a> {
    Shared(tokio::sync::RwLockReadGuard<'a, ()>),
    Alone(tokio::sync::RwLockWriteGuard<'a, ()>),
}

/// The conversation as one prompt, turns labelled so the model can tell who said what. The
/// system prompt leads it only for a tool that cannot be given one separately.
fn prompt(request: &ChatRequest, with_system: bool) -> String {
    let mut out = String::new();
    if let (true, Some(system)) = (with_system, &request.system) {
        out.push_str(system);
        out.push_str("\n\n");
    }
    for message in &request.messages {
        let who = match message.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            _ => "Other",
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

#[async_trait]
impl llmr::Provider for CliProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        // Text in, text out: a command line tool carries no tools, images or schemas.
        self.serves
            .contains(model.as_str())
            .then(|| ModelCapabilities::none(Reach::LocalCli))
    }

    async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
        let unmet = request
            .needs()
            .unmet_by(&ModelCapabilities::none(Reach::LocalCli));
        if !unmet.is_empty() {
            return Err(llmr::Error::Unsupported(format!(
                "{} cannot carry {} through a command line tool",
                self.id,
                unmet.join(" or ")
            )));
        }
        // Checked when the model was enabled too; this is the second line.
        if !valid_model(request.model.as_str()) {
            return Err(llmr::Error::InvalidRequest(format!(
                "{} is not a model id a command line tool can be given",
                request.model
            )));
        }
        let system = request.system.as_deref().unwrap_or(DEFAULT_SYSTEM);
        let captured = self
            .run(
                request.model.as_str(),
                system,
                &prompt(&request, self.tool == Tool::Codex),
            )
            .await?;
        let answer = read(self.tool, &captured)?;
        if answer.text.is_empty() {
            return Err(llmr::Error::Unreadable(format!(
                "{} answered with nothing",
                self.tool.title()
            )));
        }
        let model = answer
            .model
            .map_or_else(|| request.model.clone(), ModelId::from);
        let reply = ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text(answer.text)],
            },
            answer.stop.unwrap_or(StopReason::Other),
            answer.usage,
            model,
        );
        Ok(match answer.stop {
            Some(_) => reply,
            None => reply.with_stop_details("this command line tool does not say why it stopped"),
        })
    }

    /// Free: whether the tool is installed and the model was named. A tool cannot prove a
    /// key without a billable call, so that is left to a live test.
    async fn validate(&self, model: &ModelId) -> Access {
        let Some(installed) = self.toolbox.installed(self.tool) else {
            return Access::denied(format!("{} is not installed", self.tool.title()));
        };
        if !self.serves.contains(model.as_str()) {
            return Access::denied(format!("{model} is not a model of {}", self.id));
        }
        Access::unknown(format!(
            "{} {} is installed; a command line tool cannot check a key without a billable \
             call, so send a live test to prove it",
            self.tool.title(),
            installed.version.as_deref().unwrap_or("(version unknown)")
        ))
    }
}

/// A stand in for a vendor tool, written as a shell script under an npm style prefix.
#[cfg(all(test, unix))]
pub fn fake_tool(image: &Path, tool: Tool, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let prefix = image.join(tool.name());
    let bin = prefix.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join(tool.program());
    std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let package = prefix.join("lib/node_modules").join(tool.package());
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("package.json"), r#"{"version":"9.9.9"}"#).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Claude Code call with an API key, for tests about running rather than signing in.
    fn key_call(timeout: Duration) -> Invocation<'static> {
        Invocation {
            tool: Tool::ClaudeCode,
            auth: Auth::ApiKey,
            secret: "k",
            base_url: None,
            model: "m",
            system: DEFAULT_SYSTEM,
            prompt: "hi",
            timeout,
        }
    }

    fn invocation(tool: Tool, auth: Auth, base_url: Option<&'static str>) -> Invocation<'static> {
        Invocation {
            tool,
            auth,
            secret: "the-secret",
            base_url,
            model: "the-model",
            system: DEFAULT_SYSTEM,
            prompt: "hi",
            timeout: Duration::from_secs(1),
        }
    }

    fn recorded(stdout: &str, stderr: &str, exit_code: i32) -> Captured {
        Captured {
            exit_code: Some(exit_code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn claude_code_answers_with_anthropic_usage_and_stop_reason() {
        let out = recorded(include_str!("recorded/claude-code.ok.stdout"), "", 0);
        let answer = read(Tool::ClaudeCode, &out).unwrap();
        assert_eq!(answer.text, "Hello from mock");
        assert_eq!(answer.usage.input_tokens, Some(11));
        assert_eq!(answer.usage.cache_read_tokens, Some(0));
        assert_eq!(answer.usage.output_tokens, Some(4));
        assert_eq!(answer.stop, Some(StopReason::EndTurn));
        assert_eq!(answer.model.as_deref(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn claude_code_failures_are_classified_by_their_status() {
        let bad_key = recorded(include_str!("recorded/claude-code.401.stdout"), "", 1);
        assert!(matches!(
            read(Tool::ClaudeCode, &bad_key),
            Err(llmr::Error::Auth(_))
        ));
        let limited = recorded(include_str!("recorded/claude-code.429.stdout"), "", 1);
        assert!(matches!(
            read(Tool::ClaudeCode, &limited),
            Err(llmr::Error::RateLimited { .. })
        ));
    }

    #[test]
    fn codex_reads_the_last_message_and_takes_the_cache_out_of_the_prompt() {
        let out = recorded(include_str!("recorded/codex.ok.stdout"), "", 0);
        let answer = read(Tool::Codex, &out).unwrap();
        assert_eq!(answer.text, "Hello from mock");
        // 11 prompt tokens reported, 2 of them cached.
        assert_eq!(answer.usage.input_tokens, Some(9));
        assert_eq!(answer.usage.cache_read_tokens, Some(2));
        // Reasoning is inside the output count already.
        assert_eq!(answer.usage.output_tokens, Some(7));
        assert_eq!(answer.stop, None);
    }

    #[test]
    fn codex_failures_are_classified_by_the_status_in_their_message() {
        let bad_key = recorded(include_str!("recorded/codex.401.stdout"), "", 1);
        assert!(matches!(
            read(Tool::Codex, &bad_key),
            Err(llmr::Error::Auth(_))
        ));
        let limited = recorded(include_str!("recorded/codex.429.stdout"), "", 1);
        assert!(matches!(
            read(Tool::Codex, &limited),
            Err(llmr::Error::RateLimited { .. })
        ));
    }

    #[test]
    fn gemini_cli_sums_its_models_and_counts_thinking_as_output() {
        let out = recorded(include_str!("recorded/gemini-cli.ok.stdout"), "", 0);
        let answer = read(Tool::GeminiCli, &out).unwrap();
        assert_eq!(answer.text, "Hello from mock");
        assert_eq!(answer.usage.input_tokens, Some(9));
        assert_eq!(answer.usage.cache_read_tokens, Some(2));
        assert_eq!(answer.usage.output_tokens, Some(7));
        assert_eq!(answer.model.as_deref(), Some("gemini-mock"));
    }

    #[test]
    fn gemini_cli_failures_are_read_from_standard_error() {
        let bad_key = recorded("", include_str!("recorded/gemini-cli.401.stderr"), 145);
        assert!(matches!(
            read(Tool::GeminiCli, &bad_key),
            Err(llmr::Error::Auth(_))
        ));
        let limited = recorded("", include_str!("recorded/gemini-cli.429.stderr"), 173);
        assert!(matches!(
            read(Tool::GeminiCli, &limited),
            Err(llmr::Error::RateLimited { .. })
        ));
    }

    #[test]
    fn a_gemini_sign_in_refused_before_any_output_is_a_credential_failure() {
        let refused = recorded(
            "",
            "Error authenticating: FatalAuthenticationError: Manual authorization is required\n",
            41,
        );
        assert!(matches!(
            read(Tool::GeminiCli, &refused),
            Err(llmr::Error::Auth(_))
        ));
    }

    #[test]
    fn output_nobody_can_read_is_transient_when_the_tool_failed_and_unreadable_when_not() {
        for tool in Tool::ALL {
            let crashed = recorded("", "Segmentation fault\n", 139);
            assert!(
                matches!(read(tool, &crashed), Err(llmr::Error::Transient(_))),
                "{tool:?}"
            );
            let silent = recorded("", "", 0);
            assert!(
                matches!(read(tool, &silent), Err(llmr::Error::Unreadable(_))),
                "{tool:?}"
            );
        }
    }

    #[test]
    fn a_status_is_found_in_the_ways_codex_writes_it() {
        assert_eq!(
            status_in("unexpected status 401 Unauthorized: x"),
            Some(401)
        );
        assert_eq!(
            status_in("exceeded retry limit, last status: 429 Too Many"),
            Some(429)
        );
        assert_eq!(status_in("stream disconnected"), None);
        assert_eq!(status_in("status 12 and then status 503"), Some(503));
    }

    #[test]
    fn a_model_id_cannot_become_a_flag() {
        for good in [
            "claude-sonnet-5",
            "gpt-5.1",
            "gemini-2.5-pro",
            "models/x:latest",
        ] {
            assert!(valid_model(good), "{good}");
        }
        for bad in [
            "",
            "-m",
            "--dangerously-skip-permissions",
            "a b",
            "a\nb",
            "a\u{0}b",
        ] {
            assert!(!valid_model(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_a_plain_version_reaches_npm() {
        for good in ["1.2.3", "0.157.1", "2.1.283", "1.0.0-beta.4", "1.0.0-rc-1"] {
            assert!(valid_version(good), "{good}");
        }
        for bad in [
            "",
            "1.2",
            "1.2.3.4",
            "latest",
            "^1.2.3",
            "1.2.3 evil",
            "1.2.3-",
            "git+https://x",
            "../../x",
            "1.2.3-a/b",
            "1.x.3",
        ] {
            assert!(!valid_version(bad), "{bad}");
        }
    }

    #[test]
    fn every_tool_runs_with_its_actions_and_retries_off() {
        let home = Path::new("/h");
        for auth in [Auth::ApiKey, Auth::Subscription] {
            let claude = arguments(&invocation(Tool::ClaudeCode, auth, None), home);
            let at = claude.iter().position(|a| a == "--tools").unwrap();
            assert_eq!(claude[at + 1], "");
            assert!(claude.contains(&"--strict-mcp-config".to_string()));
            let at = claude
                .iter()
                .position(|a| a == "--system-prompt-file")
                .unwrap();
            assert_eq!(claude[at + 1], "/h/system.md");
            // `--bare` reads only an API key.
            assert_eq!(claude.contains(&"--bare".to_string()), auth == Auth::ApiKey);

            let codex = arguments(&invocation(Tool::Codex, auth, None), home);
            assert!(codex.contains(&"features.shell_tool=false".to_string()));
            assert!(codex.contains(&"web_search=\"disabled\"".to_string()));
            assert_eq!(codex.last().map(String::as_str), Some("-"));

            let settings: Value =
                serde_json::from_str(&gemini_settings(auth == Auth::Subscription)).unwrap();
            assert_eq!(settings["tools"]["core"], serde_json::json!([]));
            assert_eq!(settings["general"]["maxAttempts"], 1);
        }

        let codex = arguments(
            &invocation(Tool::Codex, Auth::ApiKey, Some("http://up\"stream")),
            home,
        );
        let provider = codex
            .iter()
            .find(|a| a.starts_with("model_providers.llmr="))
            .unwrap();
        assert!(provider.contains("request_max_retries=0"));
        assert!(provider.contains("stream_max_retries=0"));
        // A quote in a URL cannot end the TOML string early.
        assert!(provider.contains("base_url=\"http://up\\\"stream\""));
    }

    #[test]
    fn a_credential_is_an_api_key_or_the_sign_in_its_tool_writes() {
        let codex_login = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"a","refresh_token":"r","id_token":"i","account_id":"x"}}"#;
        let google_login = r#"{"access_token":"ya29","refresh_token":"1//r","expiry_date":1}"#;
        let cases = [
            (Tool::ClaudeCode, "sk-ant-api03-abc", Ok(Auth::ApiKey)),
            (Tool::ClaudeCode, "sk-ant-oat01-abc", Ok(Auth::Subscription)),
            (Tool::Codex, "sk-proj-abc", Ok(Auth::ApiKey)),
            (Tool::Codex, codex_login, Ok(Auth::Subscription)),
            (Tool::GeminiCli, "AIzaSyabc", Ok(Auth::ApiKey)),
            (Tool::GeminiCli, google_login, Ok(Auth::Subscription)),
        ];
        for (tool, credential, expected) in cases {
            assert_eq!(
                Auth::of(tool, credential),
                expected,
                "{tool:?} {credential}"
            );
        }
        // The wrong file, or a broken one, is refused rather than sent as a key.
        assert!(Auth::of(Tool::Codex, google_login).is_err());
        assert!(Auth::of(Tool::GeminiCli, codex_login).is_err());
        assert!(Auth::of(Tool::GeminiCli, "{not json").is_err());
        assert!(Auth::of(Tool::ClaudeCode, codex_login).is_err());
    }

    #[test]
    fn each_sign_in_reaches_its_tool_where_the_tool_reads_it() {
        let home = Path::new("/h");
        let env = |tool, auth, base| {
            tool_env(&invocation(tool, auth, base), home)
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>()
        };
        let claude = env(Tool::ClaudeCode, Auth::Subscription, None);
        assert_eq!(claude["CLAUDE_CODE_OAUTH_TOKEN"], "the-secret");
        assert!(!claude.contains_key("ANTHROPIC_API_KEY"));
        let claude = env(Tool::ClaudeCode, Auth::ApiKey, None);
        assert_eq!(claude["ANTHROPIC_API_KEY"], "the-secret");
        assert!(!claude.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));

        // Codex and Gemini CLI read a subscription from a file, never the environment.
        let codex = env(Tool::Codex, Auth::Subscription, None);
        assert!(!codex.values().any(|v| v == "the-secret"));
        assert_eq!(
            env(Tool::Codex, Auth::ApiKey, None)["CODEX_API_KEY"],
            "the-secret"
        );
        let gemini = env(Tool::GeminiCli, Auth::Subscription, Some("http://ca"));
        assert!(!gemini.values().any(|v| v == "the-secret"));
        assert_eq!(gemini["CODE_ASSIST_ENDPOINT"], "http://ca");
        assert_eq!(gemini["GEMINI_SYSTEM_MD"], "/h/system.md");
        let gemini = env(Tool::GeminiCli, Auth::ApiKey, Some("http://g"));
        assert_eq!(gemini["GEMINI_API_KEY"], "the-secret");
        assert_eq!(gemini["GOOGLE_GEMINI_BASE_URL"], "http://g");

        // A subscription keeps Codex's own provider, the one that knows the ChatGPT backend,
        // unless it is pointed somewhere else.
        let home = Path::new("/h");
        let own = arguments(&invocation(Tool::Codex, Auth::Subscription, None), home);
        assert!(!own.iter().any(|a| a.contains("model_provider")));
        let pointed = arguments(
            &invocation(Tool::Codex, Auth::Subscription, Some("http://m/v1")),
            home,
        );
        let provider = pointed
            .iter()
            .find(|a| a.starts_with("model_providers.llmr="))
            .unwrap();
        assert!(provider.contains("requires_openai_auth=true"));
        assert!(!provider.contains("env_key"));
    }

    fn codex_login(access_expires_in: i64, refresh: &str) -> String {
        use base64::Engine;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = serde_json::json!({ "exp": now + access_expires_in });
        let jwt = format!(
            "e30.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": { "access_token": jwt, "refresh_token": refresh, "id_token": "i", "account_id": "a" },
            "last_refresh": "2026-09-27T00:00:00Z",
        })
        .to_string()
    }

    #[test]
    fn a_codex_call_runs_alone_only_when_it_will_refresh() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(!codex_refresh_due(&codex_login(86_400, "r"), now));
        assert!(!codex_refresh_due(&codex_login(600, "r"), now));
        assert!(codex_refresh_due(&codex_login(200, "r"), now));
        assert!(codex_refresh_due(&codex_login(-60, "r"), now));
        assert!(codex_refresh_due("{}", now));
    }

    #[test]
    fn a_refreshed_sign_in_is_kept_whole() {
        let before = codex_login(10, "rt-1");
        let after = codex_login(86_400, "rt-2");
        assert_eq!(refreshed(Tool::Codex, &before, &after), Some(after.clone()));
        assert_eq!(refreshed(Tool::Codex, &before, &before), None);
        assert_eq!(refreshed(Tool::Codex, &before, "{}"), None);

        // Google does not repeat the refresh token on a refresh; the old one still holds.
        let google_before = r#"{"access_token":"a1","refresh_token":"1//r","expiry_date":1}"#;
        let google_after = r#"{"access_token":"a2","expiry_date":2}"#;
        let kept: Value =
            serde_json::from_str(&refreshed(Tool::GeminiCli, google_before, google_after).unwrap())
                .unwrap();
        assert_eq!(kept["access_token"], "a2");
        assert_eq!(kept["refresh_token"], "1//r");
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("llmr-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Resolved, because a tool's `$PWD` is: macOS's temporary directory is under
        // `/var`, a link to `/private/var`.
        dir.canonicalize().unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_call_sees_its_own_key_and_home_and_nothing_else() {
        let root = scratch("env");
        let image = root.join("image");
        // Answers with its working directory, home, key, one inherited variable and prompt.
        fake_tool(
            &image,
            Tool::GeminiCli,
            r#"input=$(cat)
printf '{"response":"%s|%s|%s|%s|%s","stats":{"models":{}}}' "$PWD" "$HOME" "$GEMINI_API_KEY" "${CARGO_MANIFEST_DIR:-unset}" "$input""#,
        );
        // Cargo gives the test process this variable; the tool must not see it, as it must
        // not see LLMR_MASTER_KEY or LLMR_TOKEN.
        assert!(std::env::var("CARGO_MANIFEST_DIR").is_ok());
        let toolbox = Arc::new(Toolbox::new(image, &root.join("data")));
        toolbox.prepare().unwrap();
        let provider = CliProvider::new(
            "gem".into(),
            Tool::GeminiCli,
            "key-for-gem",
            None,
            BTreeSet::from(["gemini-2.5-flash".to_string()]),
            Duration::from_secs(20),
            toolbox.clone(),
        )
        .unwrap();
        let reply = llmr::Provider::chat(
            &provider,
            ChatRequest::new("gemini-2.5-flash", vec![Message::user("hello there")]),
        )
        .await
        .unwrap();
        let text = reply.text();
        let parts: Vec<&str> = text.split('|').collect();
        assert_eq!(parts.len(), 5, "{text}");
        assert_eq!(
            parts[0], parts[1],
            "the working directory is the call's home"
        );
        assert!(parts[1].starts_with(root.join("data/run").to_str().unwrap()));
        assert_eq!(parts[2], "key-for-gem");
        assert_eq!(parts[3], "unset");
        assert_eq!(parts[4], "User: hello there");
        // The call's directory is gone once it answered.
        assert_eq!(std::fs::read_dir(root.join("data/run")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A Codex that answers with the refresh token it was given, then refreshes it: what a
    /// real one does when its access token is about to expire.
    #[cfg(unix)]
    const REFRESHING_CODEX: &str = r#"cat > /dev/null
seen=$(sed 's/.*"refresh_token":"\([^"]*\)".*/\1/' "$CODEX_HOME/auth.json")
printf '{"type":"item.completed","item":{"type":"agent_message","text":"%s"}}\n' "$seen"
printf '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}\n'
sed "s/\"refresh_token\":\"$seen\"/\"refresh_token\":\"$seen+\"/" "$CODEX_HOME/auth.json" > "$CODEX_HOME/next"
mv "$CODEX_HOME/next" "$CODEX_HOME/auth.json""#;

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refreshed_sign_in_is_used_next_and_written_back_unless_replaced() {
        use crate::records::{Provider, ProviderType};
        let root = scratch("refresh");
        let image = root.join("image");
        fake_tool(&image, Tool::Codex, REFRESHING_CODEX);
        let toolbox = Arc::new(Toolbox::new(image, &root.join("data")));
        toolbox.prepare().unwrap();

        let key =
            crate::crypto::MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                .unwrap();
        let db = crate::store::Db::new(crate::store::Store::in_memory(key).unwrap());
        let first = codex_login(86_400, "rt");
        let stored = first.clone();
        db.run_mut(move |store| {
            let (sealed, _) = store.seal_credential(&stored)?;
            store.insert_provider(&Provider {
                id: "cx".into(),
                provider_type: ProviderType::Codex,
                base_url: None,
                reach: None,
                timeout_secs: 20,
                enabled: true,
                credential: Some(sealed),
                credential_hint: Some("subscription".into()),
                created_at: 0,
                updated_at: 0,
            })
        })
        .await
        .unwrap();
        toolbox.attach(db.clone());

        let build = |credential: &str| {
            CliProvider::new(
                "cx".into(),
                Tool::Codex,
                credential,
                None,
                BTreeSet::from(["gpt-5.1".to_string()]),
                Duration::from_secs(20),
                toolbox.clone(),
            )
            .unwrap()
        };
        let ask = |provider: CliProvider| async move {
            llmr::Provider::chat(
                &provider,
                ChatRequest::new("gpt-5.1", vec![Message::user("hi")]),
            )
            .await
            .unwrap()
            .text()
        };
        let stored_now = || {
            let db = db.clone();
            async move {
                db.run(|store| {
                    let provider = store.provider("cx")?;
                    store.open_credential(&provider)
                })
                .await
                .unwrap()
                .unwrap()
            }
        };

        // Each call hands the next the token the last one refreshed to: the same provider,
        // and a gateway rebuilt from the store, which now holds it.
        let provider = build(&first);
        let hi = || ChatRequest::new("gpt-5.1", vec![Message::user("hi")]);
        let answer = llmr::Provider::chat(&provider, hi()).await.unwrap();
        assert_eq!(answer.text(), "rt");
        let answer = llmr::Provider::chat(&provider, hi()).await.unwrap();
        assert_eq!(answer.text(), "rt+");
        assert!(stored_now().await.contains("\"rt++\""));
        assert_eq!(ask(build(&stored_now().await)).await, "rt++");

        // The panel replaces the sign in while a call holding the old one runs. The rebuild
        // takes the panel's, and the call finishing afterwards keeps neither its refresh in
        // memory nor in the store.
        let in_flight = stored_now().await;
        let replaced = codex_login(86_400, "panel");
        let written = replaced.clone();
        db.run_mut(move |store| {
            let mut provider = store.provider("cx")?;
            let (sealed, _) = store.seal_credential(&written)?;
            provider.credential = Some(sealed);
            store.update_provider(&provider)
        })
        .await
        .unwrap();
        let login = toolbox.login("cx", &replaced);
        toolbox
            .keep(&login, in_flight.clone(), codex_login(86_400, "late"))
            .await;
        assert_eq!(login.current(), replaced);
        assert_eq!(stored_now().await, replaced);
        assert_eq!(ask(build(&replaced)).await, "panel");
        assert!(stored_now().await.contains("\"panel+\""));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_that_hangs_is_killed_with_everything_it_started() {
        let root = scratch("hang");
        let image = root.join("image");
        let marker = root.join("child.pid");
        fake_tool(
            &image,
            Tool::ClaudeCode,
            &format!("sleep 60 & echo $! > {}; wait", marker.display()),
        );
        let toolbox = Toolbox::new(image, &root.join("data"));
        toolbox.prepare().unwrap();
        let outcome = toolbox.call(&key_call(Duration::from_millis(500))).await;
        assert!(matches!(outcome, Err(llmr::Error::Timeout { .. })));
        let pid: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The grandchild went with the group. Give the kernel a moment to deliver it.
        let mut gone = false;
        for _ in 0..50 {
            // No signal only asks whether the process exists.
            if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                gone = true;
                break;
            }
            // A killed child stays a zombie until reaped; count that as gone.
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            if status.contains("State:\tZ") {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the tool's child outlived the call");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_call_waits_for_a_slot_and_gives_up_when_none_frees() {
        let root = scratch("slots");
        let image = root.join("image");
        fake_tool(&image, Tool::ClaudeCode, "sleep 2; echo '{}'");
        let toolbox = Arc::new(Toolbox::with_concurrency(image, &root.join("data"), 1));
        toolbox.prepare().unwrap();
        let busy = {
            let toolbox = toolbox.clone();
            tokio::spawn(async move { toolbox.call(&key_call(Duration::from_secs(10))).await })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let waited = toolbox.call(&key_call(Duration::from_millis(300))).await;
        assert!(
            matches!(&waited, Err(llmr::Error::Transient(why)) if why.contains("slots")),
            "{waited:?}"
        );
        assert!(busy.await.unwrap().is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn the_updated_copy_wins_over_the_image_and_reset_goes_back() {
        let root = scratch("source");
        let image = root.join("image");
        fake_tool(&image, Tool::Codex, "true");
        let data = root.join("data");
        let toolbox = Toolbox::new(image, &data);
        toolbox.prepare().unwrap();
        assert_eq!(
            toolbox.installed(Tool::Codex).unwrap().source,
            Source::Image
        );
        assert!(toolbox.installed(Tool::ClaudeCode).is_none());

        fake_tool(&data.join("cli"), Tool::Codex, "true");
        let installed = toolbox.installed(Tool::Codex).unwrap();
        assert_eq!(installed.source, Source::Updated);
        assert_eq!(installed.version.as_deref(), Some("9.9.9"));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert!(runtime.block_on(toolbox.reset(Tool::Codex)).unwrap());
        assert_eq!(
            toolbox.installed(Tool::Codex).unwrap().source,
            Source::Image
        );
        assert!(!runtime.block_on(toolbox.reset(Tool::Codex)).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }
}
