//! Any endpoint speaking the OpenAI chat completions shape.
//!
//! # A shape, not a vendor
//!
//! OpenAI, Groq, Together, Fireworks, vLLM, Ollama, LM Studio, OpenRouter and LiteLLM all
//! answer at `/v1/chat/completions` with the same envelope. The base URL is a constructor
//! argument because that is the only thing that differs between them. A provider per vendor
//! would be the same translation copied eight times, drifting apart from the second copy.
//!
//! # The reach is given, never guessed
//!
//! A model on your laptop and a hosted API answer the same request shape and are completely
//! different places for your data to go. This provider cannot tell them apart, so it does
//! not try. You say which it is, and everything downstream can trust the answer.
//!
//! # Usage, and the one adjustment this provider makes
//!
//! OpenAI reports `prompt_tokens` as the **whole** prompt, cached part included, and
//! reports the cached count separately. [`crate::Usage::input_tokens`] means the part that
//! was not cached. So this provider subtracts.
//!
//! Without that, the same conversation through two providers would report different input
//! numbers, and a cost report comparing them would be comparing two different things.

use crate::chat::stream::Event;
use crate::chat::{
    ChatRequest, ChatResponse, ContentBlock, ImageSource, Message, Role, StopReason,
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

/// The chat completions protocol.
///
/// Holds nothing. Every method is a pure function over a request or a body.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatCompletions {
    /// What to call this endpoint in a record.
    ///
    /// Given rather than fixed, because this protocol is spoken by a dozen vendors and two
    /// providers reporting usage are only comparable if you can tell which is which.
    id: &'static str,
}

/// Anything speaking the OpenAI chat completions shape.
pub type OpenAiCompatible = ApiProvider<ChatCompletions>;

/// A provider at a base URL.
///
/// The id goes into every record beside the calls it made. The base URL should include the
/// version prefix the endpoint expects, usually `/v1`.
///
/// The reach is given, never guessed. A model on your laptop and a hosted API answer this
/// same shape, and the difference between them is where your data goes.
pub fn at(
    id: &'static str,
    base_url: impl Into<String>,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
    reach: Reach,
    registry: Arc<Registry>,
) -> OpenAiCompatible {
    ApiProvider::new(
        ChatCompletions { id },
        base_url,
        transport,
        key,
        reach,
        registry,
    )
}

/// OpenAI's own API, reading `OPENAI_API_KEY` from the environment.
///
/// # Errors
///
/// [`Error::Auth`] when the variable is unset or blank, and [`Error::Transient`] when the
/// HTTP client cannot be built.
#[cfg(feature = "reqwest")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
pub fn from_env(timeout: std::time::Duration) -> Result<OpenAiCompatible> {
    Ok(at(
        "openai",
        "https://api.openai.com/v1",
        Arc::new(crate::transport::Reqwest::new(timeout)?),
        Secret::from_env("openai-api-key", "OPENAI_API_KEY")?,
        Reach::FirstPartyApi,
        // No shipped table. Writing rows nobody verified would invent exactly the
        // provenance the registry exists to record. Ask the endpoint, or supply one.
        Arc::new(Registry::empty("openai", Reach::FirstPartyApi)),
    ))
}

impl Protocol for ChatCompletions {
    fn id(&self) -> &str {
        self.id
    }

    fn chat_url(&self, base_url: &str, _model: &ModelId) -> String {
        format!("{base_url}/chat/completions")
    }

    fn catalogue_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{base_url}/models"))
    }

    fn headers(&self, key: &Secret) -> Result<Vec<(String, String)>> {
        let key = key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;
        Ok(vec![
            ("authorization".into(), format!("Bearer {key}")),
            ("content-type".into(), "application/json".into()),
        ])
    }

    fn body(&self, request: &ChatRequest) -> Result<Value> {
        let mut messages = Vec::new();

        if let Some(system) = &request.system {
            messages.push(json!({ "role": "system", "content": system }));
        }
        for message in &request.messages {
            messages.extend(wire_message(message));
        }

        let mut body = json!({
            "model": request.model.as_str(),
            "messages": messages,
        });

        if let Some(max) = request.generation.max_tokens {
            body["max_completion_tokens"] = json!(max);
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
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                }))
                .collect::<Vec<_>>());
        }
        if let Some(schema) = &request.response_schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": { "name": "response", "schema": schema, "strict": true },
            });
        }

        Ok(body)
    }

    fn read(&self, body: &Value, asked_for: &ModelId) -> Result<ChatResponse> {
        let choice = body
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| Error::Unreadable("the reply carried no choices".into()))?;

        let message = choice
            .get("message")
            .ok_or_else(|| Error::Unreadable("the choice carried no message".into()))?;

        let mut blocks = Vec::new();
        if let Some(text) = message.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                blocks.push(ContentBlock::Text(text.to_string()));
            }
        }
        for call in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let function = call.get("function");
            let arguments = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .unwrap_or("{}");
            blocks.push(ContentBlock::ToolUse {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: function
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                // Arguments arrive as a JSON string and a model can produce one that does
                // not parse. The call is kept with the raw text rather than dropped, so a
                // caller can see what was attempted.
                input: serde_json::from_str(arguments)
                    .unwrap_or_else(|_| json!({ "raw": arguments })),
            });
        }

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
            read_stop(choice.get("finish_reason").and_then(Value::as_str)),
            read_usage(body.get("usage")),
            body.get("model")
                .and_then(Value::as_str)
                .map(ModelId::from)
                .unwrap_or_else(|| asked_for.clone()),
        );

        // The provider's own word, including one this crate does not know.
        // `StopReason::Other` says code cannot act on it; this says what it was.
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            response = response.with_stop_details(reason);
        }
        Ok(response)
    }

    fn stream_body(&self, request: &ChatRequest) -> Result<Option<Value>> {
        let mut body = self.body(request)?;
        body["stream"] = json!(true);
        // Asked for explicitly, because this shape reports no usage in a streamed call
        // unless you do. A stream that forgets reports nothing, and nothing turns into zero
        // in whatever adds it up — the one failure this crate exists to prevent.
        body["stream_options"] = json!({ "include_usage": true });
        Ok(Some(body))
    }

    fn read_event(&self, frame: &SseFrame, asked_for: &ModelId) -> Result<Vec<Event>> {
        // This shape names no frames and terminates with a sentinel that is not JSON.
        if frame.data.trim() == "[DONE]" {
            return Ok(Vec::new());
        }
        let Some(body) = frame.json() else {
            return Err(Error::Unreadable(format!(
                "a frame that was neither JSON nor [DONE]: {}",
                frame.data.chars().take(120).collect::<String>()
            )));
        };

        let mut events = Vec::new();

        // Every frame repeats the model. Emitted once; a second `Started` would only
        // overwrite the first with the same value, but sending it on every text delta is
        // noise in anything that logs events.
        if let Some(model) = body.get("model").and_then(Value::as_str) {
            if body
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|c| {
                    c.first()
                        .and_then(|choice| choice.get("delta"))
                        .and_then(|delta| delta.get("role"))
                        .is_some()
                })
            {
                events.push(Event::Started {
                    model: ModelId::from(model),
                });
            }
        }

        if let Some(choice) = body
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        {
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        events.push(Event::TextDelta(text.to_string()));
                    }
                }
                // Reasoning, where a backend behind this shape produces it. There is no
                // signature in this shape, so there is none to lose.
                for field in ["reasoning_content", "reasoning"] {
                    if let Some(text) = delta.get(field).and_then(Value::as_str) {
                        if !text.is_empty() {
                            events.push(Event::ThinkingDelta(text.to_string()));
                        }
                    }
                }
                for call in delta
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    let function = call.get("function");
                    // A name means a new call. Later frames for the same call carry only
                    // more arguments.
                    if let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str)
                    {
                        events.push(Event::ToolUseStarted {
                            id: call
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: name.to_string(),
                        });
                    }
                    if let Some(arguments) = function
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                    {
                        if !arguments.is_empty() {
                            events.push(Event::ToolArgumentsDelta(arguments.to_string()));
                        }
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason") {
                if !reason.is_null() {
                    events.push(Event::Stopped {
                        reason: read_stop(reason.as_str()),
                        details: None,
                    });
                }
            }
        }

        // Arrives in its own final frame, after the one carrying `finish_reason`. Read
        // through the same function the whole call uses, so the cached part is subtracted
        // the same way and a streamed and a non streamed call report the same numbers.
        let usage = read_usage(body.get("usage"));
        if usage.coverage() != crate::cost::UsageCoverage::Absent {
            events.push(Event::Metered(usage));
        }

        let _ = asked_for;
        Ok(events)
    }

    fn read_catalogue(&self, body: &Value) -> Result<Vec<ModelId>> {
        let mut ids: Vec<ModelId> = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unreadable("the model list had no data array".into()))?
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(ModelId::from)
            .collect();
        ids.sort();
        Ok(ids)
    }
}

/// One of our messages as one or more of theirs.
///
/// Tool results are their own top level message in this protocol, where Anthropic carries
/// them as blocks inside a user turn. One of ours can therefore become several of theirs.
fn wire_message(message: &Message) -> Vec<Value> {
    let mut out = Vec::new();
    let mut text = Vec::new();
    let mut calls = Vec::new();
    let mut images = Vec::new();

    for block in &message.content {
        match block {
            ContentBlock::Text(t) => text.push(t.clone()),
            // Reasoning is not sent back. This protocol has no place to put it, and there
            // is no signature to preserve, so dropping it changes nothing the model checks.
            ContentBlock::Thinking { .. } => {}
            // Nor is an opaque block. It came from a different protocol, and this one has
            // nowhere to put it. Dropped rather than guessed at, which is safe here for the
            // same reason: nothing in this protocol checks the history against a signature.
            ContentBlock::Opaque { .. } => {}
            // This shape takes an image as a URL, and a data URL is how bytes get into
            // one. The media type is the caller's rather than sniffed here, so what the
            // provider is told is what they said it was.
            ContentBlock::Image { media_type, source } => images.push(json!({
                "type": "image_url",
                "image_url": {
                    "url": match source {
                        ImageSource::Url(url) => url.clone(),
                        ImageSource::Bytes(bytes) => {
                            use base64::Engine as _;
                            format!(
                                "data:{media_type};base64,{}",
                                base64::engine::general_purpose::STANDARD.encode(bytes)
                            )
                        }
                    },
                },
            })),
            ContentBlock::ToolUse { id, name, input } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                }
            })),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => out.push(json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                // The protocol has no error flag on a tool result. Saying so in the content
                // is worse than silence would be: a model told a tool failed can work
                // around it, and one left to infer it from an empty string cannot.
                "content": if *is_error { format!("error: {content}") } else { content.clone() },
            })),
        }
    }

    if !text.is_empty() || !calls.is_empty() || !images.is_empty() {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        // A turn with an image has to carry its content as parts. A turn without one keeps
        // the plain string, because that is what every endpoint speaking this shape accepts
        // and some of the smaller ones accept nothing else.
        let content = if images.is_empty() {
            json!(text.join("\n"))
        } else {
            let mut parts: Vec<Value> = text
                .iter()
                .map(|t| json!({ "type": "text", "text": t }))
                .collect();
            parts.extend(images);
            Value::Array(parts)
        };
        let mut turn = json!({ "role": role, "content": content });
        if !calls.is_empty() {
            turn["tool_calls"] = Value::Array(calls);
        }
        out.push(turn);
    }

    out
}

fn read_stop(reason: Option<&str>) -> StopReason {
    match reason {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => StopReason::Refusal,
        Some("context_length_exceeded") => StopReason::ContextWindowExceeded,
        _ => StopReason::Other,
    }
}

fn read_usage(value: Option<&Value>) -> Usage {
    let Some(usage) = value else {
        return Usage::absent();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);

    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64);

    Usage {
        // The adjustment this whole provider is documented for. `prompt_tokens` here is the
        // whole prompt including the cached part, and this crate's input_tokens is the part
        // that was not cached. Saturating, because a provider reporting more cached tokens
        // than prompt tokens is wrong and should not underflow into a huge number.
        input_tokens: field("prompt_tokens").map(|total| total.saturating_sub(cached.unwrap_or(0))),
        cache_read_tokens: cached,
        // This protocol has no cache write count. Absent rather than zero: nobody said it
        // was zero, and a zero here would price a cache write as free.
        cache_write_tokens: None,
        output_tokens: field("completion_tokens"),
        // Reported by the vendor, not counted here.
        estimated: false,
    }
}
