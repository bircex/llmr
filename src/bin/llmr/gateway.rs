//! Providers and routers, built once from the configuration and shared by every request.
//!
//! Everything here is immutable after startup except the routers' own health counters,
//! which are atomics inside `llmr::Router`. There is no lock on the request path apart from
//! the one guarding the cache of direct routes, and it is never held across an await.

use crate::config::{Config, ModelConfig, OrderConfig, ProviderConfig, ProviderKind};
use async_trait::async_trait;
use llmr::chat::EventStream;
use llmr::providers::api::ApiProvider;
use llmr::providers::{anthropic, gemini, openai};
use llmr::transport::{HttpTransport, Reqwest};
use llmr::{
    Access, Breaker, ChatRequest, ChatResponse, ModelCapabilities, ModelId, Order, PriceBook,
    Provider, Retry, Route, Router, Secret,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A provider under the name the configuration gave it.
///
/// The crate's providers name themselves after their protocol, so two Anthropic accounts
/// would both be `anthropic` in every route name and every log line. This puts the
/// operator's id in front and changes nothing else.
struct Named {
    id: String,
    inner: Arc<dyn Provider>,
}

#[async_trait]
impl Provider for Named {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        self.inner.capabilities(model)
    }

    async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
        self.inner.chat(request).await
    }

    async fn stream(&self, request: ChatRequest) -> llmr::Result<EventStream<'_>> {
        self.inner.stream(request).await
    }

    async fn catalogue(&self) -> llmr::Result<Vec<ModelId>> {
        self.inner.catalogue().await
    }

    async fn validate(&self, model: &ModelId) -> Access {
        self.inner.validate(model).await
    }

    fn subscription(&self) -> Option<&str> {
        self.inner.subscription()
    }
}

/// One configured provider.
struct Built {
    provider: Arc<dyn Provider>,
    /// The vendor's published prices, when the endpoint is the vendor's own. A price book
    /// for OpenAI's API says nothing about what Groq charges for the same shape.
    prices: Option<Arc<PriceBook>>,
}

/// A name a client may ask for, and the router behind it.
pub struct Served {
    pub name: String,
    pub router: Arc<Router>,
    /// Only self hosted routes may serve it.
    pub on_device: bool,
}

/// Everything a request needs to find a provider.
pub struct Gateway {
    providers: BTreeMap<String, Built>,
    served: BTreeMap<String, Served>,
    allow_direct: bool,
    /// Routers for `provider/model` asked for directly, kept so their breakers remember.
    direct: Mutex<HashMap<String, Arc<Router>>>,
}

impl Gateway {
    /// Builds every provider and router the configuration describes.
    ///
    /// # Errors
    ///
    /// A message naming the provider whose key is missing or whose client could not be
    /// built. Refused at startup rather than on the first request, so a deploy with a
    /// missing secret fails where somebody is watching.
    pub fn build(config: &Config) -> Result<Gateway, String> {
        let mut providers = BTreeMap::new();
        for provider in &config.providers {
            providers.insert(provider.id.clone(), build_provider(provider)?);
        }

        let mut served = BTreeMap::new();
        for model in &config.models {
            served.insert(model.name.clone(), build_served(model, &providers)?);
        }

        Ok(Gateway {
            providers,
            served,
            allow_direct: config.server.allow_direct,
            direct: Mutex::new(HashMap::new()),
        })
    }

    /// The configured names, in name order.
    pub fn served(&self) -> impl Iterator<Item = &Served> {
        self.served.values()
    }

    /// The router for a name a client asked for, and whether it must stay on device.
    ///
    /// A configured name first. Otherwise, when direct addressing is on, `provider/model`
    /// becomes a one route router, kept so its breaker remembers between requests. A direct
    /// route is only kept for a model the provider knows, so a client cannot grow the cache
    /// by asking for names that do not exist.
    pub fn resolve(&self, model: &str) -> Option<(Arc<Router>, bool)> {
        if let Some(served) = self.served.get(model) {
            return Some((served.router.clone(), served.on_device));
        }
        if !self.allow_direct {
            return None;
        }
        let (provider_id, target) = crate::config::split_route(model)?;
        let built = self.providers.get(provider_id)?;
        let target = ModelId::from(target);
        built.provider.capabilities(&target)?;

        let mut direct = self.direct.lock().ok()?;
        let router = direct
            .entry(model.to_string())
            .or_insert_with(|| {
                Arc::new(
                    Router::new(vec![route(built, target)])
                        .retrying(Retry::new(2))
                        .breaking(Breaker::default()),
                )
            })
            .clone();
        Some((router, false))
    }
}

impl Gateway {
    /// A gateway over routers built by hand, for tests that need a provider of their own.
    #[cfg(test)]
    pub fn from_routers(served: Vec<(&str, Router, bool)>) -> Gateway {
        Gateway {
            providers: BTreeMap::new(),
            served: served
                .into_iter()
                .map(|(name, router, on_device)| {
                    (
                        name.to_string(),
                        Served {
                            name: name.to_string(),
                            router: Arc::new(router),
                            on_device,
                        },
                    )
                })
                .collect(),
            allow_direct: false,
            direct: Mutex::new(HashMap::new()),
        }
    }
}

fn build_provider(config: &ProviderConfig) -> Result<Built, String> {
    let transport: Arc<dyn HttpTransport> = Arc::new(
        Reqwest::new(Duration::from_secs(config.timeout_secs))
            .map_err(|e| format!("provider {}: {e}", config.id))?,
    );

    let (env, default_url) = match config.kind {
        ProviderKind::Anthropic => (
            Some("ANTHROPIC_API_KEY"),
            Some(anthropic::api::DEFAULT_BASE_URL),
        ),
        ProviderKind::Openai => (Some("OPENAI_API_KEY"), Some("https://api.openai.com/v1")),
        ProviderKind::Gemini => (Some("GEMINI_API_KEY"), Some(gemini::api::DEFAULT_BASE_URL)),
        ProviderKind::OpenaiCompatible => (None, None),
    };
    let key = match config.api_key_env.as_deref().or(env) {
        Some(variable) => Secret::from_env("provider-api-key", variable).map_err(|_| {
            format!(
                "provider {}: the environment variable {variable} is unset or empty",
                config.id
            )
        })?,
        // A local server with no key. Nothing is sent that means anything.
        None => Secret::new("provider-api-key", ""),
    };
    let base_url = config
        .base_url
        .clone()
        .or(default_url.map(String::from))
        .ok_or_else(|| format!("provider {}: no base_url", config.id))?;
    // Shipped prices hold for the vendor's own endpoint and nowhere else.
    let own_endpoint = config.base_url.is_none();
    let reach = config.reach();

    let (inner, prices): (Arc<dyn Provider>, Option<PriceBook>) = match config.kind {
        ProviderKind::Anthropic => (
            Arc::new(ApiProvider::new(
                anthropic::api::Messages,
                base_url,
                transport,
                key,
                reach,
                Arc::new(config.registry(Some(anthropic::api::shipped_registry()))),
            )),
            own_endpoint.then(anthropic::api::shipped_prices),
        ),
        ProviderKind::Gemini => (
            Arc::new(ApiProvider::new(
                gemini::api::GenerateContent,
                base_url,
                transport,
                key,
                reach,
                Arc::new(config.registry(Some(gemini::api::shipped_registry()))),
            )),
            own_endpoint.then(gemini::api::shipped_prices),
        ),
        ProviderKind::Openai | ProviderKind::OpenaiCompatible => {
            let shipped = (config.kind == ProviderKind::Openai).then(openai::api::shipped_registry);
            (
                Arc::new(openai::api::at(
                    // The protocol wants a name that lives as long as the program, and this
                    // one does: it is read once at startup and never freed. `Named` below
                    // is what a report shows.
                    Box::leak(config.id.clone().into_boxed_str()),
                    base_url,
                    transport,
                    key,
                    reach,
                    Arc::new(config.registry(shipped)),
                )),
                (config.kind == ProviderKind::Openai && own_endpoint)
                    .then(openai::api::shipped_prices),
            )
        }
    };

    Ok(Built {
        provider: Arc::new(Named {
            id: config.id.clone(),
            inner,
        }),
        prices: prices.map(Arc::new),
    })
}

fn route(built: &Built, model: ModelId) -> Route {
    let route = Route::new(built.provider.clone(), model);
    match &built.prices {
        Some(prices) => route.priced_by(prices.clone()),
        None => route,
    }
}

fn build_served(
    model: &ModelConfig,
    providers: &BTreeMap<String, Built>,
) -> Result<Served, String> {
    let mut routes = Vec::new();
    for written in &model.routes {
        let (provider_id, target) = crate::config::split_route(written)
            .ok_or_else(|| format!("model {}: bad route {written}", model.name))?;
        let built = providers
            .get(provider_id)
            .ok_or_else(|| format!("model {}: no provider {provider_id}", model.name))?;
        routes.push(route(built, ModelId::from(target)));
    }

    let mut router = Router::new(routes)
        .ordering(match model.order {
            OrderConfig::AsListed => Order::AsListed,
            OrderConfig::Cheapest => Order::Cheapest,
            OrderConfig::Healthiest => Order::Healthiest,
        })
        .retrying(Retry::new(model.retry_attempts));
    if model.breaker {
        router = router.breaking(Breaker::default());
    }
    if let Some(seconds) = model.deadline_secs {
        router = router.within_deadline(Duration::from_secs(seconds));
    }

    Ok(Served {
        name: model.name.clone(),
        router: Arc::new(router),
        on_device: model.on_device,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[[provider]]
id = "local"
kind = "openai-compatible"
base_url = "http://127.0.0.1:9/v1"
reach = "self-hosted"

[[provider.model]]
id = "small"
streaming = true

[[model]]
name = "default"
routes = ["local/small"]
"#;

    #[test]
    fn a_configured_name_and_a_direct_route_both_resolve() {
        let config = Config::parse(CONFIG).expect("parses");
        let gateway = Gateway::build(&config).expect("builds");
        assert!(gateway.resolve("default").is_some());
        assert!(gateway.resolve("local/small").is_some());
        // The same router both times, so its breaker remembers.
        let (first, _) = gateway.resolve("local/small").expect("direct");
        let (second, _) = gateway.resolve("local/small").expect("direct");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn a_model_nobody_configured_does_not_resolve_or_grow_the_cache() {
        let config = Config::parse(CONFIG).expect("parses");
        let gateway = Gateway::build(&config).expect("builds");
        assert!(gateway.resolve("local/unknown").is_none());
        assert!(gateway.resolve("nowhere/small").is_none());
        assert!(gateway.resolve("nothing").is_none());
        assert!(gateway.direct.lock().map(|d| d.is_empty()).unwrap_or(false));
    }

    #[test]
    fn a_route_is_named_after_the_configured_provider() {
        let config = Config::parse(CONFIG).expect("parses");
        let gateway = Gateway::build(&config).expect("builds");
        let served = gateway.served().next().expect("one");
        let names: Vec<String> = served.router.routes().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["local/small".to_string()]);
    }

    #[test]
    fn a_missing_key_fails_at_startup_and_names_the_variable() {
        let text = format!(
            "{CONFIG}\n[[provider]]\nid = \"vendor\"\nkind = \"openai\"\napi_key_env = \"LLMR_TEST_SURELY_UNSET\"\n"
        );
        let config = Config::parse(&text).expect("parses");
        let error = Gateway::build(&config).err().expect("no key");
        assert!(error.contains("LLMR_TEST_SURELY_UNSET"), "{error}");
    }
}
