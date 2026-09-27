//! The HTTP surface: the OpenAI-shaped client API, and the management API beside it.
//!
//! | Method | Path | What |
//! |---|---|---|
//! | `GET`  | `/healthz` | Liveness. No token needed, so an orchestrator can ask. |
//! | `GET`  | `/v1/models` | The names this gateway serves. |
//! | `POST` | `/v1/chat/completions` | A chat call, whole or streamed. |
//! | `POST` | `/v1/embeddings` | Text as vectors. See `media.rs` for this and the next three. |
//! | `POST` | `/v1/images/generations` | Pictures from a prompt. |
//! | `POST` | `/v1/audio/speech` | Text read aloud. |
//! | `POST` | `/v1/audio/transcriptions` | A recording written down. |
//! | *      | `/manage/...` | Providers, models, route sets and status; see `manage.rs`. |
//!
//! Nothing here logs a prompt, a reply or a key. A request is logged as the name asked for,
//! the route that answered, how many attempts it took, the token counts and the stop reason.

use crate::error::ApiError;
use crate::gateway::{Live, Resolution};
use crate::openai::{self, ChunkWriter};
use crate::store::Db;
use crate::usage::{Cost, Outcome, Recorder, Tokens, UsageRecord};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use llmr::{Event, Requirements, Routed, Transcript};
use serde_json::{json, Value};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

/// What every handler shares.
pub struct AppState {
    /// The gateway serving requests, replaced whole when the management API changes it.
    pub live: Live,
    /// Providers, models and route sets.
    pub db: Db,
    /// Tokens a caller may present. Empty when `LLMR_TOKEN` is unset, and then the API is
    /// open to whoever can reach the port.
    pub keys: Vec<String>,
    /// When the process started, for the status endpoint.
    pub started: std::time::Instant,
    /// Where each request's usage row goes.
    pub recorder: Recorder,
}

/// Milliseconds since `started`, for a usage row.
pub(crate) fn elapsed_ms(started: std::time::Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// The route an answer came from, split for a usage row.
fn route_parts(route: &str) -> (Option<String>, Option<String>) {
    match crate::records::split_route(route) {
        Some((provider, model)) => (Some(provider.to_string()), Some(model.to_string())),
        None => (None, None),
    }
}

/// A usage row for a request some provider answered.
#[allow(clippy::too_many_arguments)]
pub(crate) fn answered_row(
    id: &str,
    asked: &str,
    stream: bool,
    route: &str,
    served: &str,
    outcome: Outcome,
    stop: &str,
    attempts: u32,
    fell_through: usize,
    started: std::time::Instant,
    usage: &llmr::Usage,
    cost: Cost,
) -> UsageRecord {
    let (provider_id, model) = route_parts(route);
    UsageRecord {
        at: crate::usage::now(),
        request_id: id.to_string(),
        asked: asked.to_string(),
        route: Some(route.to_string()),
        provider_id,
        model,
        served_model: Some(served.to_string()),
        stream,
        outcome,
        error_code: None,
        stop_reason: Some(stop.to_string()),
        attempts: i64::from(attempts),
        fell_through: i64::try_from(fell_through).unwrap_or(i64::MAX),
        latency_ms: elapsed_ms(started),
        tokens: Tokens::from_usage(usage),
        cost,
    }
}

/// The whole application, ready to serve.
pub fn app(state: Arc<AppState>, max_body_bytes: usize) -> axum::Router {
    let protected = axum::Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/embeddings", post(crate::media::embeddings))
        .route("/v1/images/generations", post(crate::media::images))
        .route("/v1/audio/speech", post(crate::media::speech))
        .route(
            "/v1/audio/transcriptions",
            post(crate::media::transcriptions),
        )
        .merge(crate::manage::routes())
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));

    axum::Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        .fallback(|| async {
            ApiError {
                status: StatusCode::NOT_FOUND,
                kind: "invalid_request_error",
                code: "not_found",
                param: None,
                message: "no such endpoint".into(),
                retry_after: None,
            }
        })
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

/// Refuses a call without a key this gateway was given.
async fn authenticate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if state.keys.is_empty() {
        return next.run(request).await;
    }
    let headers = request.headers();
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .map(str::trim);

    match presented {
        Some(key)
            if state
                .keys
                .iter()
                .any(|k| same(k.as_bytes(), key.as_bytes())) =>
        {
            next.run(request).await
        }
        _ => ApiError::unauthorized().into_response(),
    }
}

/// Compares two keys in time that does not depend on where they first differ.
///
/// The length still shows, which is not a secret worth protecting: every key this gateway
/// is given is the operator's own choice of length.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let gateway = state.live.current();
    // `llmr_kind` says which endpoint a name belongs to; the OpenAI shape has no field for it.
    let entry = |id: &str, kind: crate::records::Kind| {
        json!({
            "id": id,
            "object": "model",
            "created": 0,
            "owned_by": "llmr",
            "llmr_kind": kind,
        })
    };
    // Route sets first, because those are the names a client is meant to use; then every
    // enabled model, addressable directly as provider/model.
    let data: Vec<Value> = gateway
        .served()
        .map(|served| entry(&served.name, served.kind))
        .chain(
            gateway
                .enabled()
                .map(|id| entry(id, gateway.kind_of(id).unwrap_or_default())),
        )
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

/// A process-unique id for one reply.
fn completion_id() -> String {
    request_id("chatcmpl")
}

/// A process-unique id for one request, after a prefix saying what it was.
pub(crate) fn request_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "{prefix}-{nanos:x}{:x}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Whether the caller asked, by header, for the data to stay on device.
///
/// Only ever tightens. A header cannot loosen a name configured `on_device`, or any caller
/// could send a private prompt to a vendor by adding one line.
pub(crate) fn wants_on_device(headers: &HeaderMap) -> bool {
    headers
        .get("x-llmr-on-device")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn route_headers<T>(response: &mut Response, routed: &Routed<T>) {
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&routed.route) {
        headers.insert("x-llmr-route", value);
    }
    headers.insert("x-llmr-attempts", HeaderValue::from(routed.attempts));
    headers.insert(
        "x-llmr-fell-through",
        HeaderValue::from(routed.fell_through.len()),
    );
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let started = std::time::Instant::now();
    let id = completion_id();
    let body: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::invalid(format!("the body is not JSON: {e}")))?;
    // Whatever the client asked for, recorded even when the request is refused, so a panel
    // can see who is asking for a model that is off.
    let asked = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let refuse = |error: ApiError| {
        state.recorder.record(UsageRecord::failed(
            &id,
            &asked,
            streaming,
            error.code,
            elapsed_ms(started),
        ));
        error
    };

    let incoming = openai::read_request(&body).map_err(refuse)?;
    // One gateway for the whole request, so a change made while it runs cannot price it
    // against providers it never used.
    let gateway = state.live.current();
    let (router, on_device) = match gateway.resolve(&asked) {
        Resolution::Found(router, on_device) => (router, on_device),
        Resolution::NotEnabled(why) => {
            return Err(refuse(ApiError::model_not_enabled(&asked, why)))
        }
        Resolution::WrongKind(kind) => return Err(refuse(ApiError::wrong_endpoint(&asked, kind))),
        Resolution::Unknown => return Err(refuse(ApiError::model_not_found(&asked))),
    };

    let mut needs = Requirements::of(&incoming.request);
    if on_device || wants_on_device(&headers) {
        needs = needs.on_device();
    }

    let created = now();

    if incoming.stream {
        return Ok(stream(Streamed {
            router,
            gateway,
            recorder: state.recorder.clone(),
            request: incoming.request,
            needs,
            include_usage: incoming.include_usage,
            id,
            created,
            asked,
            started,
        })
        .await);
    }

    match router.chat(incoming.request, needs).await {
        Ok(routed) => {
            let reply = &routed.response;
            let cost = gateway.cost(&routed.route, &reply.model, &reply.usage);
            let stop = openai::stop_name(reply.stop_reason);
            tracing::info!(
                model = %asked,
                route = %routed.route,
                attempts = routed.attempts,
                fell_through = routed.fell_through.len(),
                input_tokens = reply.usage.prompt_tokens(),
                output_tokens = reply.usage.output_tokens,
                stop = stop,
                cost = cost.header().unwrap_or_else(|| cost.status().unwrap_or("none").to_string()),
                "answered"
            );
            for attempt in &routed.fell_through {
                tracing::warn!(model = %asked, route = %attempt.route, why = %attempt.why, "fell through");
            }

            let mut body = openai::write_response(&id, created, reply);
            body["llmr_cost"] = cost.view();
            let mut response = Json(body).into_response();
            route_headers(&mut response, &routed);
            if let Some(value) = cost.header().and_then(|h| HeaderValue::from_str(&h).ok()) {
                response.headers_mut().insert("x-llmr-cost", value);
            }

            let outcome = if reply.stop_reason == llmr::StopReason::Refusal {
                Outcome::Refused
            } else {
                Outcome::Ok
            };
            state.recorder.record(answered_row(
                &id,
                &asked,
                false,
                &routed.route,
                reply.model.as_str(),
                outcome,
                stop,
                routed.attempts,
                routed.fell_through.len(),
                started,
                &reply.usage,
                cost,
            ));
            Ok(response)
        }
        // A refusal is an answer, and this shape writes one as a successful reply with no
        // content. An error status would send a client's retry logic asking again.
        Err(llmr::Error::Refused { category }) => {
            tracing::info!(model = %asked, "refused");
            let mut row = UsageRecord::failed(&id, &asked, false, "refused", elapsed_ms(started));
            row.outcome = Outcome::Refused;
            row.error_code = None;
            state.recorder.record(row);
            Ok(Json(openai::write_refusal(
                &id,
                created,
                &asked,
                category.as_deref(),
            ))
            .into_response())
        }
        Err(error) => {
            tracing::warn!(model = %asked, error = %error, "failed");
            Err(refuse(error.into()))
        }
    }
}

/// A body fed from a channel, so a spawned task can own the upstream stream.
struct Channel(mpsc::Receiver<Bytes>);

impl futures_core::Stream for Channel {
    type Item = Result<Bytes, std::convert::Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx).map(|chunk| chunk.map(Ok))
    }
}

fn frame(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

fn sse(receiver: mpsc::Receiver<Bytes>) -> Response {
    let mut response = Response::new(Body::from_stream(Channel(receiver)));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // Tells a buffering proxy in front of this one to pass chunks through as they arrive.
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// How often a comment is sent while a stream is quiet, so a proxy does not close it while
/// a model is thinking.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// A streamed call.
///
/// The router is asked in a spawned task that owns it, and the first thing that task says is
/// whether a route was found. A failure there is still an ordinary error status, because
/// nothing has been written yet; a failure after the first event can only arrive inside the
/// stream, which is where the router's own rule puts it.
/// Everything a streamed call needs, moved into the task that owns the upstream stream.
struct Streamed {
    router: Arc<llmr::Router>,
    gateway: Arc<crate::gateway::Gateway>,
    recorder: Recorder,
    request: llmr::ChatRequest,
    needs: Requirements,
    include_usage: bool,
    id: String,
    created: u64,
    asked: String,
    started: std::time::Instant,
}

async fn stream(call: Streamed) -> Response {
    let Streamed {
        router,
        gateway,
        recorder,
        request,
        needs,
        include_usage,
        id,
        created,
        asked,
        started,
    } = call;
    let (ready_tx, ready_rx) = oneshot::channel::<Result<Routed<()>, llmr::Error>>();
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let model_for_task = asked.clone();
    let refusal_id = id.clone();
    let task_recorder = recorder.clone();

    tokio::spawn(async move {
        let recorder = task_recorder;
        let asked = model_for_task;
        let (mut events, routed) = match router.stream(request, needs).await {
            Ok(opened) => opened,
            Err(error) => {
                // Recorded here rather than by the handler, which only sees the error after
                // this task has handed it over.
                let row = match &error {
                    llmr::Error::Refused { .. } => {
                        let mut row =
                            UsageRecord::failed(&id, &asked, true, "refused", elapsed_ms(started));
                        row.outcome = Outcome::Refused;
                        row.error_code = None;
                        row
                    }
                    other => UsageRecord::failed(
                        &id,
                        &asked,
                        true,
                        ApiError::from(other).code,
                        elapsed_ms(started),
                    ),
                };
                recorder.record(row);
                let _ = ready_tx.send(Err(error));
                return;
            }
        };
        let route = routed.route.clone();
        let attempts = routed.attempts;
        let fell_through = routed.fell_through.len();
        if ready_tx.send(Ok(routed)).is_err() {
            return;
        }

        let mut writer = ChunkWriter::new(id.clone(), created, asked.clone());
        let mut transcript = Transcript::new(asked.as_str());
        let mut failure = None;
        let mut client_left = false;
        let mut quiet = tokio::time::interval(KEEP_ALIVE);
        quiet.tick().await;

        'events: loop {
            let next = tokio::select! {
                next = std::future::poll_fn(|cx| events.as_mut().poll_next(cx)) => next,
                _ = quiet.tick() => {
                    if tx.send(Bytes::from_static(b": keep-alive\n\n")).await.is_err() {
                        client_left = true;
                        break 'events;
                    }
                    continue;
                }
                // The client went away while the provider was quiet. Leaving the loop drops
                // the upstream stream, which closes that connection now rather than paying
                // for tokens nobody will read.
                () = tx.closed() => {
                    client_left = true;
                    break 'events;
                }
            };
            match next {
                Some(Ok(event)) => {
                    quiet.reset();
                    let chunks = writer.write(&event);
                    transcript.push(event);
                    for chunk in chunks {
                        if tx.send(frame(&chunk)).await.is_err() {
                            client_left = true;
                            break 'events;
                        }
                    }
                }
                Some(Err(error)) => {
                    failure = Some(error);
                    break;
                }
                None => break,
            }
        }
        // Dropped before the row is written, so the provider connection closes first.
        drop(events);

        // A stream that ended without saying why the model stopped did not finish, whatever
        // the transport thinks. Sending `[DONE]` after it would hand a client half an answer
        // marked complete.
        if !client_left && failure.is_none() && !transcript.is_finished() {
            failure = Some(llmr::Error::Transient(
                "the provider's stream ended before the model said it was done".into(),
            ));
        }
        let reply = transcript.finish();
        let cost = gateway.cost(&route, &reply.model, &reply.usage);
        let stop = openai::stop_name(reply.stop_reason);
        let outcome = match (client_left || failure.is_some(), reply.stop_reason) {
            (true, _) => Outcome::Interrupted,
            (false, llmr::StopReason::Refusal) => Outcome::Refused,
            (false, _) => Outcome::Ok,
        };
        // What arrived was delivered and is counted, however the stream ended.
        recorder.record(answered_row(
            &id,
            &asked,
            true,
            &route,
            reply.model.as_str(),
            outcome,
            stop,
            attempts,
            fell_through,
            started,
            &reply.usage,
            cost.clone(),
        ));

        if client_left {
            tracing::info!(model = %asked, route = %route, "client left mid-stream");
            return;
        }
        if let Some(error) = failure {
            tracing::warn!(model = %asked, route = %route, error = %error, "stream broke");
            // What arrived is still the client's. The error says why the rest is not
            // coming, and no `[DONE]` follows, so no client reads the reply as finished.
            let _ = tx.send(frame(&ApiError::from(error).body())).await;
            return;
        }

        tracing::info!(
            model = %asked,
            route = %route,
            attempts,
            input_tokens = reply.usage.prompt_tokens(),
            output_tokens = reply.usage.output_tokens,
            stop = stop,
            cost = cost.header().unwrap_or_else(|| cost.status().unwrap_or("none").to_string()),
            "streamed"
        );
        if include_usage {
            if let Some(mut chunk) = writer.usage_chunk(&reply.usage) {
                chunk["llmr_cost"] = cost.view();
                let _ = tx.send(frame(&chunk)).await;
            }
        }
        let _ = tx.send(Bytes::from_static(b"data: [DONE]\n\n")).await;
    });

    match ready_rx.await {
        Ok(Ok(routed)) => {
            let mut response = sse(rx);
            route_headers(&mut response, &routed);
            response
        }
        Ok(Err(llmr::Error::Refused { category })) => {
            let (tx, rx) = mpsc::channel(4);
            let mut writer = ChunkWriter::new(refusal_id, created, asked);
            let frames = writer.write(&Event::Stopped {
                reason: llmr::StopReason::Refusal,
                details: category,
            });
            for chunk in frames {
                let _ = tx.try_send(frame(&chunk));
            }
            let _ = tx.try_send(Bytes::from_static(b"data: [DONE]\n\n"));
            sse(rx)
        }
        Ok(Err(error)) => {
            tracing::warn!(model = %asked, error = %error, "failed");
            ApiError::from(error).into_response()
        }
        Err(_) => ApiError::internal("the routing task ended without an answer").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use llmr::chat::{ContentBlock, Message, Role, StopReason};
    use llmr::{
        ChatRequest, ChatResponse, ModelCapabilities, ModelId, Provider, Reach, Route, Router,
        Usage,
    };
    use tower::ServiceExt;

    /// Answers every request the same way, and says so in its reply.
    struct Fake {
        id: &'static str,
        reach: Reach,
        answer: Result<&'static str, fn() -> llmr::Error>,
    }

    #[async_trait]
    impl Provider for Fake {
        fn id(&self) -> &str {
            self.id
        }

        fn capabilities(&self, _model: &ModelId) -> Option<ModelCapabilities> {
            Some(
                ModelCapabilities::none(self.reach)
                    .with_tools()
                    .with_streaming(),
            )
        }

        async fn chat(&self, request: ChatRequest) -> llmr::Result<ChatResponse> {
            let text = self.answer.map_err(|make| make())?;
            Ok(ChatResponse::new(
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text(text.into())],
                },
                StopReason::EndTurn,
                Usage::absent().with_input(12).with_output(3),
                request.model,
            ))
        }
    }

    fn fake(
        id: &'static str,
        reach: Reach,
        answer: Result<&'static str, fn() -> llmr::Error>,
    ) -> Arc<dyn Provider> {
        Arc::new(Fake { id, reach, answer })
    }

    fn app_with(served: Vec<(&str, Router, bool)>) -> axum::Router {
        let db = Db::new(
            crate::store::Store::in_memory(
                crate::crypto::MasterKey::from_base64(
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                )
                .unwrap(),
            )
            .unwrap(),
        );
        app(
            Arc::new(AppState {
                live: Live::new(crate::gateway::Gateway::from_routers(served)),
                db: db.clone(),
                keys: vec!["secret".into()],
                started: std::time::Instant::now(),
                recorder: crate::usage::Recorder::start(db.clone()).0,
            }),
            1024 * 1024,
        )
    }

    fn one_route() -> axum::Router {
        app_with(vec![(
            "default",
            Router::new(vec![Route::new(
                fake("hosted", Reach::FirstPartyApi, Ok("Hello there")),
                "m1",
            )]),
            false,
        )])
    }

    fn post(body: Value) -> Request {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer secret")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn read(response: Response) -> (StatusCode, HeaderMap, String) {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn hello(model: &str) -> Value {
        json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] })
    }

    #[tokio::test]
    async fn health_needs_no_key_and_everything_else_does() {
        let app = one_route();
        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let bare = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);

        let wrong = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("authorization", "Bearer secreT")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let (status, _, body) = read(
            app.oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("x-api-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let listed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed["data"][0]["id"], "default");
    }

    #[tokio::test]
    async fn a_whole_reply_comes_back_in_the_openai_shape_with_its_route() {
        let (status, headers, body) =
            read(one_route().oneshot(post(hello("default"))).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["object"], "chat.completion");
        assert_eq!(reply["choices"][0]["message"]["content"], "Hello there");
        assert_eq!(reply["choices"][0]["finish_reason"], "stop");
        assert_eq!(reply["usage"]["total_tokens"], 15);
        // The route's model, not the name the client asked for.
        assert_eq!(reply["model"], "m1");
        assert_eq!(headers["x-llmr-route"], "hosted/m1");
        assert_eq!(headers["x-llmr-attempts"], "1");
    }

    #[tokio::test]
    async fn an_unknown_name_is_a_404_naming_it() {
        let (status, _, body) = read(one_route().oneshot(post(hello("nope"))).await.unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("model_not_found"), "{body}");
    }

    #[tokio::test]
    async fn a_failing_route_falls_through_and_the_headers_say_so() {
        let app = app_with(vec![(
            "default",
            Router::new(vec![
                Route::new(
                    fake(
                        "down",
                        Reach::FirstPartyApi,
                        Err(|| llmr::Error::Transient("503".into())),
                    ),
                    "m1",
                ),
                Route::new(
                    fake("up", Reach::FirstPartyApi, Ok("from the second")),
                    "m2",
                ),
            ]),
            false,
        )]);
        let (status, headers, body) =
            read(app.oneshot(post(hello("default"))).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(headers["x-llmr-route"], "up/m2");
        assert_eq!(headers["x-llmr-fell-through"], "1");
    }

    #[tokio::test]
    async fn every_route_failing_returns_the_last_error_as_a_status() {
        let app = app_with(vec![(
            "default",
            Router::new(vec![Route::new(
                fake(
                    "limited",
                    Reach::FirstPartyApi,
                    Err(|| llmr::Error::RateLimited {
                        retry_after: Some(Duration::from_secs(7)),
                    }),
                ),
                "m1",
            )]),
            false,
        )]);
        let (status, headers, _) = read(app.oneshot(post(hello("default"))).await.unwrap()).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(headers["retry-after"], "7");
    }

    #[tokio::test]
    async fn a_refusal_is_a_successful_reply_with_no_content() {
        let app = app_with(vec![(
            "default",
            Router::new(vec![Route::new(
                fake(
                    "careful",
                    Reach::FirstPartyApi,
                    Err(|| llmr::Error::Refused { category: None }),
                ),
                "m1",
            )]),
            false,
        )]);
        let (status, _, body) = read(app.oneshot(post(hello("default"))).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let reply: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(reply["choices"][0]["finish_reason"], "content_filter");
        assert!(reply["choices"][0]["message"]["content"].is_null());
    }

    #[tokio::test]
    async fn the_on_device_header_keeps_a_prompt_off_a_hosted_route() {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer secret")
            .header("x-llmr-on-device", "true")
            .body(Body::from(hello("default").to_string()))
            .unwrap();
        let (status, _, body) = read(one_route().oneshot(request).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("on-device"), "{body}");
    }

    #[tokio::test]
    async fn a_name_configured_on_device_never_reaches_a_hosted_route() {
        let app = app_with(vec![(
            "private",
            Router::new(vec![Route::new(
                fake("hosted", Reach::FirstPartyApi, Ok("leaked")),
                "m1",
            )]),
            true,
        )]);
        let (status, _, body) = read(app.oneshot(post(hello("private"))).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(!body.contains("leaked"));
    }

    #[tokio::test]
    async fn a_stream_is_server_sent_events_ending_in_usage_and_done() {
        let mut body = hello("default");
        body["stream"] = json!(true);
        body["stream_options"] = json!({ "include_usage": true });
        let (status, headers, text) = read(one_route().oneshot(post(body)).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "text/event-stream");
        assert_eq!(headers["x-llmr-route"], "hosted/m1");

        let frames: Vec<&str> = text
            .split("\n\n")
            .filter_map(|f| f.strip_prefix("data: "))
            .collect();
        assert_eq!(frames.last(), Some(&"[DONE]"));
        let chunks: Vec<Value> = frames[..frames.len() - 1]
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect();
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        let text: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "Hello there");
        assert!(chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "stop"));
        assert_eq!(chunks.last().unwrap()["usage"]["completion_tokens"], 3);
    }

    #[tokio::test]
    async fn a_stream_that_cannot_start_is_an_ordinary_error_status() {
        let app = app_with(vec![(
            "default",
            Router::new(vec![Route::new(
                fake(
                    "down",
                    Reach::FirstPartyApi,
                    Err(|| llmr::Error::Transient("503".into())),
                ),
                "m1",
            )]),
            false,
        )]);
        let mut body = hello("default");
        body["stream"] = json!(true);
        let (status, headers, _) = read(app.oneshot(post(body)).await.unwrap()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_ne!(
            headers.get("content-type").map(|v| v.as_bytes()),
            Some(&b"text/event-stream"[..])
        );
    }

    /// Streams some text and then ends, without ever saying why the model stopped.
    struct CutShort;

    #[async_trait]
    impl Provider for CutShort {
        fn id(&self) -> &str {
            "cut"
        }

        fn capabilities(&self, _model: &ModelId) -> Option<ModelCapabilities> {
            Some(ModelCapabilities::none(Reach::FirstPartyApi).with_streaming())
        }

        async fn chat(&self, _request: ChatRequest) -> llmr::Result<ChatResponse> {
            Err(llmr::Error::Unsupported("stream only".into()))
        }

        async fn stream(&self, _request: ChatRequest) -> llmr::Result<llmr::EventStream<'_>> {
            struct Two(Vec<Event>);
            impl futures_core::Stream for Two {
                type Item = llmr::Result<Event>;
                fn poll_next(
                    mut self: Pin<&mut Self>,
                    _cx: &mut Context<'_>,
                ) -> Poll<Option<Self::Item>> {
                    Poll::Ready(if self.0.is_empty() {
                        None
                    } else {
                        Some(Ok(self.0.remove(0)))
                    })
                }
            }
            Ok(Box::pin(Two(vec![
                Event::Started { model: "m1".into() },
                Event::TextDelta("half an answ".into()),
            ])))
        }
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_stop_reason_is_never_marked_done() {
        let app = app_with(vec![(
            "default",
            Router::new(vec![Route::new(Arc::new(CutShort), "m1")]),
            false,
        )]);
        let mut body = hello("default");
        body["stream"] = json!(true);
        let (status, _, text) = read(app.oneshot(post(body)).await.unwrap()).await;
        // The status went out with the first byte; what follows can only say so in-band.
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("half an answ"), "{text}");
        assert!(text.contains("\"error\""), "{text}");
        assert!(!text.contains("[DONE]"), "{text}");
    }

    #[tokio::test]
    async fn a_stream_is_recorded_once_it_ends() {
        let db = Db::new(
            crate::store::Store::in_memory(
                crate::crypto::MasterKey::from_base64(
                    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                )
                .unwrap(),
            )
            .unwrap(),
        );
        let app = app(
            Arc::new(AppState {
                live: Live::new(crate::gateway::Gateway::from_routers(vec![(
                    "default",
                    Router::new(vec![Route::new(
                        fake("hosted", Reach::FirstPartyApi, Ok("Hello there")),
                        "m1",
                    )]),
                    false,
                )])),
                db: db.clone(),
                keys: vec!["secret".into()],
                started: std::time::Instant::now(),
                recorder: crate::usage::Recorder::start(db.clone()).0,
            }),
            1024 * 1024,
        );
        let mut body = hello("default");
        body["stream"] = json!(true);
        let (status, _, _) = read(app.oneshot(post(body)).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);

        let mut rows = Vec::new();
        for _ in 0..100 {
            rows = db
                .run(|s| s.usage_requests(&crate::usage::UsageFilter::default(), None, 10))
                .await
                .unwrap();
            if !rows.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (_, row) = rows.first().expect("a usage row");
        assert!(row.stream);
        assert_eq!(row.outcome, Outcome::Ok);
        assert_eq!(row.route.as_deref(), Some("hosted/m1"));
        assert_eq!(row.tokens.input, Some(12));
        assert_eq!(row.tokens.output, Some(3));
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_is_a_400() {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer secret")
            .body(Body::from("{nope"))
            .unwrap();
        let (status, _, _) = read(one_route().oneshot(request).await.unwrap()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn keys_compare_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
    }
}
