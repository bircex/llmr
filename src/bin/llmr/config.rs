//! What the gateway is told to serve, read from one TOML file.
//!
//! Two kinds of thing are configured, and they are kept apart on purpose:
//!
//! * **Providers** are where a prompt can go and whose credential pays: a kind, a base URL,
//!   the name of the environment variable holding the key, and where the data ends up.
//! * **Models** are the names a client asks for. Each one is an ordered list of
//!   `provider/model` routes and the policy the router applies to them.
//!
//! A client never names a vendor. It asks for `default` or `private`, and which vendor
//! answers is this file's decision, changed without touching the client.
//!
//! Keys are never written here. The file names an environment variable and the value is
//! read at startup, so the file can be committed and mounted read only.

use llmr::registry::{Entry, Registry};
use llmr::Reach;
use serde::Deserialize;
use std::collections::BTreeSet;

/// The whole file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// How the server listens and who may call it.
    #[serde(default)]
    pub server: ServerConfig,
    /// Where prompts can go.
    #[serde(default, rename = "provider")]
    pub providers: Vec<ProviderConfig>,
    /// The names clients ask for.
    #[serde(default, rename = "model")]
    pub models: Vec<ModelConfig>,
}

/// How the server listens and who may call it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address and port to bind.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Whether callers must present a key.
    #[serde(default)]
    pub auth: AuthMode,
    /// The environment variable holding the keys callers may present, comma separated.
    #[serde(default = "default_api_keys_env")]
    pub api_keys_env: String,
    /// The largest request body accepted, in megabytes. Images arrive inline, so this is
    /// larger than a text-only gateway would need.
    #[serde(default = "default_max_body_mb")]
    pub max_body_mb: usize,
    /// Whether a client may ask for `provider/model` directly, bypassing the named models.
    #[serde(default = "default_true")]
    pub allow_direct: bool,
    /// Whether to ask every route if it is reachable at startup.
    ///
    /// Free by construction: a provider answers from its model list, never from a billable
    /// call. With a breaker, a route that is denied is rested rather than tried first.
    #[serde(default = "default_true")]
    pub preflight: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            auth: AuthMode::default(),
            api_keys_env: default_api_keys_env(),
            max_body_mb: default_max_body_mb(),
            allow_direct: true,
            preflight: true,
        }
    }
}

/// Whether callers must present a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// A key from the configured variable is required. The default, and the server refuses
    /// to start if the variable is empty rather than quietly serving everybody.
    #[default]
    Keys,
    /// Anybody who can reach the port may spend the providers' money. Only for a network
    /// nobody else is on.
    None,
}

/// Which protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Anthropic's Messages API.
    Anthropic,
    /// OpenAI's own API.
    Openai,
    /// Google's Gemini API.
    Gemini,
    /// Anything else speaking `/v1/chat/completions`: Ollama, vLLM, LM Studio, Groq,
    /// OpenRouter. The reach must be given, because nothing on the wire says where it runs.
    OpenaiCompatible,
}

/// One place a prompt can go.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// The name routes use, as in `anthropic/claude-sonnet-5`.
    pub id: String,
    /// The protocol.
    pub kind: ProviderKind,
    /// Where it answers. Defaults to the vendor's own endpoint for the vendor kinds; required
    /// for `openai-compatible`.
    pub base_url: Option<String>,
    /// The environment variable holding the key. Defaults to the vendor's usual name for
    /// the vendor kinds; optional for `openai-compatible`, where a local server usually has
    /// none.
    pub api_key_env: Option<String>,
    /// Where the data goes. Required for `openai-compatible`; defaults to
    /// `first-party-api` for the vendor kinds.
    pub reach: Option<String>,
    /// How long one call may take before it is given up on, in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Whether to start from the model table this release ships for the vendor kinds.
    #[serde(default = "default_true")]
    pub shipped_models: bool,
    /// Models this provider serves, and what each can do.
    ///
    /// For a vendor kind these add to or replace rows of the shipped table. For
    /// `openai-compatible` they are the whole table: a route to a model not listed here is
    /// reported as unusable at startup, because nothing is known about what it can do.
    #[serde(default, rename = "model")]
    pub models: Vec<ModelRow>,
}

/// What one model can do, written by whoever runs the gateway.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRow {
    /// The name the provider uses.
    pub id: String,
    /// How many tokens fit in one request. Zero when unknown.
    #[serde(default)]
    pub context_window: u32,
    /// The most tokens it will produce. Zero when unknown.
    #[serde(default)]
    pub max_output: u32,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub structured_output: bool,
    #[serde(default)]
    pub prompt_caching: bool,
    #[serde(default)]
    pub thinking: bool,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub streaming: bool,
    /// Where these facts came from.
    #[serde(default = "default_source")]
    pub source: String,
    /// When somebody last checked them, as `YYYY-MM-DD`.
    #[serde(default)]
    pub verified_at: String,
}

/// Which route the router reaches for first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrderConfig {
    /// The order written.
    #[default]
    AsListed,
    /// Lowest published rate first; an unpriced route last.
    Cheapest,
    /// Fewest recent failures first.
    Healthiest,
}

/// A name clients ask for, and the routes behind it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// What a client puts in `model`.
    pub name: String,
    /// `provider/model`, in the order to try them.
    pub routes: Vec<String>,
    /// Which route to reach for first.
    #[serde(default)]
    pub order: OrderConfig,
    /// Only self hosted routes may serve this name. A floor, never relaxed by a fallback.
    #[serde(default)]
    pub on_device: bool,
    /// Attempts per route for a failure worth repeating. One means no retry.
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
    /// Skip a route that has been failing, for a while, rather than waiting on it in every
    /// request.
    #[serde(default = "default_true")]
    pub breaker: bool,
    /// Give up on the whole request after this many seconds, however many routes are left.
    pub deadline_secs: Option<u64>,
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_api_keys_env() -> String {
    "LLMR_API_KEYS".into()
}
fn default_max_body_mb() -> usize {
    32
}
fn default_true() -> bool {
    true
}
fn default_timeout_secs() -> u64 {
    120
}
fn default_retry_attempts() -> u32 {
    2
}
fn default_source() -> String {
    "gateway configuration".into()
}

impl Config {
    /// Reads and checks a configuration.
    ///
    /// Everything that can be checked without the environment or the network is checked
    /// here, so `llmr check` catches a typo before a deploy does.
    ///
    /// # Errors
    ///
    /// A message naming what is wrong and where.
    pub fn parse(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.check()?;
        Ok(config)
    }

    fn check(&self) -> Result<(), String> {
        let mut ids = BTreeSet::new();
        for provider in &self.providers {
            if provider.id.is_empty() || provider.id.contains('/') {
                return Err(format!(
                    "provider id {:?} must be non empty and contain no '/', because routes are \
                     written provider/model",
                    provider.id
                ));
            }
            if !ids.insert(provider.id.as_str()) {
                return Err(format!("provider {} is defined twice", provider.id));
            }
            if provider.kind == ProviderKind::OpenaiCompatible {
                if provider.base_url.is_none() {
                    return Err(format!(
                        "provider {}: openai-compatible needs a base_url",
                        provider.id
                    ));
                }
                if provider.reach.is_none() {
                    return Err(format!(
                        "provider {}: openai-compatible needs a reach. The same protocol is \
                         spoken by a model on your own hardware and by a hosted API, and \
                         which one this is decides where prompts may go",
                        provider.id
                    ));
                }
            }
            if let Some(reach) = &provider.reach {
                if Reach::parse(reach).is_none() {
                    return Err(format!(
                        "provider {}: unknown reach {reach:?}. One of first-party-api, \
                         cloud-partner, private-endpoint, self-hosted",
                        provider.id
                    ));
                }
            }
            if provider.timeout_secs == 0 {
                return Err(format!(
                    "provider {}: timeout_secs must be above zero",
                    provider.id
                ));
            }
        }

        let mut names = BTreeSet::new();
        for model in &self.models {
            if model.name.is_empty() {
                return Err("a model has an empty name".into());
            }
            if !names.insert(model.name.as_str()) {
                return Err(format!("model {} is defined twice", model.name));
            }
            if model.routes.is_empty() {
                return Err(format!("model {} has no routes", model.name));
            }
            if model.retry_attempts == 0 {
                return Err(format!(
                    "model {}: retry_attempts counts the first try, so it is at least 1",
                    model.name
                ));
            }
            for route in &model.routes {
                let (provider, target) = split_route(route).ok_or_else(|| {
                    format!(
                        "model {}: route {route:?} is not written provider/model",
                        model.name
                    )
                })?;
                if !ids.contains(provider) {
                    return Err(format!(
                        "model {}: route {route} names provider {provider}, which is not \
                         defined",
                        model.name
                    ));
                }
                if target.is_empty() {
                    return Err(format!(
                        "model {}: route {route} names no model",
                        model.name
                    ));
                }
            }
        }

        if self.models.is_empty() && !self.server.allow_direct {
            return Err(
                "no models are defined and allow_direct is off, so nothing could be served".into(),
            );
        }
        Ok(())
    }
}

impl ProviderConfig {
    /// Where the data goes.
    pub fn reach(&self) -> Reach {
        self.reach
            .as_deref()
            .and_then(Reach::parse)
            .unwrap_or(Reach::FirstPartyApi)
    }

    /// The configured rows, laid over a starting table.
    pub fn registry(&self, shipped: Option<Registry>) -> Registry {
        let mut registry = match shipped {
            Some(shipped) if self.shipped_models => shipped,
            _ => Registry::empty(self.id.clone(), self.reach()),
        };
        // The reach belongs to the table. A vendor table reached through a base URL the
        // operator says is somewhere else takes the operator's word for it.
        registry.reach = self.reach();
        for row in &self.models {
            let mut entry = Entry::new(row.id.clone(), row.source.clone(), row.verified_at.clone())
                .with_window(row.context_window, row.max_output)
                .able_to(
                    row.tools,
                    row.structured_output,
                    row.prompt_caching,
                    row.thinking,
                );
            if row.images {
                entry = entry.with_images();
            }
            if row.streaming {
                entry = entry.with_streaming();
            }
            registry.models.insert(row.id.clone(), entry);
        }
        registry
    }
}

/// `provider/model`, split at the first slash.
///
/// The first, because model names carry slashes of their own (`meta-llama/llama-3.1-8b`
/// through OpenRouter) and provider ids may not.
pub fn split_route(route: &str) -> Option<(&str, &str)> {
    route.split_once('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[server]
listen = "127.0.0.1:9000"

[[provider]]
id = "anthropic"
kind = "anthropic"

[[provider]]
id = "local"
kind = "openai-compatible"
base_url = "http://ollama:11434/v1"
reach = "self-hosted"

[[provider.model]]
id = "llama3.1:8b"
context_window = 128000
max_output = 8192
tools = true
streaming = true

[[model]]
name = "default"
routes = ["anthropic/claude-sonnet-5", "local/llama3.1:8b"]
order = "healthiest"

[[model]]
name = "private"
routes = ["local/llama3.1:8b"]
on_device = true
"#;

    #[test]
    fn the_example_shipped_in_the_image_reads() {
        // The image starts from it, so a mistake in it is a container that will not start.
        Config::parse(include_str!("../../../llmr.example.toml")).expect("the example parses");
    }

    #[test]
    fn a_complete_file_reads() {
        let config = Config::parse(GOOD).expect("parses");
        assert_eq!(config.server.listen, "127.0.0.1:9000");
        assert_eq!(config.server.auth, AuthMode::Keys);
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.models[0].order, OrderConfig::Healthiest);
        assert_eq!(config.models[0].retry_attempts, 2);
        assert!(config.models[1].on_device);
    }

    #[test]
    fn an_inline_row_becomes_a_capability_with_the_providers_reach() {
        let config = Config::parse(GOOD).expect("parses");
        let registry = config.providers[1].registry(None);
        let have = registry
            .capabilities(&"llama3.1:8b".into())
            .expect("listed");
        assert!(have.tools && have.streaming && !have.images);
        assert_eq!(have.reach, Reach::SelfHosted);
    }

    #[test]
    fn a_compatible_endpoint_without_a_reach_is_refused() {
        let text = GOOD.replace("reach = \"self-hosted\"\n", "");
        let error = Config::parse(&text).expect_err("no reach");
        assert!(error.contains("needs a reach"), "{error}");
    }

    #[test]
    fn a_route_to_an_undefined_provider_is_refused() {
        let text = GOOD.replace("anthropic/claude-sonnet-5", "anthropc/claude-sonnet-5");
        let error = Config::parse(&text).expect_err("typo");
        assert!(error.contains("anthropc"), "{error}");
    }

    #[test]
    fn a_misspelt_key_is_refused_rather_than_ignored() {
        let text = GOOD.replace("on_device = true", "on_devcie = true");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn a_route_splits_at_the_first_slash_only() {
        assert_eq!(
            split_route("openrouter/meta-llama/llama-3.1-8b"),
            Some(("openrouter", "meta-llama/llama-3.1-8b"))
        );
        assert_eq!(split_route("no-slash"), None);
    }

    #[test]
    fn a_provider_defined_twice_is_refused() {
        let text = format!("{GOOD}\n[[provider]]\nid = \"anthropic\"\nkind = \"anthropic\"\n");
        assert!(Config::parse(&text).is_err());
    }
}
