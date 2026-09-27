//! The endpoints beside chat, in the OpenAI shape: embeddings, images, speech and
//! transcription.
//!
//! Each resolves a name the way chat does, to a route set or a `provider/model`, and tries
//! the routes in order with the set's attempts and deadline (see `Gateway::media`). A name of
//! another kind is told which endpoint it belongs to. Every request is recorded like a chat
//! call: the route, the attempts, the tokens where the provider reported them, and the cost.
//!
//! A field that cannot be carried is refused by name, as in chat, rather than dropped.

use crate::error::ApiError;
use crate::gateway::{Gateway, MediaResolution, MediaRouted, Plan};
use crate::records::Kind;
use crate::server::{answered_row, elapsed_ms, now, request_id, wants_on_device, AppState};
use crate::usage::{Cost, Outcome, UsageRecord};
use axum::body::Bytes;
use axum::extract::{Multipart, State};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use llmr::audio::{SpeechRequest, TranscriptionRequest};
use llmr::image::{ImageRequest, Picture};
use llmr::{EmbedRequest, ModelId, Units, Usage};
use serde_json::{json, Map, Value};
use std::sync::Arc;

type Shared = State<Arc<AppState>>;

/// One request to an endpoint that is not chat, from arrival to its usage row.
struct Call {
    state: Arc<AppState>,
    id: String,
    asked: String,
    started: std::time::Instant,
}

impl Call {
    fn new(state: Arc<AppState>, prefix: &str) -> Self {
        Self {
            state,
            id: request_id(prefix),
            asked: String::new(),
            started: std::time::Instant::now(),
        }
    }

    /// Records a refusal, and hands it back to be returned.
    fn refuse(&self, error: ApiError) -> ApiError {
        self.state.recorder.record(UsageRecord::failed(
            &self.id,
            &self.asked,
            false,
            error.code,
            elapsed_ms(self.started),
        ));
        error
    }

    /// Where a name may go, or why it may not.
    fn plan(&self, gateway: &Gateway, kind: Kind) -> Result<Plan, ApiError> {
        match gateway.resolve_media(&self.asked, kind) {
            MediaResolution::Found(plan) => Ok(plan),
            MediaResolution::NotEnabled(why) => {
                Err(self.refuse(ApiError::model_not_enabled(&self.asked, why)))
            }
            MediaResolution::WrongKind(found) => {
                Err(self.refuse(ApiError::wrong_endpoint(&self.asked, found)))
            }
            MediaResolution::Unknown => Err(self.refuse(ApiError::model_not_found(&self.asked))),
        }
    }

    /// A failure after the routes were tried.
    fn failed(&self, error: llmr::Error) -> ApiError {
        tracing::warn!(model = %self.asked, error = %error, "failed");
        if matches!(error, llmr::Error::Refused { .. }) {
            let mut row = UsageRecord::failed(
                &self.id,
                &self.asked,
                false,
                "refused",
                elapsed_ms(self.started),
            );
            row.outcome = Outcome::Refused;
            self.state.recorder.record(row);
            return ApiError::from(error);
        }
        self.refuse(ApiError::from(error))
    }

    /// Prices and records an answer, and returns the cost to put on the reply.
    fn answered<T>(
        &self,
        gateway: &Gateway,
        routed: &MediaRouted<T>,
        served: &ModelId,
        usage: &Usage,
        units: &Units,
    ) -> Cost {
        let cost = gateway.cost_with(&routed.route, served, usage, units);
        for (route, why) in &routed.fell_through {
            tracing::warn!(model = %self.asked, route = %route, why = %why, "fell through");
        }
        tracing::info!(
            model = %self.asked,
            route = %routed.route,
            attempts = routed.attempts,
            cost = cost.header().unwrap_or_else(|| cost.status().unwrap_or("none").to_string()),
            "answered"
        );
        let mut row = answered_row(
            &self.id,
            &self.asked,
            false,
            &routed.route,
            served.as_str(),
            Outcome::Ok,
            "",
            routed.attempts,
            routed.fell_through.len(),
            self.started,
            usage,
            cost.clone(),
        );
        // Nothing stops the way a chat reply does.
        row.stop_reason = None;
        self.state.recorder.record(row);
        cost
    }
}

/// The headers every answer carries: which route, how many attempts, what it cost.
fn with_headers<T>(
    mut response: Response,
    id: &str,
    routed: &MediaRouted<T>,
    cost: &Cost,
) -> Response {
    let headers = response.headers_mut();
    // The id the usage row carries, so a client's log and this one can be joined.
    if let Ok(value) = HeaderValue::from_str(id) {
        headers.insert("x-llmr-request-id", value);
    }
    if let Ok(value) = HeaderValue::from_str(&routed.route) {
        headers.insert("x-llmr-route", value);
    }
    headers.insert("x-llmr-attempts", HeaderValue::from(routed.attempts));
    headers.insert(
        "x-llmr-fell-through",
        HeaderValue::from(routed.fell_through.len()),
    );
    if let Some(value) = cost.header().and_then(|h| HeaderValue::from_str(&h).ok()) {
        headers.insert("x-llmr-cost", value);
    }
    response
}

/// A JSON body as an object, and its `model`.
fn read_json(call: &mut Call, body: &Bytes) -> Result<Map<String, Value>, ApiError> {
    let parsed: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::invalid(format!("the body is not JSON: {e}")))?;
    let Value::Object(object) = parsed else {
        return Err(ApiError::invalid("the body must be a JSON object"));
    };
    call.asked = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if call.asked.is_empty() {
        return Err(call.refuse(ApiError::invalid_param("model", "model is required")));
    }
    Ok(object)
}

/// Refuses any of these fields that was sent, by name.
fn refuse_fields(
    call: &Call,
    body: &Map<String, Value>,
    fields: &[(&str, &str)],
) -> Result<(), ApiError> {
    for (field, why) in fields {
        if body.get(*field).is_some_and(|v| !v.is_null()) {
            return Err(call.refuse(ApiError::invalid_param(field, *why)));
        }
    }
    Ok(())
}

fn string(call: &Call, body: &Map<String, Value>, field: &str) -> Result<Option<String>, ApiError> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(call.refuse(ApiError::invalid_param(
            field,
            format!("{field} must be a string"),
        ))),
    }
}

fn usage_view(usage: &Usage) -> Option<Value> {
    if usage.input_tokens.is_none() && usage.output_tokens.is_none() {
        return None;
    }
    let input = usage.input_tokens.unwrap_or(0);
    let output = usage.output_tokens.unwrap_or(0);
    Some(json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": input + output,
    }))
}

// ----- embeddings -----------------------------------------------------------------------

/// `POST /v1/embeddings`.
///
/// `input` is a string or an array of strings; token arrays are refused, because they are
/// one tokenizer's numbers and mean nothing to another vendor's model. `encoding_format`
/// `base64` is honoured as the OpenAI SDKs expect it: the little endian 32 bit floats.
pub async fn embeddings(
    State(state): Shared,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let mut call = Call::new(state, "emb");
    let body = read_json(&mut call, &body)?;
    let inputs: Vec<String> = match body.get("input") {
        Some(Value::String(one)) => vec![one.clone()],
        Some(Value::Array(many)) if many.iter().all(Value::is_string) && !many.is_empty() => many
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => {
            return Err(call.refuse(ApiError::invalid_param(
                "input",
                "input must be a string or a non-empty array of strings; token arrays are not \
                 carried, because they mean nothing to another vendor's model",
            )))
        }
    };
    let base64 = match string(&call, &body, "encoding_format")?.as_deref() {
        None | Some("float") => false,
        Some("base64") => true,
        Some(_) => {
            return Err(call.refuse(ApiError::invalid_param(
                "encoding_format",
                "encoding_format is float or base64",
            )))
        }
    };
    let dimensions = match body.get("dimensions") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|d| u32::try_from(d).ok())
                .filter(|d| *d > 0)
                .ok_or_else(|| {
                    call.refuse(ApiError::invalid_param(
                        "dimensions",
                        "dimensions must be a positive integer",
                    ))
                })?,
        ),
    };

    let gateway = call.state.live.current();
    let plan = call.plan(&gateway, Kind::Embedding)?;
    let routed = gateway
        .media(&plan, wants_on_device(&headers), |built, model| {
            let embedder = built.embedder.clone()?;
            let mut request = EmbedRequest::new(model, inputs.clone());
            if let Some(dimensions) = dimensions {
                request = request.with_dimensions(dimensions);
            }
            Some(async move { embedder.embed(request).await })
        })
        .await
        .map_err(|e| call.failed(e))?;

    let reply = &routed.value;
    let cost = call.answered(
        &gateway,
        &routed,
        &reply.model,
        &reply.usage,
        &Units::none(),
    );
    let data: Vec<Value> = reply
        .vectors
        .iter()
        .enumerate()
        .map(|(index, embedding)| {
            let vector = if base64 {
                let bytes: Vec<u8> = embedding
                    .vector
                    .iter()
                    .flat_map(|f| f.to_le_bytes())
                    .collect();
                use base64::Engine as _;
                json!(base64::engine::general_purpose::STANDARD.encode(bytes))
            } else {
                json!(embedding.vector)
            };
            json!({ "object": "embedding", "index": index, "embedding": vector })
        })
        .collect();
    let mut out = json!({
        "object": "list",
        "data": data,
        "model": reply.model.as_str(),
        "llmr_cost": cost.view(),
    });
    // Left out when the provider reported nothing, as chat does: absent rather than zero.
    if let Some(prompt) = reply.usage.input_tokens {
        out["usage"] = json!({ "prompt_tokens": prompt, "total_tokens": prompt });
    }
    Ok(with_headers(
        Json(out).into_response(),
        &call.id,
        &routed,
        &cost,
    ))
}

// ----- images ---------------------------------------------------------------------------

/// `POST /v1/images/generations`.
pub async fn images(
    State(state): Shared,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let mut call = Call::new(state, "img");
    let body = read_json(&mut call, &body)?;
    refuse_fields(
        &call,
        &body,
        &[
            (
                "stream",
                "streamed pictures are not carried; ask for the whole picture",
            ),
            (
                "partial_images",
                "streamed pictures are not carried; ask for the whole picture",
            ),
            (
                "style",
                "style is not carried to every provider; say it in the prompt",
            ),
            ("moderation", "moderation is not carried to every provider"),
            (
                "output_compression",
                "output_compression is not carried to every provider",
            ),
        ],
    )?;
    let Some(prompt) = string(&call, &body, "prompt")?.filter(|p| !p.trim().is_empty()) else {
        return Err(call.refuse(ApiError::invalid_param("prompt", "prompt is required")));
    };
    let count = match body.get("n") {
        None | Some(Value::Null) => None,
        Some(n) => Some(
            n.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (1..=10).contains(n))
                .ok_or_else(|| call.refuse(ApiError::invalid_param("n", "n is from 1 to 10")))?,
        ),
    };
    let size = string(&call, &body, "size")?;
    let quality = string(&call, &body, "quality")?;
    let background = string(&call, &body, "background")?;
    let output_format = string(&call, &body, "output_format")?;
    let response_format = string(&call, &body, "response_format")?;
    if response_format
        .as_deref()
        .is_some_and(|f| f != "url" && f != "b64_json")
    {
        return Err(call.refuse(ApiError::invalid_param(
            "response_format",
            "response_format is url or b64_json",
        )));
    }

    let gateway = call.state.live.current();
    let plan = call.plan(&gateway, Kind::Image)?;
    let routed = gateway
        .media(&plan, wants_on_device(&headers), |built, model| {
            let generator = built.images.clone()?;
            let mut request = ImageRequest::new(model, prompt.clone());
            request.count = count;
            request.size = size.clone();
            request.quality = quality.clone();
            request.background = background.clone();
            request.output_format = output_format.clone();
            request.response_format = response_format.clone();
            Some(async move { generator.generate(request).await })
        })
        .await
        .map_err(|e| call.failed(e))?;

    let reply = &routed.value;
    let pictures = u64::try_from(reply.pictures.len()).unwrap_or(u64::MAX);
    let cost = call.answered(
        &gateway,
        &routed,
        &reply.model,
        &reply.usage,
        &Units::none().with_images(pictures),
    );
    let data: Vec<Value> = reply
        .pictures
        .iter()
        .enumerate()
        .map(|(index, picture)| {
            let mut item = match picture {
                Picture::Bytes { data, .. } => {
                    use base64::Engine as _;
                    json!({ "b64_json": base64::engine::general_purpose::STANDARD.encode(data) })
                }
                Picture::Url(url) => json!({ "url": url }),
                _ => json!({}),
            };
            if index == 0 {
                if let Some(revised) = &reply.revised_prompt {
                    item["revised_prompt"] = json!(revised);
                }
            }
            item
        })
        .collect();
    let mut out = json!({
        "created": now(),
        "data": data,
        "llmr_cost": cost.view(),
    });
    // The format of the first picture that came as bytes, as the OpenAI shape reports it.
    if let Some(format) = reply.pictures.iter().find_map(|p| match p {
        Picture::Bytes { media_type, .. } => media_type.strip_prefix("image/").map(str::to_string),
        _ => None,
    }) {
        out["output_format"] = json!(format);
    }
    if let Some(usage) = usage_view(&reply.usage) {
        out["usage"] = usage;
    }
    Ok(with_headers(
        Json(out).into_response(),
        &call.id,
        &routed,
        &cost,
    ))
}

// ----- speech ---------------------------------------------------------------------------

/// `POST /v1/audio/speech`. The reply is the recording itself, typed by `Content-Type`.
pub async fn speech(
    State(state): Shared,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let mut call = Call::new(state, "tts");
    let body = read_json(&mut call, &body)?;
    if string(&call, &body, "stream_format")?.is_some_and(|f| f != "audio") {
        return Err(call.refuse(ApiError::invalid_param(
            "stream_format",
            "speech comes back whole, as audio; sse is not carried",
        )));
    }
    let Some(input) = string(&call, &body, "input")?.filter(|i| !i.trim().is_empty()) else {
        return Err(call.refuse(ApiError::invalid_param("input", "input is required")));
    };
    let voice = match body.get("voice") {
        Some(Value::String(voice)) => voice.clone(),
        Some(Value::Object(custom)) => match custom.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => {
                return Err(call.refuse(ApiError::invalid_param(
                    "voice",
                    "a voice object needs an id",
                )))
            }
        },
        _ => return Err(call.refuse(ApiError::invalid_param("voice", "voice is required"))),
    };
    let format = string(&call, &body, "response_format")?;
    let instructions = string(&call, &body, "instructions")?;
    let speed = match body.get("speed") {
        None | Some(Value::Null) => None,
        Some(speed) => Some(
            speed
                .as_f64()
                .filter(|s| (0.25..=4.0).contains(s))
                .ok_or_else(|| {
                    call.refuse(ApiError::invalid_param(
                        "speed",
                        "speed is from 0.25 to 4.0",
                    ))
                })? as f32,
        ),
    };

    let gateway = call.state.live.current();
    let plan = call.plan(&gateway, Kind::Speech)?;
    let routed = gateway
        .media(&plan, wants_on_device(&headers), |built, model| {
            let speaker = built.speech.clone()?;
            let mut request = SpeechRequest::new(model, input.clone(), voice.clone());
            request.format = format.clone();
            request.speed = speed;
            request.instructions = instructions.clone();
            Some(async move { speaker.speak(request).await })
        })
        .await
        .map_err(|e| call.failed(e))?;

    // What was read aloud, for a model sold by the character.
    let characters = u64::try_from(input.chars().count()).unwrap_or(u64::MAX);
    let cost = call.answered(
        &gateway,
        &routed,
        &routed.value.model,
        &routed.value.usage,
        &Units::none().with_characters(characters),
    );
    let media_type = routed.value.media_type.clone();
    let mut response = routed.value.data.clone().into_response();
    if let Ok(value) = HeaderValue::from_str(&media_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    Ok(with_headers(response, &call.id, &routed, &cost))
}

// ----- transcription --------------------------------------------------------------------

/// The media type of an uploaded recording: what the upload said, else its extension.
fn recording_type(declared: Option<&str>, name: &str) -> Option<String> {
    if let Some(declared) = declared.filter(|t| t.starts_with("audio/") || t.starts_with("video/"))
    {
        return Some(declared.to_string());
    }
    let name = name.to_ascii_lowercase();
    [
        (".mp3", "audio/mpeg"),
        (".mpga", "audio/mpeg"),
        (".mpeg", "audio/mpeg"),
        (".wav", "audio/wav"),
        (".flac", "audio/flac"),
        (".ogg", "audio/ogg"),
        (".webm", "audio/webm"),
        (".m4a", "audio/mp4"),
        (".mp4", "audio/mp4"),
    ]
    .iter()
    .find(|(extension, _)| name.ends_with(extension))
    .map(|(_, media_type)| (*media_type).to_string())
}

/// `POST /v1/audio/transcriptions`, a multipart form.
///
/// `response_format` is `json` or `text`. The timed formats (`srt`, `vtt`, `verbose_json`)
/// are refused: most transcription models cannot produce them, and a caller who needs the
/// timings needs to know that before the call rather than get plain text back.
pub async fn transcriptions(
    State(state): Shared,
    headers: HeaderMap,
    mut form: Multipart,
) -> Result<Response, ApiError> {
    let mut call = Call::new(state, "stt");
    let mut file: Option<(String, Option<String>, Vec<u8>)> = None;
    let mut fields: Map<String, Value> = Map::new();
    loop {
        let next = form.next_field().await.map_err(|e| {
            call.refuse(ApiError::invalid(format!(
                "the form could not be read: {e}"
            )))
        })?;
        let Some(field) = next else { break };
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            let file_name = field.file_name().unwrap_or("audio").to_string();
            let declared = field.content_type().map(str::to_string);
            let bytes = field.bytes().await.map_err(|e| {
                call.refuse(ApiError::invalid(format!(
                    "the file could not be read: {e}"
                )))
            })?;
            file = Some((file_name, declared, bytes.to_vec()));
        } else {
            let text = field.text().await.map_err(|e| {
                call.refuse(ApiError::invalid(format!(
                    "field {name} could not be read: {e}"
                )))
            })?;
            fields.insert(name, Value::String(text));
        }
    }
    call.asked = fields
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if call.asked.is_empty() {
        return Err(call.refuse(ApiError::invalid_param("model", "model is required")));
    }
    for (field, why) in [
        (
            "timestamp_granularities[]",
            "timestamps are not carried to every provider",
        ),
        ("include[]", "include is not carried to every provider"),
        ("stream", "a transcription comes back whole"),
        (
            "chunking_strategy",
            "chunking_strategy is not carried to every provider",
        ),
    ] {
        if fields.contains_key(field) {
            return Err(call.refuse(ApiError::invalid_param(field, why)));
        }
    }
    let text_reply = match fields.get("response_format").and_then(Value::as_str) {
        None | Some("json") => false,
        Some("text") => true,
        Some(_) => {
            return Err(call.refuse(ApiError::invalid_param(
                "response_format",
                "response_format is json or text; timed formats are not carried",
            )))
        }
    };
    let Some((file_name, declared, audio)) = file.filter(|(_, _, bytes)| !bytes.is_empty()) else {
        return Err(call.refuse(ApiError::invalid_param("file", "file is required")));
    };
    let Some(media_type) = recording_type(declared.as_deref(), &file_name) else {
        return Err(call.refuse(ApiError::invalid_param(
            "file",
            "the recording's type could not be read from its content type or its name",
        )));
    };
    let temperature = match fields.get("temperature").and_then(Value::as_str) {
        None => None,
        Some(t) => Some(
            t.parse::<f32>()
                .ok()
                .filter(|t| (0.0..=1.0).contains(t))
                .ok_or_else(|| {
                    call.refuse(ApiError::invalid_param(
                        "temperature",
                        "temperature is from 0 to 1",
                    ))
                })?,
        ),
    };
    let language = fields
        .get("language")
        .and_then(Value::as_str)
        .map(str::to_string);
    let prompt = fields
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::to_string);

    let gateway = call.state.live.current();
    let plan = call.plan(&gateway, Kind::Transcription)?;
    let routed = gateway
        .media(&plan, wants_on_device(&headers), |built, model| {
            let transcriber = built.transcriber.clone()?;
            let mut request = TranscriptionRequest::new(
                model,
                audio.clone(),
                file_name.clone(),
                media_type.clone(),
            );
            request.language = language.clone();
            request.prompt = prompt.clone();
            request.temperature = temperature;
            Some(async move { transcriber.transcribe(request).await })
        })
        .await
        .map_err(|e| call.failed(e))?;

    let reply = &routed.value;
    // How long the recording was, for a model sold by the second, when the vendor said.
    let mut units = Units::none();
    if let Some(seconds) = reply.seconds.filter(|s| s.is_finite() && *s >= 0.0) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let millis = (seconds * 1000.0).round() as u64;
        units = units.with_audio_millis(millis);
    }
    let cost = call.answered(&gateway, &routed, &reply.model, &reply.usage, &units);
    if text_reply {
        let mut response = reply.text.clone().into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        return Ok(with_headers(response, &call.id, &routed, &cost));
    }
    let mut out = json!({ "text": reply.text, "llmr_cost": cost.view() });
    if let Some(usage) = usage_view(&reply.usage) {
        out["usage"] = usage;
    } else if let Some(seconds) = reply.seconds {
        out["usage"] = json!({ "type": "duration", "seconds": seconds });
    }
    Ok(with_headers(
        Json(out).into_response(),
        &call.id,
        &routed,
        &cost,
    ))
}
