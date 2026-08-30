//! Anthropic's Messages API.
//!
//! A [`Protocol`] and nothing else. The transport, the credential, the status codes and the
//! error mapping are [`ApiProvider`]'s, which is why this file is only the translation.
//!
//! # What it carries that a common subset would not
//!
//! Reasoning blocks keep their signature, so a conversation with thinking in it can be
//! continued. Cache breakpoints land where you asked for them. Blocks this crate does not
//! model come back verbatim and go out again unchanged. All three are the difference between
//! a conversation that works on turn four and one that does not.
//!
//! # Usage
//!
//! Anthropic reports `input_tokens` as the part of the prompt that was **not** served from
//! cache, and the cached parts separately. That is exactly what [`crate::Usage`] means, so
//! nothing is adjusted here. The OpenAI shape does adjust, and says so.

use crate::chat::stream::Event;
use crate::chat::{
    ChatRequest, ChatResponse, ContentBlock, Effort, Message, Role, StopReason, Thinking,
};
use crate::cost::Usage;
use crate::error::{Error, Result};
use crate::model::{ModelId, Reach};
use crate::providers::api::{ApiProvider, Protocol, SseFrame};
use crate::registry::Registry;
use crate::secret::Secret;
use crate::transport::HttpTransport;
use serde_json::{json, Value};
use std::sync::Arc;

/// The API version this protocol sends.
const API_VERSION: &str = "2023-06-01";

/// Anthropic's own hosted API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The Messages protocol.
///
/// Holds nothing. Every method is a pure function over a request or a body, which is what
/// lets one instance serve any number of concurrent calls without a thought.
#[derive(Debug, Clone, Copy, Default)]
pub struct Messages;

/// Anthropic, ready to call.
pub type Anthropic = ApiProvider<Messages>;

/// A provider reading `ANTHROPIC_API_KEY` from the environment.
///
/// # Errors
///
/// [`Error::Auth`] when the variable is unset or blank, and [`Error::Transient`] when the
/// HTTP client cannot be built.
#[cfg(feature = "reqwest")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
pub fn from_env(timeout: std::time::Duration) -> Result<Anthropic> {
    Ok(ApiProvider::new(
        Messages,
        DEFAULT_BASE_URL,
        Arc::new(crate::transport::Reqwest::new(timeout)?),
        Secret::from_env("anthropic-api-key", "ANTHROPIC_API_KEY")?,
        Reach::FirstPartyApi,
        Arc::new(shipped_registry()),
    ))
}

/// A provider over a transport you supply.
///
/// What the tests use, and what to reach for when you need your own client settings, a
/// gateway that speaks this protocol, or a recorded transport.
pub fn with(transport: Arc<dyn HttpTransport>, key: Secret, registry: Arc<Registry>) -> Anthropic {
    ApiProvider::new(
        Messages,
        DEFAULT_BASE_URL,
        transport,
        key,
        Reach::FirstPartyApi,
        registry,
    )
}

impl Protocol for Messages {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn chat_url(&self, base_url: &str) -> String {
        format!("{base_url}/v1/messages")
    }

    fn headers(&self, key: &Secret) -> Result<Vec<(String, String)>> {
        let key = key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;
        Ok(vec![
            ("x-api-key".into(), key.to_string()),
            ("anthropic-version".into(), API_VERSION.into()),
            ("content-type".into(), "application/json".into()),
        ])
    }

    fn body(&self, request: &ChatRequest) -> Result<Value> {
        // The API requires a limit. Sending none is a request it refuses, so this is the one
        // place a default beats an error: 4096 is small enough not to surprise a bill and
        // large enough for most replies.
        let max_tokens = request.generation.max_tokens.unwrap_or(4_096);

        let mut body = json!({
            "model": request.model.as_str(),
            "max_tokens": max_tokens,
            "messages": messages(request)?,
        });

        if let Some(system) = &request.system {
            body["system"] = json!(system);
        }
        if let Some(temperature) = request.generation.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(top_p) = request.generation.top_p {
            body["top_p"] = json!(top_p);
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request
                .tools
                .iter()
                .map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                }))
                .collect::<Vec<_>>());
        }

        // `Unset` sends nothing. On a model that reasons by default that leaves it on, which
        // is what no opinion means; sending "off" here would be a decision the caller did
        // not make.
        if let Thinking::On(effort) = request.thinking {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget(effort).min(max_tokens.saturating_sub(1)),
            });
        }

        Ok(body)
    }

    fn read(&self, body: &Value, asked_for: &ModelId) -> Result<ChatResponse> {
        let blocks: Vec<ContentBlock> = body
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| blocks.iter().filter_map(read_block).collect())
            .unwrap_or_default();

        // A reply with nothing readable is a failure, not an empty answer. Returning an
        // empty message lets a caller carry on with nothing and call it a success.
        if blocks.is_empty() {
            return Err(Error::Unreadable(
                "the reply carried no content this crate could read".into(),
            ));
        }

        let mut response = ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: blocks,
            },
            read_stop(body.get("stop_reason").and_then(Value::as_str)),
            read_usage(body.get("usage")),
            body.get("model")
                .and_then(Value::as_str)
                .map(ModelId::from)
                .unwrap_or_else(|| asked_for.clone()),
        );

        if let Some(sequence) = body.get("stop_sequence").and_then(Value::as_str) {
            response = response.with_stop_details(sequence);
        }
        Ok(response)
    }

    fn stream_body(&self, request: &ChatRequest) -> Result<Option<Value>> {
        let mut body = self.body(request)?;
        body["stream"] = json!(true);
        Ok(Some(body))
    }

    fn read_event(&self, frame: &SseFrame, _asked_for: &ModelId) -> Result<Vec<Event>> {
        // The frame type is on the `event:` line here, and repeated inside the JSON. The
        // line is what the format says is authoritative, so that is what this reads.
        let Some(body) = frame.json() else {
            // Anthropic sends no non JSON frames. One that arrives is a frame this code
            // cannot read, and dropping it silently loses whatever it carried.
            return Err(Error::Unreadable(format!(
                "a frame that was not JSON: {}",
                frame.data.chars().take(120).collect::<String>()
            )));
        };

        Ok(match frame.event.as_str() {
            // Carries the model and the prompt half of the usage. The output half comes in
            // `message_delta` at the end, and `Transcript` merges the two.
            "message_start" => {
                let message = body.get("message");
                let mut events = Vec::new();
                if let Some(model) = message.and_then(|m| m.get("model")).and_then(Value::as_str) {
                    events.push(Event::Started {
                        model: ModelId::from(model),
                    });
                }
                let usage = read_usage(message.and_then(|m| m.get("usage")));
                if usage.coverage() != crate::cost::UsageCoverage::Absent {
                    events.push(Event::Metered(usage));
                }
                events
            }

            "content_block_start" => match body.get("content_block") {
                Some(block) => match block.get("type").and_then(Value::as_str) {
                    // Text and thinking open empty and are filled by deltas. Nothing to
                    // emit: an empty delta would add a block the model has not spoken into.
                    Some("text") | Some("thinking") => Vec::new(),
                    Some("tool_use") => vec![Event::ToolUseStarted {
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }],
                    // Redacted reasoning and anything added after this was written. Kept
                    // verbatim, exactly as the non streamed reader keeps it: a block
                    // dropped here is a continuation the provider rejects.
                    Some(kind) => vec![Event::Opaque {
                        kind: kind.to_string(),
                        raw: block.clone(),
                    }],
                    None => Vec::new(),
                },
                None => Vec::new(),
            },

            "content_block_delta" => match body.get("delta") {
                Some(delta) => match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => text_of(delta, "text").map(Event::TextDelta),
                    Some("thinking_delta") => text_of(delta, "thinking").map(Event::ThinkingDelta),
                    // The proof for the thinking block that just ended. Losing this is not
                    // visible now; it is visible one turn later, when the provider rejects
                    // the history it is missing from.
                    Some("signature_delta") => {
                        text_of(delta, "signature").map(Event::ThinkingSignature)
                    }
                    Some("input_json_delta") => {
                        text_of(delta, "partial_json").map(Event::ToolArgumentsDelta)
                    }
                    _ => None,
                }
                .into_iter()
                .collect(),
                None => Vec::new(),
            },

            // The stop reason, and the output half of the usage.
            "message_delta" => {
                let mut events = Vec::new();
                let delta = body.get("delta");
                if let Some(reason) = delta.and_then(|d| d.get("stop_reason")) {
                    // A null stop_reason means "not yet", which is not a stop.
                    if !reason.is_null() {
                        events.push(Event::Stopped {
                            reason: read_stop(reason.as_str()),
                            details: delta
                                .and_then(|d| d.get("stop_sequence"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        });
                    }
                }
                let usage = read_usage(body.get("usage"));
                if usage.coverage() != crate::cost::UsageCoverage::Absent {
                    events.push(Event::Metered(usage));
                }
                events
            }

            // An error mid stream. The provider is telling us the rest is not coming, and
            // saying so is the difference between a truncated answer and a known failure.
            "error" => {
                return Err(Error::Transient(format!(
                    "the stream stopped: {}",
                    body.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("no reason given")
                )))
            }

            // `content_block_stop`, `message_stop`, `ping`. Nothing a caller needs.
            _ => Vec::new(),
        })
    }
}

/// One string field of a delta, when it is there and is a string.
fn text_of(delta: &Value, field: &str) -> Option<String> {
    delta.get(field).and_then(Value::as_str).map(str::to_string)
}

/// The API takes a token budget and this crate takes a named level.
///
/// The mapping is here rather than at the call site, where one number would be tuned against
/// one model and quietly wrong on the next.
fn budget(effort: Effort) -> u32 {
    match effort {
        Effort::Low => 2_048,
        Effort::Medium => 8_192,
        Effort::High => 24_576,
        Effort::XHigh => 49_152,
        Effort::Max => 98_304,
    }
}

/// The conversation in the shape the API takes, with cache breakpoints placed.
fn messages(request: &ChatRequest) -> Result<Value> {
    let mut out = Vec::with_capacity(request.messages.len());
    for (index, message) in request.messages.iter().enumerate() {
        let mut blocks: Vec<Value> = message.content.iter().map(wire_block).collect();

        // A breakpoint marks the last block of the message it follows. Everything before it
        // is cached, so putting it anywhere else changes what you are billed for.
        if request.cache_breakpoints.contains(&index) {
            if let Some(last) = blocks.last_mut() {
                last["cache_control"] = json!({ "type": "ephemeral" });
            }
        }

        out.push(json!({
            "role": match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            },
            "content": blocks,
        }));
    }
    Ok(Value::Array(out))
}

fn wire_block(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text(text) => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { text, signature } => {
            // The signature goes back untouched. A thinking block replayed without it is
            // rejected on arrival, and dropping the block changes the turn the model sees.
            let mut out = json!({ "type": "thinking", "thinking": text });
            if let Some(signature) = signature {
                out["signature"] = json!(signature);
            }
            out
        }
        ContentBlock::ToolUse { id, name, input } => {
            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        // Byte for byte. The provider checks the history against what it produced, so a
        // block reshaped here is a block it will not recognise.
        ContentBlock::Opaque { raw, .. } => raw.clone(),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
    }
}

fn read_block(value: &Value) -> Option<ContentBlock> {
    match value.get("type")?.as_str()? {
        "text" => Some(ContentBlock::Text(value.get("text")?.as_str()?.to_string())),
        "thinking" => Some(ContentBlock::Thinking {
            // Absent when the provider elides the reasoning. Not an error, and the block
            // still matters because the signature is on it.
            text: value
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            signature: value
                .get("signature")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "tool_use" => Some(ContentBlock::ToolUse {
            id: value.get("id")?.as_str()?.to_string(),
            name: value.get("name")?.as_str()?.to_string(),
            input: value.get("input").cloned().unwrap_or(json!({})),
        }),
        // Kept rather than dropped. A redacted reasoning blob replayed without this block is
        // a continuation the provider rejects, one turn after the mistake.
        kind => Some(ContentBlock::Opaque {
            kind: kind.to_string(),
            raw: value.clone(),
        }),
    }
}

fn read_stop(reason: Option<&str>) -> StopReason {
    match reason {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("stop_sequence") => StopReason::StopSequence,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("refusal") => StopReason::Refusal,
        Some("pause_turn") => StopReason::PauseTurn,
        Some("model_context_window_exceeded") => StopReason::ContextWindowExceeded,
        // A reason this crate has not seen, kept as unknown rather than mapped to the
        // nearest one. Guessing would report a complete answer for a truncated reply.
        _ => StopReason::Other,
    }
}

fn read_usage(value: Option<&Value>) -> Usage {
    let Some(usage) = value else {
        return Usage::absent();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    let mut out = Usage::absent();
    // Already the uncached remainder here, which is what this crate means. Nothing is
    // subtracted, unlike the OpenAI shape.
    if let Some(n) = field("input_tokens") {
        out = out.with_input(n);
    }
    if let Some(n) = field("cache_read_input_tokens") {
        out = out.with_cache_read(n);
    }
    if let Some(n) = field("cache_creation_input_tokens") {
        out = out.with_cache_write(n);
    }
    if let Some(n) = field("output_tokens") {
        out = out.with_output(n);
    }
    out
}

/// The models this release knows about.
///
/// A starting point, not a source of truth. Every row carries when a person last checked it.
/// Supply your own [`Registry`] when you need one that is current.
pub fn shipped_registry() -> Registry {
    Registry::parse(include_str!("../../../models/anthropic.toml"))
        .unwrap_or_else(|_| Registry::empty("anthropic", Reach::FirstPartyApi))
}

/// What this release believes Anthropic charges.
///
/// Dated, for the same reason. Read [`crate::PriceBook::verified_at`] before trusting a
/// total. Where a provider reports cost directly, that wins over anything here.
pub fn shipped_prices() -> crate::cost::PriceBook {
    crate::cost::PriceBook::parse(include_str!("../../../models/anthropic-prices.toml"))
        .unwrap_or_else(|_| crate::cost::PriceBook {
            id: "unavailable".into(),
            provider: "anthropic".into(),
            effective_from: String::new(),
            source: String::new(),
            verified_at: String::new(),
            currency: "USD".into(),
            rates: Default::default(),
        })
}
