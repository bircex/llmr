//! Providers and routers, built from what the store holds and swapped in whole on a change.
//!
//! A [`Gateway`] is immutable once built. A change through the management API reads the
//! store again, builds a new one, and replaces the old one in [`Live`]; requests already in
//! flight finish on the gateway they started on. The routers' health counters start again
//! with each build, which is the price of never locking the request path.

use crate::records::{
    split_route, Capabilities, Model, Order as OrderSpec, Provider, ProviderType, RouteSpec,
};
use crate::store::{Store, StoreResult};
use async_trait::async_trait;
use llmr::chat::EventStream;
use llmr::providers::api::ApiProvider;
use llmr::providers::{anthropic, gemini, openai};
use llmr::registry::{Entry, Registry};
use llmr::transport::{HttpTransport, Reqwest};
use llmr::{
    Access, Breaker, ChatRequest, ChatResponse, ModelCapabilities, ModelId, Order, PriceBook,
    Retry, Route, Router, Secret,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// A provider under the id it was stored with.
///
/// The engine's providers name themselves after their protocol, so two Anthropic accounts
/// would both be `anthropic` in every route name and every log line. This puts the stored id
/// in front and changes nothing else.
struct Named {
    id: String,
    inner: Arc<dyn llmr::Provider>,
}

#[async_trait]
impl llmr::Provider for Named {
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
}

/// One provider, ready to call.
pub struct Built {
    pub provider: Arc<dyn llmr::Provider>,
    pub provider_type: ProviderType,
    pub reach: llmr::Reach,
    /// The vendor's published prices, when the endpoint is the vendor's own. A price book
    /// for OpenAI's API says nothing about what Groq charges for the same shape.
    pub prices: Option<Arc<PriceBook>>,
}

/// Everything the store holds, read in one go, with credentials opened.
pub struct Snapshot {
    pub providers: Vec<(Provider, Option<String>)>,
    pub models: Vec<Model>,
    pub routes: Vec<(String, RouteSpec)>,
}

impl Snapshot {
    /// Reads the whole store.
    ///
    /// # Errors
    ///
    /// When the store cannot be read or a credential cannot be opened.
    pub fn read(store: &Store) -> StoreResult<Snapshot> {
        let mut providers = Vec::new();
        for provider in store.providers()? {
            let credential = store.open_credential(&provider)?;
            providers.push((provider, credential));
        }
        Ok(Snapshot {
            providers,
            models: store.models(None)?,
            routes: store.routes()?,
        })
    }
}

/// A name a client may ask for, and the router behind it.
pub struct Served {
    pub name: String,
    pub router: Arc<Router>,
    /// Only self hosted routes may serve it.
    pub on_device: bool,
    /// Routes in the set that cannot be used right now, and why.
    pub unavailable: Vec<(String, String)>,
}

/// What a name a client sent turned out to be.
pub enum Resolution {
    /// A router to send it to, and whether the data must stay on device.
    Found(Arc<Router>, bool),
    /// A model that exists and is switched off, with the reason to give the client.
    NotEnabled(String),
    /// Nothing by that name.
    Unknown,
}

/// Everything a request needs to find a provider.
pub struct Gateway {
    providers: BTreeMap<String, Built>,
    /// `provider/model` for every model that is enabled on an enabled provider.
    enabled: BTreeSet<String>,
    served: BTreeMap<String, Served>,
    /// Providers that could not be built, and why, for the status endpoint.
    problems: Vec<(String, String)>,
    /// Routers for `provider/model` asked for directly, kept so their breakers remember.
    direct: Mutex<HashMap<String, Arc<Router>>>,
}

impl Gateway {
    /// A gateway serving nothing.
    #[cfg(test)]
    pub fn empty() -> Gateway {
        Gateway {
            providers: BTreeMap::new(),
            enabled: BTreeSet::new(),
            served: BTreeMap::new(),
            problems: Vec::new(),
            direct: Mutex::new(HashMap::new()),
        }
    }

    /// Builds every enabled provider and every route set.
    ///
    /// Never fails as a whole. A provider that cannot be built is left out and reported in
    /// [`Gateway::problems`]; a route that cannot be used is left out of its set and reported
    /// in [`Served::unavailable`]. One bad row must not take every other name offline.
    pub fn build(snapshot: &Snapshot) -> Gateway {
        let mut providers = BTreeMap::new();
        let mut problems = Vec::new();

        for (record, credential) in &snapshot.providers {
            if !record.enabled {
                continue;
            }
            let rows: Vec<&Model> = snapshot
                .models
                .iter()
                .filter(|m| m.provider_id == record.id)
                .collect();
            match build_provider(record, credential.as_deref(), &rows) {
                Ok(built) => {
                    providers.insert(record.id.clone(), built);
                }
                Err(why) => problems.push((record.id.clone(), why)),
            }
        }

        let enabled: BTreeSet<String> = snapshot
            .models
            .iter()
            .filter(|m| m.enabled && providers.contains_key(&m.provider_id))
            .map(|m| format!("{}/{}", m.provider_id, m.model_id))
            .collect();

        let mut served = BTreeMap::new();
        for (name, spec) in &snapshot.routes {
            served.insert(
                name.clone(),
                build_served(name, spec, &providers, &enabled, snapshot),
            );
        }

        Gateway {
            providers,
            enabled,
            served,
            problems,
            direct: Mutex::new(HashMap::new()),
        }
    }

    /// The route sets, in name order.
    pub fn served(&self) -> impl Iterator<Item = &Served> {
        self.served.values()
    }

    /// Every enabled `provider/model`, in name order.
    pub fn enabled(&self) -> impl Iterator<Item = &String> {
        self.enabled.iter()
    }

    /// Providers that are enabled and could not be built.
    pub fn problems(&self) -> &[(String, String)] {
        &self.problems
    }

    /// The built provider with this id, if it is enabled and built.
    pub fn provider(&self, id: &str) -> Option<&Built> {
        self.providers.get(id)
    }

    /// What a client's `model` names.
    ///
    /// A route set first. Otherwise `provider/model` for an enabled model, as a one route
    /// router kept so its breaker remembers between requests. Only enabled models are kept,
    /// so a client cannot grow the cache by inventing names.
    pub fn resolve(&self, model: &str) -> Resolution {
        if let Some(served) = self.served.get(model) {
            return Resolution::Found(served.router.clone(), served.on_device);
        }
        let Some((provider_id, target)) = split_route(model) else {
            return Resolution::Unknown;
        };
        if !self.enabled.contains(model) {
            return match self.providers.get(provider_id) {
                Some(_) => Resolution::NotEnabled(format!(
                    "{target} is not enabled on provider {provider_id}. Enable it through \
                     PUT /manage/providers/{provider_id}/models/{target}"
                )),
                None => Resolution::Unknown,
            };
        }
        let Some(built) = self.providers.get(provider_id) else {
            return Resolution::Unknown;
        };
        let Ok(mut direct) = self.direct.lock() else {
            return Resolution::Unknown;
        };
        let router = direct
            .entry(model.to_string())
            .or_insert_with(|| {
                Arc::new(
                    Router::new(vec![route(built, ModelId::from(target))])
                        .retrying(Retry::new(2))
                        .breaking(Breaker::default()),
                )
            })
            .clone();
        Resolution::Found(router, false)
    }
}

impl Gateway {
    /// What a call through this route cost.
    ///
    /// A self hosted route is free: its tokens are counted and nothing is charged. A route
    /// with a price book is priced against the model that served it, or, when a provider
    /// answered under a dated alias the book does not list, the model the route names. Any
    /// other answered call is unpriced, which is a different thing from free.
    pub fn cost(&self, route: &str, served: &ModelId, usage: &llmr::Usage) -> crate::usage::Cost {
        use crate::usage::Cost;
        let Some((provider_id, target)) = split_route(route) else {
            return Cost::Unpriced;
        };
        let Some(built) = self.providers.get(provider_id) else {
            return Cost::Unpriced;
        };
        if built.reach.is_on_device() {
            return Cost::Free;
        }
        let Some(book) = &built.prices else {
            return Cost::Unpriced;
        };
        match book
            .price(served, usage)
            .or_else(|| book.price(&ModelId::from(target), usage))
        {
            Some(priced) => Cost::priced(priced.amount, priced.currency, priced.coverage),
            None => Cost::Unpriced,
        }
    }
}

/// The gateway currently serving, replaced whole on every change.
pub struct Live(RwLock<Arc<Gateway>>);

impl Live {
    pub fn new(gateway: Gateway) -> Self {
        Live(RwLock::new(Arc::new(gateway)))
    }

    /// The gateway to serve this request with. The lock is held only to clone the `Arc`.
    pub fn current(&self) -> Arc<Gateway> {
        match self.0.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn replace(&self, gateway: Gateway) {
        let next = Arc::new(gateway);
        match self.0.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }
}

/// The table a provider answers `capabilities` from: the release's own for a vendor type,
/// with every row the panel wrote laid over it.
pub fn registry(record: &Provider, rows: &[&Model]) -> Registry {
    let shipped = match record.provider_type {
        ProviderType::Anthropic => Some(anthropic::api::shipped_registry()),
        ProviderType::Openai => Some(openai::api::shipped_registry()),
        ProviderType::Gemini => Some(gemini::api::shipped_registry()),
        ProviderType::OpenaiCompatible => None,
    };
    let reach = record
        .effective_reach()
        .unwrap_or(llmr::Reach::FirstPartyApi);
    let mut registry = shipped.unwrap_or_else(|| Registry::empty(record.id.clone(), reach));
    registry.reach = reach;
    for row in rows {
        if let Some(have) = row.capabilities {
            registry
                .models
                .insert(row.model_id.clone(), entry(&row.model_id, have));
        }
    }
    registry
}

fn entry(id: &str, have: Capabilities) -> Entry {
    let mut entry = Entry::new(id, "management API", "")
        .with_window(have.context_window, have.max_output)
        .able_to(
            have.tools,
            have.structured_output,
            have.prompt_caching,
            have.thinking,
        );
    if have.images {
        entry = entry.with_images();
    }
    if have.documents {
        entry = entry.with_documents();
    }
    if have.audio {
        entry = entry.with_audio();
    }
    if have.streaming {
        entry = entry.with_streaming();
    }
    entry
}

/// A provider from its stored record.
///
/// # Errors
///
/// When the record is incomplete (no base URL where one is needed, no credential where one
/// is required) or the HTTP client cannot be built.
pub fn build_provider(
    record: &Provider,
    credential: Option<&str>,
    rows: &[&Model],
) -> Result<Built, String> {
    let info = record.provider_type.info();
    let base_url = record
        .effective_base_url()
        .ok_or_else(|| "no base_url is set, and this provider type has no default".to_string())?;
    if record.effective_reach().is_none() {
        return Err("no reach is set, and this provider type needs one".into());
    }
    if info.credential == "required" && credential.is_none() {
        return Err("no credential is set, and this provider type needs one".into());
    }

    let transport: Arc<dyn HttpTransport> = Arc::new(
        Reqwest::new(Duration::from_secs(record.timeout_secs)).map_err(|e| e.to_string())?,
    );
    let key = Secret::new("provider-credential", credential.unwrap_or_default());
    let reach = record
        .effective_reach()
        .unwrap_or(llmr::Reach::FirstPartyApi);
    let registry = Arc::new(registry(record, rows));
    // Published prices hold for the vendor's own endpoint and nowhere else.
    let own_endpoint = record.base_url.is_none();

    let (inner, prices): (Arc<dyn llmr::Provider>, Option<PriceBook>) = match record.provider_type {
        ProviderType::Anthropic => (
            Arc::new(ApiProvider::new(
                anthropic::api::Messages,
                base_url,
                transport,
                key,
                reach,
                registry,
            )),
            own_endpoint.then(anthropic::api::shipped_prices),
        ),
        ProviderType::Gemini => (
            Arc::new(ApiProvider::new(
                gemini::api::GenerateContent,
                base_url,
                transport,
                key,
                reach,
                registry,
            )),
            own_endpoint.then(gemini::api::shipped_prices),
        ),
        ProviderType::Openai | ProviderType::OpenaiCompatible => (
            Arc::new(openai::api::at(
                // The protocol's own name, for its spans and messages. A fixed string rather
                // than the stored id, because the protocol wants one that lives for ever and
                // the gateway is rebuilt on every change; `Named` is what a route shows.
                record.provider_type.as_str(),
                base_url,
                transport,
                key,
                reach,
                registry,
            )),
            (record.provider_type == ProviderType::Openai && own_endpoint)
                .then(openai::api::shipped_prices),
        ),
    };

    Ok(Built {
        provider: Arc::new(Named {
            id: record.id.clone(),
            inner,
        }),
        provider_type: record.provider_type,
        reach,
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
    name: &str,
    spec: &RouteSpec,
    providers: &BTreeMap<String, Built>,
    enabled: &BTreeSet<String>,
    snapshot: &Snapshot,
) -> Served {
    let mut routes = Vec::new();
    let mut unavailable = Vec::new();
    for written in &spec.routes {
        let Some((provider_id, target)) = split_route(written) else {
            unavailable.push((written.clone(), "not written provider/model".into()));
            continue;
        };
        let Some(built) = providers.get(provider_id) else {
            let why = match snapshot.providers.iter().find(|(p, _)| p.id == provider_id) {
                Some((p, _)) if !p.enabled => "the provider is disabled",
                Some(_) => "the provider could not be built; see /manage/status",
                None => "no such provider",
            };
            unavailable.push((written.clone(), why.into()));
            continue;
        };
        if !enabled.contains(written) {
            unavailable.push((written.clone(), "the model is not enabled".into()));
            continue;
        }
        let model = ModelId::from(target);
        if built.provider.capabilities(&model).is_none() {
            unavailable.push((
                written.clone(),
                "nothing is known about what this model can do; set its capabilities".into(),
            ));
            continue;
        }
        routes.push(route(built, model));
    }

    let mut router = Router::new(routes)
        .ordering(match spec.order {
            OrderSpec::AsListed => Order::AsListed,
            OrderSpec::Cheapest => Order::Cheapest,
            OrderSpec::Healthiest => Order::Healthiest,
        })
        .retrying(Retry::new(spec.retry_attempts));
    if spec.breaker {
        router = router.breaking(Breaker::default());
    }
    if let Some(seconds) = spec.deadline_secs {
        router = router.within_deadline(Duration::from_secs(seconds));
    }

    Served {
        name: name.to_string(),
        router: Arc::new(router),
        on_device: spec.on_device,
        unavailable,
    }
}

impl Gateway {
    /// A gateway over routers built by hand, for tests that need a provider of their own.
    #[cfg(test)]
    pub fn from_routers(served: Vec<(&str, Router, bool)>) -> Gateway {
        let mut gateway = Gateway::empty();
        for (name, router, on_device) in served {
            gateway.served.insert(
                name.to_string(),
                Served {
                    name: name.to_string(),
                    router: Arc::new(router),
                    on_device,
                    unavailable: Vec::new(),
                },
            );
        }
        gateway
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, enabled: bool) -> (Provider, Option<String>) {
        (
            Provider {
                id: id.into(),
                provider_type: ProviderType::OpenaiCompatible,
                base_url: Some("http://127.0.0.1:9/v1".into()),
                reach: Some(llmr::Reach::SelfHosted),
                timeout_secs: 5,
                enabled,
                credential: None,
                credential_hint: None,
                created_at: 0,
                updated_at: 0,
            },
            None,
        )
    }

    fn model(provider: &str, id: &str, enabled: bool, known: bool) -> Model {
        Model {
            provider_id: provider.into(),
            model_id: id.into(),
            enabled,
            capabilities: known.then(|| Capabilities {
                streaming: true,
                ..Capabilities::default()
            }),
            updated_at: 0,
        }
    }

    fn spec(routes: &[&str]) -> RouteSpec {
        serde_json::from_value(serde_json::json!({ "routes": routes })).unwrap()
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            providers: vec![provider("local", true), provider("off", false)],
            models: vec![
                model("local", "small", true, true),
                model("local", "idle", false, true),
                model("local", "mystery", true, false),
                model("off", "small", true, true),
            ],
            routes: vec![(
                "default".into(),
                spec(&[
                    "local/small",
                    "local/idle",
                    "local/mystery",
                    "off/small",
                    "ghost/x",
                ]),
            )],
        }
    }

    #[test]
    fn a_route_set_keeps_what_can_serve_and_says_why_the_rest_cannot() {
        let gateway = Gateway::build(&snapshot());
        let served = gateway.served().next().unwrap();
        let names: Vec<String> = served.router.routes().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["local/small".to_string()]);

        let why: BTreeMap<&str, &str> = served
            .unavailable
            .iter()
            .map(|(r, w)| (r.as_str(), w.as_str()))
            .collect();
        assert_eq!(why["local/idle"], "the model is not enabled");
        assert!(why["local/mystery"].contains("capabilities"));
        assert_eq!(why["off/small"], "the provider is disabled");
        assert_eq!(why["ghost/x"], "no such provider");
    }

    #[test]
    fn only_enabled_models_on_enabled_providers_resolve_directly() {
        let gateway = Gateway::build(&snapshot());
        assert!(matches!(gateway.resolve("default"), Resolution::Found(..)));
        assert!(matches!(
            gateway.resolve("local/small"),
            Resolution::Found(..)
        ));
        assert!(matches!(
            gateway.resolve("local/idle"),
            Resolution::NotEnabled(_)
        ));
        assert!(matches!(gateway.resolve("off/small"), Resolution::Unknown));
        assert!(matches!(gateway.resolve("nothing"), Resolution::Unknown));
        // The same router both times, so its breaker remembers.
        let (Resolution::Found(a, _), Resolution::Found(b, _)) = (
            gateway.resolve("local/small"),
            gateway.resolve("local/small"),
        ) else {
            panic!("expected both to resolve")
        };
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn a_provider_that_cannot_be_built_is_reported_rather_than_taking_the_rest_down() {
        let mut snapshot = snapshot();
        let (mut broken, _) = provider("broken", true);
        broken.provider_type = ProviderType::Anthropic; // needs a credential it does not have
        snapshot.providers.push((broken, None));

        let gateway = Gateway::build(&snapshot);
        assert_eq!(gateway.problems().len(), 1);
        assert!(gateway.problems()[0].1.contains("credential"));
        assert!(matches!(
            gateway.resolve("local/small"),
            Resolution::Found(..)
        ));
    }

    #[test]
    fn a_call_is_priced_free_or_unpriced_and_never_a_made_up_zero() {
        use crate::usage::Cost;
        let (mut vendor, _) = provider("anthropic", true);
        vendor.provider_type = ProviderType::Anthropic;
        vendor.base_url = None;
        vendor.reach = None;
        let snapshot = Snapshot {
            providers: vec![(vendor, Some("sk-test".into())), provider("local", true)],
            models: vec![],
            routes: vec![],
        };
        let gateway = Gateway::build(&snapshot);
        let million = llmr::Usage::absent().with_input(1_000_000).with_output(0);

        // Priced from the vendor's published rate, even under a dated alias it answered with.
        let rate = anthropic::api::shipped_prices()
            .rate(&ModelId::from("claude-sonnet-5"))
            .copied()
            .unwrap();
        match gateway.cost(
            "anthropic/claude-sonnet-5",
            &"claude-sonnet-5-20260801".into(),
            &million,
        ) {
            Cost::Priced {
                micros,
                currency,
                partial,
            } => {
                assert_eq!(micros, rate.input.0);
                assert_eq!(currency, "USD");
                // Cache fields were not reported, so the figure is a floor.
                assert!(partial);
            }
            other => panic!("expected a price, got {other:?}"),
        }

        // A model the book does not list is unpriced, not free.
        assert_eq!(
            gateway.cost(
                "anthropic/claude-unknown",
                &"claude-unknown".into(),
                &million
            ),
            Cost::Unpriced
        );
        // A provider that reported no usage at all cannot be priced either.
        assert_eq!(
            gateway.cost(
                "anthropic/claude-sonnet-5",
                &"claude-sonnet-5".into(),
                &llmr::Usage::absent()
            ),
            Cost::Unpriced
        );
        // A self hosted model costs nothing.
        assert_eq!(
            gateway.cost("local/small", &"small".into(), &million),
            Cost::Free
        );
    }

    #[test]
    fn a_shipped_model_needs_no_capabilities_row_to_be_enabled() {
        let (mut vendor, _) = provider("anthropic", true);
        vendor.provider_type = ProviderType::Anthropic;
        vendor.base_url = None;
        vendor.reach = None;
        let snapshot = Snapshot {
            providers: vec![(vendor, Some("sk-test".into()))],
            models: vec![model("anthropic", "claude-sonnet-5", true, false)],
            routes: vec![("default".into(), spec(&["anthropic/claude-sonnet-5"]))],
        };
        let gateway = Gateway::build(&snapshot);
        let served = gateway.served().next().unwrap();
        assert!(served.unavailable.is_empty(), "{:?}", served.unavailable);
    }
}
