//! What the gateway stores and serves over the management API.
//!
//! These are the shapes a panel reads and writes. The engine never sees them: `gateway.rs`
//! turns them into providers and routers.

use llmr::{ModelCapabilities, Reach};
use serde::{Deserialize, Serialize};

/// The kinds of provider this gateway can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderType {
    /// Anthropic's Messages API.
    Anthropic,
    /// OpenAI's own API.
    Openai,
    /// Google's Gemini API.
    Gemini,
    /// Anything else speaking `/v1/chat/completions`: Ollama, vLLM, LM Studio, Groq,
    /// OpenRouter.
    OpenaiCompatible,
    /// Anthropic's Claude Code, run inside the container.
    ClaudeCode,
    /// OpenAI's Codex, run inside the container.
    Codex,
    /// Google's Gemini CLI, run inside the container.
    GeminiCli,
}

/// What a panel needs to know to offer a provider type in a form.
#[derive(Debug, Clone, Serialize)]
pub struct TypeInfo {
    pub id: ProviderType,
    pub name: &'static str,
    /// How it is reached: `api`, or `cli` for a command line tool inside the container.
    pub transport: &'static str,
    /// Where it answers unless told otherwise. `None` on an `api` type means a base URL is
    /// required; a `cli` type takes one only to point the tool somewhere else.
    pub default_base_url: Option<&'static str>,
    /// Whether `reach` must be given, because nothing on the wire says where it runs.
    pub reach_required: bool,
    #[serde(serialize_with = "reach_name")]
    pub default_reach: Option<Reach>,
    /// `required` or `optional`.
    pub credential: &'static str,
    /// Whether the provider can list the models it serves.
    pub lists_models: bool,
    /// Whether this release knows the provider's published prices.
    pub priced: bool,
}

impl ProviderType {
    pub const ALL: [ProviderType; 7] = [
        ProviderType::Anthropic,
        ProviderType::Openai,
        ProviderType::Gemini,
        ProviderType::OpenaiCompatible,
        ProviderType::ClaudeCode,
        ProviderType::Codex,
        ProviderType::GeminiCli,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderType::Anthropic => "anthropic",
            ProviderType::Openai => "openai",
            ProviderType::Gemini => "gemini",
            ProviderType::OpenaiCompatible => "openai-compatible",
            ProviderType::ClaudeCode => "claude-code",
            ProviderType::Codex => "codex",
            ProviderType::GeminiCli => "gemini-cli",
        }
    }

    /// The command line tool this type runs, for a `cli` type.
    pub fn tool(self) -> Option<crate::cli::Tool> {
        match self {
            ProviderType::ClaudeCode => Some(crate::cli::Tool::ClaudeCode),
            ProviderType::Codex => Some(crate::cli::Tool::Codex),
            ProviderType::GeminiCli => Some(crate::cli::Tool::GeminiCli),
            _ => None,
        }
    }

    pub fn parse(text: &str) -> Option<ProviderType> {
        ProviderType::ALL.into_iter().find(|t| t.as_str() == text)
    }

    pub fn info(self) -> TypeInfo {
        match self {
            ProviderType::Anthropic => TypeInfo {
                id: self,
                name: "Anthropic",
                transport: "api",
                default_base_url: Some(llmr::providers::anthropic::api::DEFAULT_BASE_URL),
                reach_required: false,
                default_reach: Some(Reach::FirstPartyApi),
                credential: "required",
                lists_models: true,
                priced: true,
            },
            ProviderType::Openai => TypeInfo {
                id: self,
                name: "OpenAI",
                transport: "api",
                default_base_url: Some(OPENAI_BASE_URL),
                reach_required: false,
                default_reach: Some(Reach::FirstPartyApi),
                credential: "required",
                lists_models: true,
                priced: true,
            },
            ProviderType::Gemini => TypeInfo {
                id: self,
                name: "Google Gemini",
                transport: "api",
                default_base_url: Some(llmr::providers::gemini::api::DEFAULT_BASE_URL),
                reach_required: false,
                default_reach: Some(Reach::FirstPartyApi),
                credential: "required",
                lists_models: true,
                priced: true,
            },
            ProviderType::OpenaiCompatible => TypeInfo {
                id: self,
                name: "OpenAI-compatible endpoint",
                transport: "api",
                default_base_url: None,
                reach_required: true,
                default_reach: None,
                credential: "optional",
                lists_models: true,
                priced: false,
            },
            // A tool calls its vendor's API with an API key, so the vendor's prices apply.
            ProviderType::ClaudeCode | ProviderType::Codex | ProviderType::GeminiCli => TypeInfo {
                id: self,
                name: self.tool().map_or("", crate::cli::Tool::title),
                transport: "cli",
                default_base_url: None,
                reach_required: false,
                default_reach: Some(Reach::LocalCli),
                credential: "required",
                lists_models: false,
                priced: true,
            },
        }
    }
}

/// A reach as the API writes it everywhere: `first-party-api`, `self-hosted`.
fn reach_name<S: serde::Serializer>(
    reach: &Option<Reach>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match reach {
        Some(reach) => serializer.serialize_some(reach.as_str()),
        None => serializer.serialize_none(),
    }
}

pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// One stored provider. The credential never leaves the store in the clear; the API shows
/// only whether there is one and its last four characters.
#[derive(Debug, Clone)]
pub struct Provider {
    pub id: String,
    pub provider_type: ProviderType,
    pub base_url: Option<String>,
    pub reach: Option<Reach>,
    pub timeout_secs: u64,
    pub enabled: bool,
    /// Sealed with the master key.
    pub credential: Option<Vec<u8>>,
    pub credential_hint: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Provider {
    /// Where the data goes: what was set, else the type's default.
    pub fn effective_reach(&self) -> Option<Reach> {
        self.reach.or(self.provider_type.info().default_reach)
    }

    /// Where it answers: what was set, else the type's default.
    pub fn effective_base_url(&self) -> Option<String> {
        self.base_url
            .clone()
            .or_else(|| self.provider_type.info().default_base_url.map(String::from))
    }

    /// The provider as the API shows it.
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": self.provider_type,
            "transport": self.provider_type.info().transport,
            "base_url": self.effective_base_url(),
            "reach": self.effective_reach().map(Reach::as_str),
            "timeout_secs": self.timeout_secs,
            "enabled": self.enabled,
            "credential": self.credential_hint.as_ref().map(|hint| format!("…{hint}")),
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }
}

/// What a model can do. Written by a panel for a model the release does not know, or to
/// correct one it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    #[serde(default)]
    pub context_window: u32,
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
}

impl Capabilities {
    pub fn from_engine(have: &ModelCapabilities) -> Self {
        Self {
            context_window: have.context_window,
            max_output: have.max_output,
            tools: have.tools,
            structured_output: have.structured_output,
            prompt_caching: have.prompt_caching,
            thinking: have.thinking,
            images: have.images,
            streaming: have.streaming,
        }
    }
}

/// A model a provider serves, and whether this gateway serves it.
#[derive(Debug, Clone)]
pub struct Model {
    pub provider_id: String,
    pub model_id: String,
    pub enabled: bool,
    /// Set when the panel said what the model can do; otherwise the release's table answers.
    pub capabilities: Option<Capabilities>,
    pub updated_at: i64,
}

/// Which route a router reaches for first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Order {
    #[default]
    AsListed,
    Cheapest,
    Healthiest,
}

/// A name clients ask for, and the routes behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// `provider/model`, in the order to try them.
    pub routes: Vec<String>,
    #[serde(default)]
    pub order: Order,
    /// Only self hosted routes may serve this name.
    #[serde(default)]
    pub on_device: bool,
    /// Attempts per route, the first included.
    #[serde(default = "default_attempts")]
    pub retry_attempts: u32,
    /// Skip a route that keeps failing, for a while.
    #[serde(default = "default_true")]
    pub breaker: bool,
    /// Give up on the whole request after this long.
    #[serde(default)]
    pub deadline_secs: Option<u64>,
}

fn default_attempts() -> u32 {
    2
}

fn default_true() -> bool {
    true
}

impl RouteSpec {
    /// What is wrong with it, if anything, before it is stored.
    pub fn check(&self) -> Result<(), String> {
        if self.routes.is_empty() {
            return Err("routes must list at least one provider/model".into());
        }
        for route in &self.routes {
            match split_route(route) {
                Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {}
                _ => return Err(format!("route {route:?} is not written provider/model")),
            }
        }
        if self.retry_attempts == 0 {
            return Err("retry_attempts counts the first try, so it is at least 1".into());
        }
        Ok(())
    }
}

/// `provider/model`, split at the first slash.
///
/// The first, because model names carry slashes of their own (`meta-llama/llama-3.1-8b`
/// through OpenRouter) and provider ids may not.
pub fn split_route(route: &str) -> Option<(&str, &str)> {
    route.split_once('/')
}

/// Whether an id can name a provider or a route set: short, and safe in a URL path.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_splits_at_the_first_slash_only() {
        assert_eq!(
            split_route("openrouter/meta-llama/llama-3.1-8b"),
            Some(("openrouter", "meta-llama/llama-3.1-8b"))
        );
        assert_eq!(split_route("no-slash"), None);
    }

    #[test]
    fn a_route_set_with_defaults_reads_and_checks() {
        let spec: RouteSpec =
            serde_json::from_value(serde_json::json!({ "routes": ["a/b"] })).unwrap();
        assert_eq!(spec.retry_attempts, 2);
        assert!(spec.breaker);
        assert_eq!(spec.order, Order::AsListed);
        spec.check().unwrap();
    }

    #[test]
    fn a_route_set_is_refused_with_a_reason() {
        for bad in [
            serde_json::json!({ "routes": [] }),
            serde_json::json!({ "routes": ["no-slash"] }),
            serde_json::json!({ "routes": ["/model"] }),
            serde_json::json!({ "routes": ["a/b"], "retry_attempts": 0 }),
        ] {
            let spec: RouteSpec = serde_json::from_value(bad.clone()).unwrap();
            assert!(spec.check().is_err(), "{bad}");
        }
        // A misspelt key is refused rather than ignored.
        assert!(serde_json::from_value::<RouteSpec>(
            serde_json::json!({ "routes": ["a/b"], "on_devcie": true })
        )
        .is_err());
    }

    #[test]
    fn ids_are_safe_in_a_path() {
        assert!(valid_id("anthropic-main"));
        assert!(valid_id("ollama_1.local"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("has space"));
    }

    #[test]
    fn every_type_reads_back_from_its_name() {
        for t in ProviderType::ALL {
            assert_eq!(ProviderType::parse(t.as_str()), Some(t));
        }
    }
}
