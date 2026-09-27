//! The management API: everything a panel needs to run the gateway.
//!
//! | Method | Path | What |
//! |---|---|---|
//! | `GET` | `/manage/status` | Version, uptime, counts, and providers that could not be built |
//! | `GET` | `/manage/provider-types` | The kinds of provider this build can reach, and what each needs |
//! | `GET` `POST` | `/manage/providers` | List, or add one |
//! | `GET` `PATCH` `DELETE` | `/manage/providers/{id}` | Read, change, remove |
//! | `POST` | `/manage/providers/{id}/test` | Is it reachable, is the key good; optionally one real call |
//! | `GET` | `/manage/providers/{id}/models` | What it serves, what each can do, which are enabled |
//! | `PUT` `DELETE` | `/manage/providers/{id}/models/{model}` | Enable, disable, set capabilities; forget |
//! | `GET` | `/manage/models` | Every enabled model, across providers |
//! | `GET` | `/manage/routes` | Every route set, with what is usable and what is resting |
//! | `GET` `PUT` `DELETE` | `/manage/routes/{name}` | One route set |
//! | `GET` `DELETE` | `/manage/usage` | Totals over a time range, grouped; or forget old rows |
//! | `GET` | `/manage/usage/requests` | Single requests, newest first |
//! | `GET` | `/manage/clis`, `/manage/clis/{name}` | The command line tools: installed version, where from, newest |
//! | `POST` | `/manage/clis/{name}/update` | Install a version onto the volume |
//! | `POST` | `/manage/clis/{name}/reset` | Go back to the version in the image |
//!
//! Every change is written to the store, then the gateway is rebuilt from the store and
//! swapped in, so the next request sees it. A credential is write only: it goes in on a
//! `POST` or `PATCH` and comes back as its last four characters.

use crate::cli::{Tool, Toolbox, UpdateError};
use crate::error::ApiError;
use crate::gateway::{build_provider, registry, Gateway, Snapshot};
use crate::records::{valid_id, Capabilities, Model, Provider, ProviderType, RouteSpec};
use crate::server::AppState;
use crate::usage::{Grouping, UsageFilter, UsageTotals};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Json;
use llmr::{ChatRequest, Message, ModelId, Reach};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

type Shared = State<Arc<AppState>>;
type Answer = Result<Response, ApiError>;

/// The management routes, merged into the application behind the same token check.
pub fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/manage/status", get(status))
        .route("/manage/provider-types", get(provider_types))
        .route(
            "/manage/providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/manage/providers/{id}",
            get(read_provider)
                .patch(update_provider)
                .delete(delete_provider),
        )
        .route("/manage/providers/{id}/test", post(test_provider))
        .route("/manage/providers/{id}/models", get(provider_models))
        .route(
            "/manage/providers/{id}/models/{*model}",
            put(put_model).delete(delete_model),
        )
        .route("/manage/models", get(enabled_models))
        .route("/manage/routes", get(list_routes))
        .route(
            "/manage/routes/{name}",
            get(read_route).put(put_route).delete(delete_route),
        )
        .route("/manage/usage", get(usage_totals).delete(forget_usage))
        .route("/manage/usage/requests", get(usage_requests))
        .route("/manage/clis", get(list_clis))
        .route("/manage/clis/{name}", get(read_cli))
        .route("/manage/clis/{name}/update", post(update_cli))
        .route("/manage/clis/{name}/reset", post(reset_cli))
}

/// Reads the store again, builds a new gateway, and swaps it in.
async fn reload(state: &AppState) -> Result<(), ApiError> {
    let tools = state.tools.clone();
    let gateway = state
        .db
        .run(move |store| Ok(Gateway::build(&Snapshot::read(store)?, &tools)))
        .await?;
    state.live.replace(gateway);
    Ok(())
}

fn ok(value: Value) -> Answer {
    Ok(Json(value).into_response())
}

fn created(value: Value) -> Answer {
    Ok((StatusCode::CREATED, Json(value)).into_response())
}

fn no_content() -> Answer {
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A body field that can be absent, `null`, or a value, told apart.
///
/// `PATCH {"credential": null}` clears a credential and `PATCH {}` leaves it alone, and a
/// plain `Option` would read both as `None`.
fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A request body as JSON, with the same error envelope as everything else.
fn json_body(raw: &[u8]) -> Result<Value, ApiError> {
    serde_json::from_slice(raw).map_err(|e| ApiError::invalid(format!("the body is not JSON: {e}")))
}

fn body<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value)
        .map_err(|e| ApiError::invalid(format!("the body could not be read: {e}")))
}

// ----- status and types --------------------------------------------------------------

async fn status(State(state): Shared) -> Answer {
    let gateway = state.live.current();
    let providers = state.db.run(|store| store.providers()).await?;
    let problems: Vec<Value> = gateway
        .problems()
        .iter()
        .map(|(id, why)| json!({ "provider": id, "problem": why }))
        .collect();
    ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started.elapsed().as_secs(),
        "auth": !state.keys.is_empty(),
        "providers": {
            "total": providers.len(),
            "enabled": providers.iter().filter(|p| p.enabled).count(),
            "problems": problems,
        },
        "models_enabled": gateway.enabled().count(),
        "route_sets": gateway.served().count(),
    }))
}

async fn provider_types() -> Answer {
    let types: Vec<Value> = ProviderType::ALL
        .into_iter()
        .map(|t| serde_json::to_value(t.info()).unwrap_or(Value::Null))
        .collect();
    ok(json!({ "data": types }))
}

// ----- providers ---------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewProvider {
    id: String,
    #[serde(rename = "type")]
    provider_type: String,
    base_url: Option<String>,
    reach: Option<String>,
    timeout_secs: Option<u64>,
    enabled: Option<bool>,
    credential: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderChange {
    #[serde(default, deserialize_with = "present")]
    base_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    reach: Option<Option<String>>,
    timeout_secs: Option<u64>,
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    credential: Option<Option<String>>,
}

fn read_reach(text: &str) -> Result<Reach, ApiError> {
    Reach::parse(text).ok_or_else(|| {
        ApiError::invalid_param(
            "reach",
            format!(
                "unknown reach {text:?}; one of first-party-api, cloud-partner, \
                 private-endpoint, self-hosted"
            ),
        )
    })
}

fn read_base_url(url: &str) -> Result<String, ApiError> {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(url.to_string())
    } else {
        Err(ApiError::invalid_param(
            "base_url",
            "base_url must start with http:// or https://",
        ))
    }
}

fn read_timeout(secs: u64) -> Result<u64, ApiError> {
    if (1..=3600).contains(&secs) {
        Ok(secs)
    } else {
        Err(ApiError::invalid_param(
            "timeout_secs",
            "timeout_secs must be between 1 and 3600",
        ))
    }
}

/// Whether a provider as it stands can be built, said the way a panel shows it.
fn complete(provider: &Provider) -> Result<(), ApiError> {
    let info = provider.provider_type.info();
    let cli = provider.provider_type.tool().is_some();
    if cli && provider.reach.is_some_and(|r| r != Reach::LocalCli) {
        return Err(ApiError::invalid_param(
            "reach",
            format!(
                "a {} provider runs a command line tool that calls its vendor, so its reach \
                 is local-cli",
                info.name
            ),
        ));
    }
    if !cli && provider.effective_base_url().is_none() {
        return Err(ApiError::invalid_param(
            "base_url",
            format!("a {} provider needs a base_url", info.name),
        ));
    }
    if provider.effective_reach().is_none() {
        return Err(ApiError::invalid_param(
            "reach",
            format!(
                "a {} provider needs a reach: the same protocol is spoken by a model on your \
                 own hardware and by a hosted API, and which one this is decides where \
                 prompts may go",
                info.name
            ),
        ));
    }
    if provider.enabled && info.credential == "required" && provider.credential.is_none() {
        return Err(ApiError::invalid_param(
            "credential",
            format!("an enabled {} provider needs a credential", info.name),
        ));
    }
    Ok(())
}

async fn list_providers(State(state): Shared) -> Answer {
    let gateway = state.live.current();
    let providers = state.db.run(|store| store.providers()).await?;
    let data: Vec<Value> = providers
        .iter()
        .map(|p| with_problem(p.view(), &gateway, &p.id))
        .collect();
    ok(json!({ "data": data }))
}

/// A provider view, with the reason it could not be built when it could not.
fn with_problem(mut view: Value, gateway: &Gateway, id: &str) -> Value {
    if let Some((_, why)) = gateway.problems().iter().find(|(p, _)| p == id) {
        view["problem"] = json!(why);
    }
    view
}

async fn create_provider(State(state): Shared, raw: axum::body::Bytes) -> Answer {
    let new: NewProvider = body(json_body(&raw)?)?;
    if !valid_id(&new.id) {
        return Err(ApiError::invalid_param(
            "id",
            "id must be 1 to 64 letters, digits, '-', '_' or '.'",
        ));
    }
    let provider_type = ProviderType::parse(&new.provider_type).ok_or_else(|| {
        ApiError::invalid_param(
            "type",
            format!(
                "unknown type {:?}; GET /manage/provider-types lists them",
                new.provider_type
            ),
        )
    })?;
    let base_url = new.base_url.as_deref().map(read_base_url).transpose()?;
    let reach = new.reach.as_deref().map(read_reach).transpose()?;
    let timeout_secs = read_timeout(new.timeout_secs.unwrap_or(120))?;
    let credential = new.credential.filter(|c| !c.trim().is_empty());

    // Checked before anything is written, against the provider as it would be stored: the
    // credential only matters here as present or absent.
    let mut provider = Provider {
        id: new.id,
        provider_type,
        base_url,
        reach,
        timeout_secs,
        enabled: new.enabled.unwrap_or(true),
        credential: credential.as_ref().map(|_| Vec::new()),
        credential_hint: None,
        created_at: 0,
        updated_at: 0,
    };
    complete(&provider)?;

    let provider = state
        .db
        .run(move |store| {
            if let Some(plain) = credential {
                let (sealed, hint) = store.seal_credential(plain.trim())?;
                provider.credential = Some(sealed);
                provider.credential_hint = Some(hint);
            }
            store.insert_provider(&provider)?;
            store.provider(&provider.id)
        })
        .await?;
    reload(&state).await?;
    created(with_problem(
        provider.view(),
        &state.live.current(),
        &provider.id,
    ))
}

async fn read_provider(State(state): Shared, Path(id): Path<String>) -> Answer {
    let gateway = state.live.current();
    let provider = state.db.run(move |store| store.provider(&id)).await?;
    ok(with_problem(provider.view(), &gateway, &provider.id))
}

async fn update_provider(
    State(state): Shared,
    Path(id): Path<String>,
    raw: axum::body::Bytes,
) -> Answer {
    let change: ProviderChange = body(json_body(&raw)?)?;
    let base_url = match change.base_url {
        Some(Some(url)) => Some(Some(read_base_url(&url)?)),
        other => other,
    };
    let reach = match change.reach {
        Some(Some(text)) => Some(Some(read_reach(&text)?)),
        Some(None) => Some(None),
        None => None,
    };
    let timeout_secs = change.timeout_secs.map(read_timeout).transpose()?;

    let after = state
        .db
        .run(move |store| {
            let mut after = store.provider(&id)?;
            if let Some(url) = base_url {
                after.base_url = url;
            }
            if let Some(reach) = reach {
                after.reach = reach;
            }
            if let Some(secs) = timeout_secs {
                after.timeout_secs = secs;
            }
            if let Some(enabled) = change.enabled {
                after.enabled = enabled;
            }
            match change.credential {
                Some(Some(plain)) if !plain.trim().is_empty() => {
                    let (sealed, hint) = store.seal_credential(plain.trim())?;
                    after.credential = Some(sealed);
                    after.credential_hint = Some(hint);
                }
                Some(_) => {
                    after.credential = None;
                    after.credential_hint = None;
                }
                None => {}
            }
            Ok(after)
        })
        .await?;

    complete(&after)?;
    let id = after.id.clone();
    let provider = state
        .db
        .run(move |store| {
            store.update_provider(&after)?;
            store.provider(&id)
        })
        .await?;
    reload(&state).await?;
    ok(with_problem(
        provider.view(),
        &state.live.current(),
        &provider.id,
    ))
}

async fn delete_provider(State(state): Shared, Path(id): Path<String>) -> Answer {
    state
        .db
        .run(move |store| store.delete_provider(&id))
        .await?;
    reload(&state).await?;
    no_content()
}

// ----- testing a provider ------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TestRequest {
    /// Validate this model rather than the provider's model list.
    model: Option<String>,
    /// Also send one short, real, billable request to `model`.
    #[serde(default)]
    live: bool,
}

/// The provider as stored, built on the spot: enabled or not, so a panel can test a key
/// before switching it on.
async fn built_on_the_spot(
    state: &AppState,
    id: String,
) -> Result<(Provider, crate::gateway::Built), ApiError> {
    let (provider, credential, rows) = state
        .db
        .run(move |store| {
            let provider = store.provider(&id)?;
            let credential = store.open_credential(&provider)?;
            let rows = store.models(Some(&id))?;
            Ok((provider, credential, rows))
        })
        .await?;
    let refs: Vec<&Model> = rows.iter().collect();
    let built =
        build_provider(&provider, credential.as_deref(), &refs, &state.tools).map_err(|why| {
            ApiError::invalid(format!("provider {} cannot be built: {why}", provider.id))
        })?;
    Ok((provider, built))
}

/// How long a test waits for the provider before it reports that it could not tell.
const TEST_DEADLINE: Duration = Duration::from_secs(30);

async fn test_provider(
    State(state): Shared,
    Path(id): Path<String>,
    raw: axum::body::Bytes,
) -> Answer {
    // No body, or an empty one with a JSON content type (which panels send), means the
    // default: the free check against the model list.
    let request: TestRequest = if raw.iter().all(u8::is_ascii_whitespace) {
        TestRequest::default()
    } else {
        body(
            serde_json::from_slice(&raw)
                .map_err(|e| ApiError::invalid(format!("the body is not JSON: {e}")))?,
        )?
    };
    if request.live && request.model.is_none() {
        return Err(ApiError::invalid_param(
            "live",
            "a live test needs a model to send to",
        ));
    }
    let (provider, built) = built_on_the_spot(&state, id).await?;

    let mut report = json!({ "provider": provider.id });
    if let Some(tool) = provider.provider_type.tool() {
        report["cli"] = cli_view(&state.tools, tool);
    }

    // The free check: a named model is validated, otherwise the model list is fetched. Both
    // prove the credential without generating a token.
    match &request.model {
        Some(model) => {
            let access = tokio::time::timeout(
                TEST_DEADLINE,
                built.provider.validate(&ModelId::from(model.as_str())),
            )
            .await
            .unwrap_or_else(|_| llmr::Access::unknown("the provider did not answer in time"));
            report["access"] = json!(access.as_str());
            report["detail"] = json!(access.detail());
        }
        None => match tokio::time::timeout(TEST_DEADLINE, built.provider.catalogue()).await {
            Ok(Ok(listed)) => {
                report["access"] = json!("ready");
                report["models_listed"] = json!(listed.len());
            }
            Ok(Err(error)) => {
                let (access, detail) = match &error {
                    llmr::Error::Auth(_) | llmr::Error::NotFound(_) => {
                        ("denied", error.to_string())
                    }
                    llmr::Error::Unsupported(_) => (
                        "unknown",
                        "this provider cannot list its models; test it with a model".to_string(),
                    ),
                    _ => ("unknown", error.to_string()),
                };
                report["access"] = json!(access);
                report["detail"] = json!(detail);
            }
            Err(_) => {
                report["access"] = json!("unknown");
                report["detail"] = json!("the provider did not answer in time");
            }
        },
    }

    if let (true, Some(model)) = (request.live, &request.model) {
        let started = Instant::now();
        let call = ChatRequest::new(
            model.as_str(),
            vec![Message::user("Reply with the word ok.")],
        )
        .with_max_tokens(16);
        let outcome = tokio::time::timeout(TEST_DEADLINE, built.provider.chat(call)).await;
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        report["live"] = match outcome {
            Ok(Ok(reply)) => json!({
                "ok": true,
                "latency_ms": elapsed,
                "model": reply.model.as_str(),
                "usage": {
                    "input_tokens": reply.usage.prompt_tokens(),
                    "output_tokens": reply.usage.output_tokens,
                },
            }),
            Ok(Err(error)) => {
                json!({ "ok": false, "latency_ms": elapsed, "error": error.to_string() })
            }
            Err(_) => {
                json!({ "ok": false, "latency_ms": elapsed, "error": "no answer within 30s" })
            }
        };
    }

    ok(report)
}

// ----- models ------------------------------------------------------------------------

async fn provider_models(State(state): Shared, Path(id): Path<String>) -> Answer {
    let (provider, built) = built_on_the_spot(&state, id).await?;
    let lookup = provider.id.clone();
    let rows = state
        .db
        .run(move |store| store.models(Some(&lookup)))
        .await?;
    let row_refs: Vec<&Model> = rows.iter().collect();
    let shipped = registry(&provider, &[]);
    let known = registry(&provider, &row_refs);
    let cli = provider.provider_type.tool().is_some();

    // What the provider says it serves right now, when it can say.
    let (listed, listing_error) =
        match tokio::time::timeout(TEST_DEADLINE, built.provider.catalogue()).await {
            Ok(Ok(ids)) => (Some(ids), None),
            Ok(Err(error)) => (None, Some(error.to_string())),
            Err(_) => (
                None,
                Some("the provider did not answer in time".to_string()),
            ),
        };

    let mut ids: BTreeMap<String, ()> = BTreeMap::new();
    for id in listed.iter().flatten() {
        ids.insert(id.as_str().to_string(), ());
    }
    for id in shipped
        .models
        .keys()
        .chain(rows.iter().map(|r| &r.model_id))
    {
        ids.insert(id.clone(), ());
    }

    let data: Vec<Value> = ids
        .keys()
        .map(|model_id| {
            let row = rows.iter().find(|r| &r.model_id == model_id);
            let capabilities = if cli {
                built
                    .provider
                    .capabilities(&ModelId::from(model_id.as_str()))
            } else {
                known.capabilities(&ModelId::from(model_id.as_str()))
            }
            .map(|c| Capabilities::from_engine(&c));
            let source = match (
                row.and_then(|r| r.capabilities),
                shipped.models.contains_key(model_id),
            ) {
                _ if cli => Some("cli"),
                (Some(_), _) => Some("custom"),
                (None, true) => Some("shipped"),
                (None, false) => None,
            };
            json!({
                "id": model_id,
                "route": format!("{}/{}", provider.id, model_id),
                "enabled": row.is_some_and(|r| r.enabled),
                // `null` when the provider could not list its models, rather than `false`.
                "listed": listed.as_ref().map(|l| l.iter().any(|m| m.as_str() == model_id)),
                "capabilities": capabilities,
                "capabilities_source": source,
                "updated_at": row.map(|r| r.updated_at),
            })
        })
        .collect();

    ok(json!({
        "provider": provider.id,
        "listing_error": listing_error,
        "data": data,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelChange {
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    capabilities: Option<Option<Capabilities>>,
}

async fn put_model(
    State(state): Shared,
    Path((provider_id, model_id)): Path<(String, String)>,
    raw: axum::body::Bytes,
) -> Answer {
    let change: ModelChange = body(json_body(&raw)?)?;
    let model_id = model_id.trim_start_matches('/').to_string();
    if model_id.is_empty() {
        return Err(ApiError::invalid_param("model", "the model id is empty"));
    }

    let (provider, existing) = {
        let (p, m) = (provider_id.clone(), model_id.clone());
        state
            .db
            .run(move |store| {
                let provider = store.provider(&p)?;
                let existing = store
                    .models(Some(&p))?
                    .into_iter()
                    .find(|row| row.model_id == m);
                Ok((provider, existing))
            })
            .await?
    };

    let mut row = existing.unwrap_or(Model {
        provider_id: provider.id.clone(),
        model_id: model_id.clone(),
        enabled: false,
        capabilities: None,
        updated_at: 0,
    });
    if let Some(enabled) = change.enabled {
        row.enabled = enabled;
    }
    if let Some(capabilities) = change.capabilities {
        row.capabilities = capabilities;
    }

    // Through a command line tool every model is text in, text out, whatever it can do
    // behind an API. Capabilities written here would promise what the tool cannot carry.
    if provider.provider_type.tool().is_some() {
        // Passed to the tool as an argument, so it must not read as one of its flags.
        if !crate::cli::valid_model(&model_id) {
            return Err(ApiError::invalid_param(
                "model",
                "a command line provider's model id may not start with '-' or contain spaces \
                 or control characters",
            ));
        }
        if row.capabilities.is_some() {
            return Err(ApiError::invalid_param(
                "capabilities",
                "a command line provider carries text only, so its models take no capabilities",
            ));
        }
    } else {
        check_known(&provider, &row, &model_id)?;
    }

    let stored = row.clone();
    state.db.run(move |store| store.put_model(&stored)).await?;
    reload(&state).await?;
    ok(json!({
        "provider": row.provider_id,
        "id": row.model_id,
        "route": format!("{}/{}", provider.id, model_id),
        "enabled": row.enabled,
        "capabilities": row.capabilities,
    }))
}

/// A model is only usable when something says what it can do. Refused here rather than
/// accepted and silently unroutable.
fn check_known(provider: &Provider, row: &Model, model_id: &str) -> Result<(), ApiError> {
    let shipped_knows = registry(provider, &[])
        .capabilities(&ModelId::from(model_id))
        .is_some();
    if row.enabled && row.capabilities.is_none() && !shipped_knows {
        return Err(ApiError::invalid_param(
            "capabilities",
            format!(
                "nothing is known about what {model_id} can do; send its capabilities \
                 (tools, streaming, images, ...) to enable it"
            ),
        ));
    }
    Ok(())
}

async fn delete_model(
    State(state): Shared,
    Path((provider_id, model_id)): Path<(String, String)>,
) -> Answer {
    let model_id = model_id.trim_start_matches('/').to_string();
    state
        .db
        .run(move |store| store.delete_model(&provider_id, &model_id))
        .await?;
    reload(&state).await?;
    no_content()
}

async fn enabled_models(State(state): Shared) -> Answer {
    let gateway = state.live.current();
    let data: Vec<Value> = gateway
        .enabled()
        .filter_map(|route| {
            let (provider_id, model_id) = crate::records::split_route(route)?;
            let built = gateway.provider(provider_id)?;
            let capabilities = built
                .provider
                .capabilities(&ModelId::from(model_id))
                .map(|c| Capabilities::from_engine(&c));
            Some(json!({
                "id": route,
                "provider": provider_id,
                "model": model_id,
                "type": built.provider_type,
                "reach": built.reach.as_str(),
                "priced": built.prices.as_ref().is_some_and(|p| p.rate(&ModelId::from(model_id)).is_some()),
                "capabilities": capabilities,
            }))
        })
        .collect();
    ok(json!({ "data": data }))
}

// ----- route sets --------------------------------------------------------------------

fn route_view(name: &str, spec: &RouteSpec, gateway: &Gateway) -> Value {
    let served = gateway.served().find(|s| s.name == name);
    let usable: Vec<String> = served
        .map(|s| s.router.routes().map(|(route, _)| route).collect())
        .unwrap_or_default();
    let unavailable: Vec<Value> = served
        .map(|s| {
            s.unavailable
                .iter()
                .map(|(route, why)| json!({ "route": route, "why": why }))
                .collect()
        })
        .unwrap_or_default();
    let resting: Vec<Value> = served
        .map(|s| {
            s.router
                .resting()
                .into_iter()
                .map(|(route, left)| json!({ "route": route, "seconds_left": left.as_secs() }))
                .collect()
        })
        .unwrap_or_default();
    let mut view = serde_json::to_value(spec).unwrap_or(Value::Null);
    view["name"] = json!(name);
    view["usable"] = json!(usable);
    view["unavailable"] = json!(unavailable);
    view["resting"] = json!(resting);
    view
}

async fn list_routes(State(state): Shared) -> Answer {
    let gateway = state.live.current();
    let specs = state.db.run(|store| store.routes()).await?;
    let data: Vec<Value> = specs
        .iter()
        .map(|(name, spec)| route_view(name, spec, &gateway))
        .collect();
    ok(json!({ "data": data }))
}

async fn read_route(State(state): Shared, Path(name): Path<String>) -> Answer {
    let gateway = state.live.current();
    let lookup = name.clone();
    let spec = state.db.run(move |store| store.route(&lookup)).await?;
    ok(route_view(&name, &spec, &gateway))
}

async fn put_route(
    State(state): Shared,
    Path(name): Path<String>,
    raw: axum::body::Bytes,
) -> Answer {
    if !valid_id(&name) {
        return Err(ApiError::invalid_param(
            "name",
            "a route set name must be 1 to 64 letters, digits, '-', '_' or '.'; a '/' would \
             read as provider/model",
        ));
    }
    let spec: RouteSpec = body(json_body(&raw)?)?;
    spec.check()
        .map_err(|why| ApiError::invalid_param("routes", why))?;
    let (key, stored) = (name.clone(), spec.clone());
    state
        .db
        .run(move |store| store.put_route(&key, &stored))
        .await?;
    reload(&state).await?;
    ok(route_view(&name, &spec, &state.live.current()))
}

async fn delete_route(State(state): Shared, Path(name): Path<String>) -> Answer {
    state.db.run(move |store| store.delete_route(&name)).await?;
    reload(&state).await?;
    no_content()
}

// ----- usage -------------------------------------------------------------------------

/// The query a usage call takes. Times are unix seconds: `from` inclusive, `to` exclusive.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct UsageQuery {
    from: Option<i64>,
    to: Option<i64>,
    provider: Option<String>,
    /// `provider/model`.
    model: Option<String>,
    /// The name the client asked for: a route set, or a direct model.
    asked: Option<String>,
    outcome: Option<String>,
    group_by: Option<String>,
    limit: Option<i64>,
    /// A request id from a previous page: rows older than it.
    before: Option<i64>,
}

impl UsageQuery {
    fn filter(&self) -> Result<UsageFilter, ApiError> {
        if let Some(outcome) = &self.outcome {
            if !matches!(outcome.as_str(), "ok" | "refused" | "error" | "interrupted") {
                return Err(ApiError::invalid_param(
                    "outcome",
                    "outcome is one of ok, refused, error, interrupted",
                ));
            }
        }
        if let (Some(from), Some(to)) = (self.from, self.to) {
            if from >= to {
                return Err(ApiError::invalid_param("to", "to must be after from"));
            }
        }
        Ok(UsageFilter {
            from: self.from,
            to: self.to,
            provider: self.provider.clone(),
            model: self.model.clone(),
            asked: self.asked.clone(),
            outcome: self.outcome.clone(),
        })
    }
}

/// A query string read into its struct, with the same error envelope as everything else.
fn query<T: serde::de::DeserializeOwned + Default>(
    raw: Result<Query<T>, axum::extract::rejection::QueryRejection>,
) -> Result<T, ApiError> {
    raw.map(|Query(q)| q)
        .map_err(|e| ApiError::invalid(format!("the query string could not be read: {e}")))
}

async fn usage_totals(
    State(state): Shared,
    raw: Result<Query<UsageQuery>, axum::extract::rejection::QueryRejection>,
) -> Answer {
    let q = query(raw)?;
    let filter = q.filter()?;
    let group = match q.group_by.as_deref() {
        None => Grouping::None,
        Some(text) => Grouping::parse(text).ok_or_else(|| {
            ApiError::invalid_param(
                "group_by",
                "group_by is one of none, model, provider, asked, day",
            )
        })?,
    };
    let (by_group, all) = state
        .db
        .run(move |store| {
            let by_group = match group {
                Grouping::None => Vec::new(),
                other => store.usage_summary(&filter, other)?,
            };
            let all = store.usage_summary(&filter, Grouping::None)?;
            Ok((by_group, all))
        })
        .await?;

    let total = all
        .into_iter()
        .fold(UsageTotals::default(), |mut total, part| {
            total.absorb(part);
            total
        });
    let mut body = json!({
        "from": q.from,
        "to": q.to,
        "group_by": group.key_name(),
        "total": total.view(None),
    });
    if group != Grouping::None {
        body["data"] = json!(by_group
            .iter()
            .map(|t| t.view(group.key_name()))
            .collect::<Vec<_>>());
    }
    ok(body)
}

async fn usage_requests(
    State(state): Shared,
    raw: Result<Query<UsageQuery>, axum::extract::rejection::QueryRejection>,
) -> Answer {
    let q = query(raw)?;
    if q.group_by.is_some() {
        return Err(ApiError::invalid_param(
            "group_by",
            "group_by is for /manage/usage; this lists single requests",
        ));
    }
    let filter = q.filter()?;
    let limit = q.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::invalid_param(
            "limit",
            "limit is between 1 and 1000",
        ));
    }
    let before = q.before;
    let rows = state
        .db
        .run(move |store| store.usage_requests(&filter, before, limit))
        .await?;
    let next = if rows.len() == usize::try_from(limit).unwrap_or(usize::MAX) {
        rows.last().map(|(id, _)| *id)
    } else {
        None
    };
    ok(json!({
        "data": rows.iter().map(|(id, row)| row.view(*id)).collect::<Vec<_>>(),
        // Pass as `before` for the next, older page. `null` when this was the last.
        "next_before": next,
    }))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ForgetQuery {
    before: Option<i64>,
}

async fn forget_usage(
    State(state): Shared,
    raw: Result<Query<ForgetQuery>, axum::extract::rejection::QueryRejection>,
) -> Answer {
    // Required rather than defaulted: "delete everything" is one missing parameter away
    // from an accident.
    let before = query(raw)?.before.ok_or_else(|| {
        ApiError::invalid_param(
            "before",
            "before is required: the unix time before which rows are removed",
        )
    })?;
    let deleted = state
        .db
        .run(move |store| store.delete_usage(before))
        .await?;
    ok(json!({ "deleted": deleted }))
}

// ----- command line tools -------------------------------------------------------------

/// A tool as the API shows it.
fn cli_view(tools: &Toolbox, tool: Tool) -> Value {
    let installed = tools.installed(tool);
    json!({
        "name": tool.name(),
        "title": tool.title(),
        "program": tool.program(),
        "package": tool.package(),
        "provider_type": tool.provider_type(),
        "installed": installed.is_some(),
        "version": installed.as_ref().and_then(|i| i.version.clone()),
        // `image`: the version this release was tested with; `updated`: installed onto the
        // volume through this API.
        "source": installed.as_ref().map(|i| i.source),
        "image_version": tools.image_version(tool),
    })
}

fn read_tool(name: &str) -> Result<Tool, ApiError> {
    Tool::parse(name).ok_or_else(|| {
        ApiError::not_found(format!(
            "no command line tool {name:?}; one of claude-code, codex, gemini-cli"
        ))
    })
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CliQuery {
    /// Also ask npm for the newest version. Slower, and needs the registry reachable.
    #[serde(default)]
    check_latest: bool,
}

/// Adds the newest version npm has, and whether it differs from the one in use.
async fn with_latest(tools: &Toolbox, tool: Tool, mut view: Value) -> Value {
    match tools.latest(tool).await {
        Ok(latest) => {
            view["update_available"] = json!(view["version"].as_str() != Some(latest.as_str()));
            view["latest"] = json!(latest);
        }
        Err(why) => {
            view["latest"] = Value::Null;
            view["latest_error"] = json!(why);
        }
    }
    view
}

async fn list_clis(
    State(state): Shared,
    query: Result<Query<CliQuery>, axum::extract::rejection::QueryRejection>,
) -> Answer {
    let query = query.map_err(|e| ApiError::invalid(e.body_text()))?.0;
    let tools = &state.tools;
    let mut data = Vec::new();
    if query.check_latest {
        let views = Tool::ALL.map(|tool| with_latest(tools, tool, cli_view(tools, tool)));
        let [a, b, c] = views;
        let (a, b, c) = tokio::join!(a, b, c);
        data.extend([a, b, c]);
    } else {
        data.extend(Tool::ALL.map(|tool| cli_view(tools, tool)));
    }
    ok(json!({ "data": data }))
}

async fn read_cli(
    State(state): Shared,
    Path(name): Path<String>,
    query: Result<Query<CliQuery>, axum::extract::rejection::QueryRejection>,
) -> Answer {
    let query = query.map_err(|e| ApiError::invalid(e.body_text()))?.0;
    let tool = read_tool(&name)?;
    let view = cli_view(&state.tools, tool);
    ok(if query.check_latest {
        with_latest(&state.tools, tool, view).await
    } else {
        view
    })
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CliUpdate {
    /// `latest`, or a version such as `2.1.283`.
    version: Option<String>,
}

fn update_error(tool: Tool, error: UpdateError) -> ApiError {
    match error {
        UpdateError::Busy => ApiError::conflict(
            "another update of a command line tool is running; try again when it is done",
        ),
        UpdateError::BadVersion => ApiError::invalid_param(
            "version",
            "version must be \"latest\" or a version number such as 1.2.3",
        ),
        UpdateError::Failed(why) => {
            ApiError::bad_gateway("update_failed", format!("{}: {why}", tool.title()))
        }
    }
}

/// Installs a version onto the volume. The next call runs it; calls in flight finish on the
/// copy they started with.
async fn update_cli(
    State(state): Shared,
    Path(name): Path<String>,
    raw: axum::body::Bytes,
) -> Answer {
    let tool = read_tool(&name)?;
    let request: CliUpdate = if raw.iter().all(u8::is_ascii_whitespace) {
        CliUpdate::default()
    } else {
        body(json_body(&raw)?)?
    };
    let version = request.version.unwrap_or_else(|| "latest".into());
    let before = state.tools.installed(tool).and_then(|i| i.version);
    let started = Instant::now();
    state
        .tools
        .update(tool, &version)
        .await
        .map_err(|e| update_error(tool, e))?;
    let mut view = cli_view(&state.tools, tool);
    view["previous_version"] = json!(before);
    view["took_ms"] = json!(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    tracing::info!(
        tool = tool.name(),
        from = before.as_deref().unwrap_or("none"),
        to = view["version"].as_str().unwrap_or("unknown"),
        "command line tool updated"
    );
    ok(view)
}

/// Removes the copy on the volume, so calls run the image's version again.
async fn reset_cli(State(state): Shared, Path(name): Path<String>) -> Answer {
    let tool = read_tool(&name)?;
    let removed = state
        .tools
        .reset(tool)
        .await
        .map_err(|e| update_error(tool, e))?;
    let mut view = cli_view(&state.tools, tool);
    view["removed_update"] = json!(removed);
    ok(view)
}

#[cfg(test)]
mod tests {
    use crate::cli::Toolbox;
    use crate::crypto::MasterKey;
    use crate::gateway::{Gateway, Live};
    use crate::server::{app, AppState};
    use crate::store::{Db, Store};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    /// An OpenAI-compatible endpoint on a real port, accepting only the key `good`.
    async fn upstream() -> String {
        use axum::http::HeaderMap;
        use axum::routing::{get, post};

        fn allowed(headers: &HeaderMap) -> bool {
            headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer good")
        }
        let router = axum::Router::new()
            .route(
                "/v1/models",
                get(|headers: HeaderMap| async move {
                    if !allowed(&headers) {
                        return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": "bad key" })));
                    }
                    (
                        StatusCode::OK,
                        axum::Json(json!({ "data": [{ "id": "small" }, { "id": "big" }] })),
                    )
                }),
            )
            .route(
                "/v1/chat/completions",
                post(|headers: HeaderMap| async move {
                    if !allowed(&headers) {
                        return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": "bad key" })));
                    }
                    (
                        StatusCode::OK,
                        axum::Json(json!({
                            "model": "small",
                            "choices": [{ "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
                            "usage": { "prompt_tokens": 5, "completion_tokens": 1 },
                        })),
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{address}/v1")
    }

    fn gateway() -> axum::Router {
        gateway_with(Toolbox::new(
            std::path::PathBuf::from("/nonexistent"),
            std::path::Path::new("/nonexistent"),
        ))
    }

    fn gateway_with(tools: Toolbox) -> axum::Router {
        let key = MasterKey::from_base64("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();
        let db = Db::new(Store::in_memory(key).unwrap());
        app(
            Arc::new(AppState {
                live: Live::new(Gateway::empty()),
                db: db.clone(),
                keys: Vec::new(),
                started: std::time::Instant::now(),
                recorder: crate::usage::Recorder::start(db.clone()).0,
                tools: Arc::new(tools),
            }),
            1024 * 1024,
        )
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(match body {
                Some(body) => Body::from(body.to_string()),
                None => Body::empty(),
            })
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    async fn local_provider(app: &axum::Router, base_url: &str, credential: &str) {
        let (status, body) = call(
            app,
            "POST",
            "/manage/providers",
            Some(json!({
                "id": "local",
                "type": "openai-compatible",
                "base_url": base_url,
                "reach": "self-hosted",
                "credential": credential,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    #[tokio::test]
    async fn from_an_empty_gateway_to_a_served_request_over_rest_alone() {
        let app = gateway();
        let base = upstream().await;

        // 1. A provider. The credential goes in and never comes back.
        local_provider(&app, &base, "good").await;
        let (_, listed) = call(&app, "GET", "/manage/providers", None).await;
        assert_eq!(listed["data"][0]["credential"], "…good");
        assert!(!listed.to_string().contains("\"good\""), "{listed}");

        // 2. What it serves, straight from its model list.
        let (status, models) = call(&app, "GET", "/manage/providers/local/models", None).await;
        assert_eq!(status, StatusCode::OK, "{models}");
        let ids: Vec<&str> = models["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["big", "small"]);
        assert_eq!(models["data"][1]["listed"], true);
        assert_eq!(models["data"][1]["enabled"], false);

        // 3. Enabling a model nothing is known about needs its capabilities.
        let (status, refused) = call(
            &app,
            "PUT",
            "/manage/providers/local/models/small",
            Some(json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(refused["error"]["param"], "capabilities");
        let (status, enabled) = call(
            &app,
            "PUT",
            "/manage/providers/local/models/small",
            Some(json!({ "enabled": true, "capabilities": { "streaming": true, "tools": true } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{enabled}");

        let (_, all) = call(&app, "GET", "/manage/models", None).await;
        assert_eq!(all["data"][0]["id"], "local/small");
        assert_eq!(all["data"][0]["reach"], "self-hosted");

        // 4. A route set over it.
        let (status, route) = call(
            &app,
            "PUT",
            "/manage/routes/default",
            Some(json!({ "routes": ["local/small", "local/big"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert_eq!(route["usable"], json!(["local/small"]));
        assert_eq!(route["unavailable"][0]["why"], "the model is not enabled");

        // 5. And a client call through it, answered by the upstream.
        let (status, reply) = call(
            &app,
            "POST",
            "/v1/chat/completions",
            Some(json!({ "model": "default", "messages": [{ "role": "user", "content": "hi" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["choices"][0]["message"]["content"], "ok");

        // The names a client may use: the route set, then every enabled model.
        let (_, served) = call(&app, "GET", "/v1/models", None).await;
        let names: Vec<&str> = served["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["default", "local/small"]);
    }

    #[tokio::test]
    async fn a_disabled_model_is_refused_by_name_rather_than_reported_missing() {
        let app = gateway();
        let base = upstream().await;
        local_provider(&app, &base, "good").await;
        call(
            &app,
            "PUT",
            "/manage/providers/local/models/big",
            Some(json!({ "enabled": false, "capabilities": {} })),
        )
        .await;

        let (status, error) = call(
            &app,
            "POST",
            "/v1/chat/completions",
            Some(
                json!({ "model": "local/big", "messages": [{ "role": "user", "content": "hi" }] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(error["error"]["code"], "model_not_enabled");
    }

    #[tokio::test]
    async fn a_test_tells_a_good_key_from_a_bad_one_without_a_billable_call() {
        let app = gateway();
        let base = upstream().await;
        local_provider(&app, &base, "bad").await;

        let (status, report) = call(&app, "POST", "/manage/providers/local/test", None).await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["access"], "denied", "{report}");

        let (status, _) = call(
            &app,
            "PATCH",
            "/manage/providers/local",
            Some(json!({ "credential": "good" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, report) = call(&app, "POST", "/manage/providers/local/test", None).await;
        assert_eq!(report["access"], "ready", "{report}");
        assert_eq!(report["models_listed"], 2);

        // A live test sends one real request, and says what it cost.
        let (_, report) = call(
            &app,
            "POST",
            "/manage/providers/local/test",
            Some(json!({ "model": "small", "live": true })),
        )
        .await;
        assert_eq!(report["live"]["ok"], true, "{report}");
        assert_eq!(report["live"]["usage"]["output_tokens"], 1);
    }

    #[tokio::test]
    async fn an_incomplete_provider_is_refused_with_the_field_that_is_missing() {
        let app = gateway();
        for (body, param) in [
            (
                json!({ "id": "x", "type": "openai-compatible", "reach": "self-hosted" }),
                "base_url",
            ),
            (
                json!({ "id": "x", "type": "openai-compatible", "base_url": "http://h/v1" }),
                "reach",
            ),
            (json!({ "id": "x", "type": "anthropic" }), "credential"),
            (json!({ "id": "x", "type": "nope" }), "type"),
            (
                json!({ "id": "a/b", "type": "anthropic", "credential": "k" }),
                "id",
            ),
            (
                json!({ "id": "x", "type": "anthropic", "credential": "k", "reach": "moon" }),
                "reach",
            ),
        ] {
            let (status, error) = call(&app, "POST", "/manage/providers", Some(body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {error}");
            assert_eq!(error["error"]["param"], param, "{body}: {error}");
        }
        // Nothing half made was left behind.
        let (_, listed) = call(&app, "GET", "/manage/providers", None).await;
        assert_eq!(listed["data"], json!([]));
    }

    #[tokio::test]
    async fn a_second_provider_with_the_same_id_is_a_conflict() {
        let app = gateway();
        let body = json!({ "id": "a", "type": "anthropic", "credential": "sk" });
        assert_eq!(
            call(&app, "POST", "/manage/providers", Some(body.clone()))
                .await
                .0,
            StatusCode::CREATED
        );
        assert_eq!(
            call(&app, "POST", "/manage/providers", Some(body)).await.0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn clearing_the_credential_of_an_enabled_vendor_provider_is_refused() {
        let app = gateway();
        call(
            &app,
            "POST",
            "/manage/providers",
            Some(json!({ "id": "a", "type": "anthropic", "credential": "sk" })),
        )
        .await;
        let (status, _) = call(
            &app,
            "PATCH",
            "/manage/providers/a",
            Some(json!({ "credential": null })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // Disabled, it may go without one.
        let (status, body) = call(
            &app,
            "PATCH",
            "/manage/providers/a",
            Some(json!({ "credential": null, "enabled": false })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["credential"], Value::Null);
    }

    #[tokio::test]
    async fn a_model_id_with_slashes_is_one_model() {
        let app = gateway();
        let base = upstream().await;
        local_provider(&app, &base, "good").await;
        let (status, body) = call(
            &app,
            "PUT",
            "/manage/providers/local/models/meta-llama/llama-3.1-8b",
            Some(json!({ "enabled": true, "capabilities": { "streaming": true } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["route"], "local/meta-llama/llama-3.1-8b");
    }

    /// Usage rows are written by a background task; wait until `n` have landed.
    async fn usage_after(app: &axum::Router, query: &str, n: i64) -> Value {
        for _ in 0..100 {
            let (_, body) = call(app, "GET", &format!("/manage/usage{query}"), None).await;
            if body["total"]["requests"].as_i64() == Some(n) {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("usage never reached {n} requests");
    }

    #[tokio::test]
    async fn every_request_is_recorded_with_its_outcome_tokens_and_cost() {
        let app = gateway();
        let base = upstream().await;
        local_provider(&app, &base, "good").await;
        call(
            &app,
            "PUT",
            "/manage/providers/local/models/small",
            Some(json!({ "enabled": true, "capabilities": {} })),
        )
        .await;
        call(
            &app,
            "PUT",
            "/manage/routes/default",
            Some(json!({ "routes": ["local/small"] })),
        )
        .await;

        // Answered, by a self hosted model: tokens counted, nothing charged.
        let (status, reply) = call(
            &app,
            "POST",
            "/v1/chat/completions",
            Some(json!({ "model": "default", "messages": [{ "role": "user", "content": "hi" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["llmr_cost"], json!({ "status": "free" }));

        // Refused before any provider was asked, and still recorded, by name.
        let (status, _) = call(
            &app,
            "POST",
            "/v1/chat/completions",
            Some(json!({ "model": "nothing", "messages": [{ "role": "user", "content": "hi" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let totals = usage_after(&app, "?group_by=model", 2).await;
        let total = &totals["total"];
        assert_eq!(total["outcomes"]["ok"], 1);
        assert_eq!(total["outcomes"]["error"], 1);
        assert_eq!(total["tokens"]["input"], 5);
        assert_eq!(total["tokens"]["output"], 1);
        assert_eq!(total["free"], 1);
        assert_eq!(total["cost"], json!([]));
        assert_eq!(total["cost_complete"], true);
        let groups: Vec<&Value> = totals["data"].as_array().unwrap().iter().collect();
        assert!(
            groups
                .iter()
                .any(|g| g["model"] == "local/small" && g["requests"] == 1),
            "{totals}"
        );
        assert!(
            groups
                .iter()
                .any(|g| g["model"].is_null() && g["requests"] == 1),
            "{totals}"
        );

        let (_, requests) = call(&app, "GET", "/manage/usage/requests?limit=1", None).await;
        let newest = &requests["data"][0];
        assert_eq!(newest["asked"], "nothing");
        assert_eq!(newest["outcome"], "error");
        assert_eq!(newest["error_code"], "model_not_found");
        assert!(newest["cost"].is_null());
        // One per page, and a pointer to the next.
        let before = requests["next_before"].as_i64().unwrap();
        let (_, older) = call(
            &app,
            "GET",
            &format!("/manage/usage/requests?limit=1&before={before}"),
            None,
        )
        .await;
        assert_eq!(older["data"][0]["route"], "local/small");
        assert_eq!(older["data"][0]["served_model"], "small");
        assert_eq!(older["data"][0]["request_id"], reply["id"]);

        // Filters narrow it.
        let (_, only_ok) = call(&app, "GET", "/manage/usage?outcome=ok", None).await;
        assert_eq!(only_ok["total"]["requests"], 1);
    }

    #[tokio::test]
    async fn usage_queries_refuse_what_they_cannot_answer() {
        let app = gateway();
        for path in [
            "/manage/usage?group_by=colour",
            "/manage/usage?outcome=maybe",
            "/manage/usage?from=10&to=5",
            "/manage/usage?nonsense=1",
            "/manage/usage/requests?limit=0",
            "/manage/usage/requests?group_by=day",
        ] {
            let (status, body) = call(&app, "GET", path, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
            assert_eq!(body["error"]["code"], "invalid_request", "{path}");
        }
        // Forgetting needs a cutoff: "delete everything" is one missing parameter away.
        let (status, _) = call(&app, "DELETE", "/manage/usage", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = call(&app, "DELETE", "/manage/usage?before=0", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted"], 0);
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_gets_the_same_error_envelope() {
        let request = Request::builder()
            .method("POST")
            .uri("/manage/providers")
            .header("content-type", "application/json")
            .body(Body::from("{nope"))
            .unwrap();
        let response = gateway().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn the_vendor_types_list_themselves_for_a_panel() {
        let (status, body) = call(&gateway(), "GET", "/manage/provider-types", None).await;
        assert_eq!(status, StatusCode::OK);
        let compatible = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == "openai-compatible")
            .unwrap();
        assert_eq!(compatible["reach_required"], true);
        assert_eq!(compatible["default_base_url"], Value::Null);
        let anthropic = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == "anthropic")
            .unwrap();
        assert_eq!(anthropic["default_reach"], "first-party-api");
    }

    #[tokio::test]
    async fn deleting_a_provider_takes_its_routes_out_of_service() {
        let app = gateway();
        let base = upstream().await;
        local_provider(&app, &base, "good").await;
        call(
            &app,
            "PUT",
            "/manage/providers/local/models/small",
            Some(json!({ "enabled": true, "capabilities": {} })),
        )
        .await;
        call(
            &app,
            "PUT",
            "/manage/routes/default",
            Some(json!({ "routes": ["local/small"] })),
        )
        .await;

        assert_eq!(
            call(&app, "DELETE", "/manage/providers/local", None)
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let (_, route) = call(&app, "GET", "/manage/routes/default", None).await;
        assert_eq!(route["usable"], json!([]));
        assert_eq!(route["unavailable"][0]["why"], "no such provider");
        let (status, _) = call(
            &app,
            "POST",
            "/v1/chat/completions",
            Some(json!({ "model": "default", "messages": [{ "role": "user", "content": "hi" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// A gateway whose image has Claude Code as a script that prints a recorded answer.
    #[cfg(unix)]
    fn gateway_with_claude_code(name: &str) -> (axum::Router, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("llmr-manage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let image = root.join("image");
        crate::cli::fake_tool(
            &image,
            crate::cli::Tool::ClaudeCode,
            &format!(
                "cat > /dev/null\ncat <<'RECORDED'\n{}\nRECORDED",
                include_str!("recorded/claude-code.ok.stdout").trim()
            ),
        );
        let tools = Toolbox::new(image, &root.join("data"));
        tools.prepare().unwrap();
        (gateway_with(tools), root)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_line_tool_is_set_up_and_served_like_any_provider() {
        let (app, root) = gateway_with_claude_code("flow");

        let (status, types) = call(&app, "GET", "/manage/provider-types", None).await;
        assert_eq!(status, StatusCode::OK);
        let claude = types["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == "claude-code")
            .unwrap();
        assert_eq!(claude["transport"], "cli");
        assert_eq!(claude["default_reach"], "local-cli");

        let (status, tools) = call(&app, "GET", "/manage/clis", None).await;
        assert_eq!(status, StatusCode::OK);
        let listed = tools["data"].as_array().unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0]["name"], "claude-code");
        assert_eq!(listed[0]["installed"], true);
        assert_eq!(listed[0]["version"], "9.9.9");
        assert_eq!(listed[0]["source"], "image");
        assert_eq!(listed[1]["installed"], false);

        // No base URL needed, and a reach that is not local-cli is refused.
        let (status, refused) = call(
            &app,
            "POST",
            "/manage/providers",
            Some(json!({ "id": "cc", "type": "claude-code", "credential": "sk-ant-x", "reach": "self-hosted" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(refused["error"]["param"], "reach");
        let (status, created) = call(
            &app,
            "POST",
            "/manage/providers",
            Some(json!({ "id": "cc", "type": "claude-code", "credential": "sk-ant-x" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(created["transport"], "cli");
        assert_eq!(created["reach"], "local-cli");

        // A model needs no capabilities, and is refused any: the tool carries text only.
        let (status, refused) = call(
            &app,
            "PUT",
            "/manage/providers/cc/models/claude-haiku-4-5",
            Some(json!({ "enabled": true, "capabilities": { "tools": true } })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(refused["error"]["param"], "capabilities");
        // Nothing a tool could read as a flag.
        let (status, refused) = call(
            &app,
            "PUT",
            "/manage/providers/cc/models/--dangerously-skip-permissions",
            Some(json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(refused["error"]["param"], "model");
        let (status, _) = call(
            &app,
            "PUT",
            "/manage/providers/cc/models/claude-haiku-4-5",
            Some(json!({ "enabled": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // The free test says what it can: installed, and the key unproven.
        let (status, report) = call(
            &app,
            "POST",
            "/manage/providers/cc/test",
            Some(json!({ "model": "claude-haiku-4-5" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(report["access"], "unknown", "{report}");
        assert_eq!(report["cli"]["version"], "9.9.9");

        // Served, priced from Anthropic's book: 11 in at $1, 4 out at $5, per million.
        let (status, reply) = call(
            &app,
            "POST",
            "/v1/chat/completions",
            Some(json!({ "model": "cc/claude-haiku-4-5", "messages": [{ "role": "user", "content": "hi" }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["choices"][0]["message"]["content"], "Hello from mock");
        assert_eq!(reply["usage"]["prompt_tokens"], 11);
        assert_eq!(reply["usage"]["completion_tokens"], 4);
        assert_eq!(reply["llmr_cost"]["status"], "priced", "{reply}");
        assert_eq!(reply["llmr_cost"]["amount"], "0.000031");

        // A request the tool cannot carry is refused, not flattened.
        let (status, _) = call(
            &app,
            "POST",
            "/v1/chat/completions",
            Some(json!({
                "model": "cc/claude-haiku-4-5",
                "messages": [{ "role": "user", "content": "hi" }],
                "tools": [{ "type": "function", "function": { "name": "f", "parameters": {} } }],
            })),
        )
        .await;
        assert!(status.is_client_error(), "{status}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_tool_update_takes_only_a_version_and_reset_is_harmless_without_one() {
        let (app, root) = gateway_with_claude_code("update");
        for bad in [
            "^2.0.0",
            "2.0",
            "next",
            "file:../x",
            "2.0.0 --registry=http://evil",
        ] {
            let (status, refused) = call(
                &app,
                "POST",
                "/manage/clis/claude-code/update",
                Some(json!({ "version": bad })),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(refused["error"]["param"], "version");
        }
        let (status, _) = call(&app, "POST", "/manage/clis/nope/update", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, reset) = call(&app, "POST", "/manage/clis/claude-code/reset", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reset["removed_update"], false);
        assert_eq!(reset["source"], "image");
        let _ = std::fs::remove_dir_all(&root);
    }
}
