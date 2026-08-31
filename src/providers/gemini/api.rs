//! Gemini's `generateContent` API.
//!
//! A [`Protocol`] and nothing else. The transport, the credential, the status codes and the
//! error mapping are [`ApiProvider`]'s.
//!
//! # What this shape does that neither of the others does
//!
//! Its parts, its roles and its usage fields all differ, and each one is a chance to lose
//! something quietly.
//!
//! **The assistant is called `model`.** A turn sent with the role `assistant` is rejected.
//!
//! **Reasoning arrives as a part marked `thought`, not as its own block type.** Read as
//! ordinary text it would land in the answer, which is the exact failure
//! [`crate::ContentBlock::reasoning`] exists to prevent — the model's private working out on
//! somebody's screen.
//!
//! **There is no signature on a thinking part.** Nothing to preserve, so unlike Anthropic
//! there is nothing here that breaks a conversation one turn later.
//!
//! # Usage
//!
//! `promptTokenCount` is the whole prompt including anything served from cache, and
//! `cachedContentTokenCount` is that cached part. [`crate::Usage::input_tokens`] means the
//! part that was **not** cached, so this subtracts — the same adjustment the OpenAI shape
//! makes and for the same reason: two providers reporting different input counts for the
//! same conversation cannot be compared.
//!
//! `thoughtsTokenCount` is reported apart from `candidatesTokenCount`. Thinking tokens are
//! billed, so [`crate::Usage::output_tokens`] is the two added together rather than the
//! visible half.

use crate::chat::stream::Event;
use crate::chat::{
    ChatRequest, ChatResponse, ContentBlock, Effort, ImageSource, Message, Role, StopReason,
    Thinking,
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

/// Google's own endpoint.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// The `generateContent` protocol.
///
/// Holds nothing. Every method is a pure function over a request or a body.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenerateContent;

/// Gemini, ready to call.
pub type Gemini = ApiProvider<GenerateContent>;

/// A provider at Google's endpoint, with a key and a table you supply.
///
/// There is no shipped model table or price book. Writing rows nobody here verified would
/// invent exactly the provenance [`Registry`] exists to record, and a row without a source
/// and a date is refused at parse time anyway. Ask the endpoint with
/// [`crate::Provider::catalogue`], or supply a table you checked.
pub fn with(transport: Arc<dyn HttpTransport>, key: Secret, registry: Arc<Registry>) -> Gemini {
    ApiProvider::new(
        GenerateContent,
        DEFAULT_BASE_URL,
        transport,
        key,
        Reach::FirstPartyApi,
        registry,
    )
}

/// A provider reading `GEMINI_API_KEY` from the environment.
///
/// # Errors
///
/// [`Error::Auth`] when the variable is unset or blank, and [`Error::Transient`] when the
/// HTTP client cannot be built.
#[cfg(feature = "reqwest")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
pub fn from_env(timeout: std::time::Duration) -> Result<Gemini> {
    Ok(with(
        Arc::new(crate::transport::Reqwest::new(timeout)?),
        Secret::from_env("gemini-api-key", "GEMINI_API_KEY")?,
        Arc::new(Registry::empty("gemini", Reach::FirstPartyApi)),
    ))
}

impl Protocol for GenerateContent {
    fn id(&self) -> &str {
        "gemini"
    }

    /// The model goes in the path here, not in the body.
    ///
    /// This is the reason [`Protocol::chat_url`] is given the model at all. Anthropic and
    /// the OpenAI shape name it in the body and ignore the argument; this API has no way to
    /// be addressed without it.
    fn chat_url(&self, base_url: &str, model: &ModelId) -> String {
        format!("{base_url}/models/{}:generateContent", model.as_str())
    }

    fn catalogue_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{base_url}/models"))
    }

    fn headers(&self, key: &Secret) -> Result<Vec<(String, String)>> {
        let key = key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;
        Ok(vec![
            ("x-goog-api-key".into(), key.to_string()),
            ("content-type".into(), "application/json".into()),
        ])
    }

    fn body(&self, request: &ChatRequest) -> Result<Value> {
        let mut contents = Vec::with_capacity(request.messages.len());
        for message in &request.messages {
            contents.push(json!({
                // Not "assistant". A turn sent with that role is rejected outright.
                "role": match message.role {
                    Role::User => "user",
                    Role::Assistant => "model",
                },
                "parts": message.content.iter().filter_map(wire_part).collect::<Vec<_>>(),
            }));
        }

        let mut body = json!({ "contents": contents });

        if let Some(system) = &request.system {
            body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
        }

        let mut generation = json!({});
        if let Some(max) = request.generation.max_tokens {
            generation["maxOutputTokens"] = json!(max);
        }
        if let Some(temperature) = request.generation.temperature {
            generation["temperature"] = json!(temperature);
        }
        if let Some(top_p) = request.generation.top_p {
            generation["topP"] = json!(top_p);
        }
        if let Some(schema) = &request.response_schema {
            generation["responseMimeType"] = json!("application/json");
            generation["responseSchema"] = schema.clone();
        }
        match request.thinking {
            Thinking::On(effort) => {
                generation["thinkingConfig"] = json!({
                    "thinkingBudget": budget(effort),
                    // Asked for explicitly. Without it the reasoning is billed and not
                    // returned, which is the worst of both: you pay for it and cannot see
                    // what the model was doing when it went wrong.
                    "includeThoughts": true,
                });
            }
            // Nought is how this API is told not to reason. Distinct from saying nothing,
            // which leaves the model's own default in place.
            Thinking::Off => {
                generation["thinkingConfig"] = json!({ "thinkingBudget": 0 });
            }
            Thinking::Unset => {}
        }
        if generation != json!({}) {
            body["generationConfig"] = generation;
        }

        if !request.tools.is_empty() {
            body["tools"] = json!([{
                "functionDeclarations": request
                    .tools
                    .iter()
                    .map(|tool| json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }))
                    .collect::<Vec<_>>(),
            }]);
        }

        Ok(body)
    }

    fn read(&self, body: &Value, asked_for: &ModelId) -> Result<ChatResponse> {
        let candidate = body
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| Error::Unreadable("the reply carried no candidates".into()))?;

        let blocks: Vec<ContentBlock> = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(read_part).collect())
            .unwrap_or_default();

        if blocks.is_empty() {
            return Err(Error::Unreadable(
                "the reply carried no content this crate could read".into(),
            ));
        }

        Ok(ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: blocks,
            },
            read_stop(candidate.get("finishReason").and_then(Value::as_str)),
            read_usage(body.get("usageMetadata")),
            body.get("modelVersion")
                .and_then(Value::as_str)
                .map(ModelId::from)
                .unwrap_or_else(|| asked_for.clone()),
        ))
    }

    fn read_catalogue(&self, body: &Value) -> Result<Vec<ModelId>> {
        let mut ids: Vec<ModelId> = body
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unreadable("the model list had no models array".into()))?
            .iter()
            .filter_map(|m| m.get("name").and_then(Value::as_str))
            // Listed as `models/gemini-...`; a caller names the model, not the path.
            .map(|name| ModelId::from(name.strip_prefix("models/").unwrap_or(name)))
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// A different method on a different path, and the frames only arrive as server sent
    /// events if you ask for them that way.
    ///
    /// Without `alt=sse` this endpoint answers with a JSON array of chunks instead, which
    /// the shared frame reader cannot read — and the failure would be an empty reply rather
    /// than an error.
    fn stream_url(&self, base_url: &str, model: &ModelId) -> String {
        format!(
            "{base_url}/models/{}:streamGenerateContent?alt=sse",
            model.as_str()
        )
    }

    fn stream_body(&self, request: &ChatRequest) -> Result<Option<Value>> {
        Ok(Some(self.body(request)?))
    }

    fn read_event(&self, frame: &SseFrame, asked_for: &ModelId) -> Result<Vec<Event>> {
        let Some(body) = frame.json() else {
            return Err(Error::Unreadable(format!(
                "a frame that was not JSON: {}",
                frame.data.chars().take(120).collect::<String>()
            )));
        };

        let mut events = Vec::new();
        if let Some(model) = body.get("modelVersion").and_then(Value::as_str) {
            events.push(Event::Started {
                model: ModelId::from(model),
            });
        }

        if let Some(candidate) = body
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            for part in candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                // Same split as `read_part`, and for the same reason: reasoning read as
                // ordinary text ends up on a screen as though the model had said it.
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if part
                        .get("thought")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        events.push(Event::ThinkingDelta(text.to_string()));
                    } else if !text.is_empty() {
                        events.push(Event::TextDelta(text.to_string()));
                    }
                } else if let Some(call) = part.get("functionCall") {
                    events.push(Event::ToolUseStarted {
                        id: call
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    });
                    // Whole in one frame here, rather than accumulated across several.
                    events.push(Event::ToolArgumentsDelta(
                        call.get("args").cloned().unwrap_or(json!({})).to_string(),
                    ));
                }
            }

            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                events.push(Event::Stopped {
                    reason: read_stop(Some(reason)),
                    details: None,
                });
            }
        }

        // Repeated on every frame here, each one a running total rather than a delta. Every
        // field is reported, so merging identical totals is idempotent and the last one
        // wins with the same numbers it had before.
        let usage = read_usage(body.get("usageMetadata"));
        if usage.coverage() != crate::cost::UsageCoverage::Absent {
            events.push(Event::Metered(usage));
        }

        let _ = asked_for;
        Ok(events)
    }
}

/// The API takes a token budget and this crate takes a named level.
fn budget(effort: Effort) -> u32 {
    match effort {
        Effort::Low => 2_048,
        Effort::Medium => 8_192,
        Effort::High => 24_576,
        Effort::XHigh => 32_768,
        // The documented ceiling for this family. Asking for more is rejected, and a
        // request refused for a number this crate invented is worse than one capped here.
        Effort::Max => 32_768,
    }
}

fn wire_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text(text) => Some(json!({ "text": text })),
        // Sent back marked as what it is. This API has no signature to preserve, so a
        // dropped thinking part costs context rather than breaking the next turn.
        ContentBlock::Thinking { text, .. } => Some(json!({ "text": text, "thought": true })),
        ContentBlock::ToolUse { name, input, .. } => Some(json!({
            "functionCall": { "name": name, "args": input },
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Some(json!({
            "functionResponse": {
                // This API keys a result by the tool's name rather than by a call id, and
                // the id is what this crate carries. Sent as the name it is: a result
                // labelled with an id this API does not recognise is dropped at the far end.
                "name": tool_use_id,
                "response": if *is_error {
                    json!({ "error": content })
                } else {
                    json!({ "result": content })
                },
            },
        })),
        ContentBlock::Image { media_type, source } => Some(match source {
            ImageSource::Bytes(bytes) => {
                use base64::Engine as _;
                json!({
                    "inlineData": {
                        "mimeType": media_type,
                        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                    },
                })
            }
            ImageSource::Url(url) => json!({
                "fileData": { "mimeType": media_type, "fileUri": url },
            }),
        }),
        // It came from a different protocol and this one has nowhere to put it. Dropped
        // rather than guessed at, which is safe here because nothing in this API checks the
        // history against a signature.
        ContentBlock::Opaque { .. } => None,
    }
}

fn read_part(part: &Value) -> Option<ContentBlock> {
    if let Some(call) = part.get("functionCall") {
        return Some(ContentBlock::ToolUse {
            // This API does not always give a call an id. Falling back to the name keeps a
            // result routable, which is what the id is for.
            id: call
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| call.get("name").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string(),
            name: call.get("name")?.as_str()?.to_string(),
            input: call.get("args").cloned().unwrap_or(json!({})),
        });
    }

    let text = part.get("text")?.as_str()?.to_string();
    // A part marked `thought` is the model's working out. Read as ordinary text it lands in
    // the answer, and somebody sees on a screen what the model was only thinking.
    if part
        .get("thought")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some(ContentBlock::Thinking {
            text,
            signature: None,
        });
    }
    Some(ContentBlock::Text(text))
}

fn read_stop(reason: Option<&str>) -> StopReason {
    match reason {
        Some("STOP") => StopReason::EndTurn,
        Some("MAX_TOKENS") => StopReason::MaxTokens,
        // Both are the model declining, for different reasons it does not always separate.
        Some("SAFETY") | Some("PROHIBITED_CONTENT") | Some("BLOCKLIST") | Some("RECITATION") => {
            StopReason::Refusal
        }
        Some("MALFORMED_FUNCTION_CALL") => StopReason::Other,
        // Kept as unknown rather than mapped to the nearest one. Guessing would report a
        // complete answer for a truncated reply.
        _ => StopReason::Other,
    }
}

fn read_usage(value: Option<&Value>) -> Usage {
    let Some(usage) = value else {
        return Usage::absent();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);

    let cached = field("cachedContentTokenCount");
    let visible = field("candidatesTokenCount");
    let thoughts = field("thoughtsTokenCount");

    Usage {
        // The whole prompt less the cached part, so this means the same thing it means
        // everywhere else in this crate.
        input_tokens: field("promptTokenCount")
            .map(|total| total.saturating_sub(cached.unwrap_or(0))),
        cache_read_tokens: cached,
        // This API reports no cache write count. Absent rather than zero: nobody said it was
        // nought, and a zero here would price a cache write as free.
        cache_write_tokens: None,
        // Thinking tokens are billed whether or not they are shown, so they belong in the
        // number that gets priced. Added rather than reported apart, because a caller
        // pricing `output_tokens` would otherwise underpay for every reasoning call.
        output_tokens: match (visible, thoughts) {
            (None, None) => None,
            (visible, thoughts) => Some(visible.unwrap_or(0) + thoughts.unwrap_or(0)),
        },
        // Reported by the vendor, not counted here.
        estimated: false,
    }
}
