//! Anthropic's Messages API.
//!
//! # What this provider carries that a common subset would not
//!
//! Reasoning blocks keep their signature, so a conversation with thinking in it can be
//! continued. Cache breakpoints are placed where you asked for them. Both of those are the
//! difference between a cheap conversation and an expensive one, and both disappear if a
//! request is flattened into text and a token count.
//!
//! # Usage
//!
//! Anthropic reports `input_tokens` as the part of the prompt that was **not** served from
//! cache, and reports the cached parts separately. That matches [`crate::Usage`] exactly,
//! so nothing is adjusted here. The OpenAI provider does adjust, and the difference is
//! documented there.

use crate::error::{Error, Result};
use crate::http::{HttpRequest, HttpTransport};
use crate::message::{ContentBlock, Message, Role, StopReason};
use crate::model::{ModelCapabilities, ModelId};
use crate::provider::Provider;
use crate::registry::Registry;
use crate::request::{ChatRequest, Effort, Thinking};
use crate::response::ChatResponse;
use crate::secret::Secret;
use crate::usage::Usage;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// The API version this provider sends.
const API_VERSION: &str = "2023-06-01";

/// Anthropic's own hosted API.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic's Messages API.
///
/// Everything here is immutable once built. There is no lock and no interior mutability, so
/// one instance serves any number of concurrent calls with nothing to contend on.
///
/// ```no_run
/// use modelreach::providers::anthropic::Anthropic;
/// use modelreach::{ChatRequest, Message, Provider, Secret};
/// use std::time::Duration;
///
/// # async fn example() -> modelreach::Result<()> {
/// let provider = Anthropic::from_env(Duration::from_secs(60))?;
/// let reply = provider
///     .chat(ChatRequest::new("claude-sonnet-5", vec![Message::user("Hello")])
///         .with_max_tokens(64))
///     .await?;
/// println!("{}", reply.text());
/// # Ok(())
/// # }
/// ```
pub struct Anthropic {
    transport: Arc<dyn HttpTransport>,
    key: Secret,
    base_url: String,
    registry: Arc<Registry>,
}

impl Anthropic {
    /// A provider reading `ANTHROPIC_API_KEY` from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] when the variable is unset or blank, and
    /// [`Error::Transient`] when the HTTP client cannot be built.
    #[cfg(feature = "http")]
    #[cfg_attr(docsrs, doc(cfg(feature = "http")))]
    pub fn from_env(timeout: std::time::Duration) -> Result<Self> {
        Ok(Self::new(
            Arc::new(crate::http::Reqwest::new(timeout)?),
            Secret::from_env("anthropic-api-key", "ANTHROPIC_API_KEY")?,
            Arc::new(shipped_registry()),
        ))
    }

    /// A provider over a transport you supply.
    ///
    /// This is the constructor the tests use, and the one to reach for when you need your
    /// own client settings or a recorded transport.
    pub fn new(transport: Arc<dyn HttpTransport>, key: Secret, registry: Arc<Registry>) -> Self {
        Self {
            transport,
            key,
            base_url: DEFAULT_BASE_URL.to_string(),
            registry,
        }
    }

    /// Points this provider at a different base URL.
    ///
    /// For a gateway or a proxy that speaks the same protocol. A trailing slash is removed
    /// so that both spellings behave the same.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    fn body(&self, request: &ChatRequest) -> Result<Value> {
        // Anthropic requires a limit. Sending none is a request the API refuses, so this is
        // the one place a default is better than an error: 4096 is small enough not to
        // surprise anyone's bill and large enough for most replies.
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
        if let Thinking::On(effort) = request.thinking {
            // The API takes a token budget and this crate takes a named level. The mapping
            // is here so that one number does not end up copied into every caller, where
            // it would be tuned against one model and wrong on the next.
            let budget = match effort {
                Effort::Low => 2_048,
                Effort::Medium => 8_192,
                Effort::High => 24_576,
            };
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget.min(max_tokens.saturating_sub(1)) });
        }

        Ok(body)
    }
}

/// Turns the conversation into the shape the API takes, placing cache breakpoints.
fn messages(request: &ChatRequest) -> Result<Value> {
    let mut out = Vec::with_capacity(request.messages.len());
    for (index, message) in request.messages.iter().enumerate() {
        let mut blocks = Vec::with_capacity(message.content.len());
        for block in &message.content {
            blocks.push(wire_block(block)?);
        }

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

fn wire_block(block: &ContentBlock) -> Result<Value> {
    Ok(match block {
        ContentBlock::Text(text) => json!({ "type": "text", "text": text }),
        ContentBlock::Thinking { text, signature } => {
            // The signature travels back untouched. A thinking block replayed without it is
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
    })
}

fn read_block(value: &Value) -> Option<ContentBlock> {
    match value.get("type")?.as_str()? {
        "text" => Some(ContentBlock::Text(value.get("text")?.as_str()?.to_string())),
        "thinking" => Some(ContentBlock::Thinking {
            // The text can be absent when the provider elides the reasoning. That is not an
            // error and the block still matters, because the signature is on it.
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
        _ => None,
    }
}

fn read_stop(reason: Option<&str>) -> StopReason {
    match reason {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("stop_sequence") => StopReason::StopSequence,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("refusal") => StopReason::Refusal,
        // A reason this crate has not seen is kept as unknown rather than mapped to the
        // nearest one. Guessing here would report a complete answer for a truncated reply.
        _ => StopReason::Other,
    }
}

fn read_usage(value: Option<&Value>) -> Usage {
    let Some(usage) = value else {
        return Usage::absent();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    Usage {
        // Anthropic already reports the uncached remainder here, which is what this crate
        // means by input tokens. Nothing is subtracted.
        input_tokens: field("input_tokens"),
        cache_read_tokens: field("cache_read_input_tokens"),
        cache_write_tokens: field("cache_creation_input_tokens"),
        output_tokens: field("output_tokens"),
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        self.registry.capabilities(model)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = serde_json::to_vec(&self.body(&request)?)
            .map_err(|e| Error::InvalidRequest(format!("building the request: {e}")))?;

        let key = self
            .key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;

        let response = self
            .transport
            .send(HttpRequest {
                url: format!("{}/v1/messages", self.base_url),
                headers: vec![
                    ("x-api-key".into(), key.to_string()),
                    ("anthropic-version".into(), API_VERSION.into()),
                    ("content-type".into(), "application/json".into()),
                ],
                body,
            })
            .await?;

        response.check()?;

        let parsed: Value = serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the reply was not JSON: {e}")))?;

        let blocks: Vec<ContentBlock> = parsed
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| blocks.iter().filter_map(read_block).collect())
            .unwrap_or_default();

        // A reply with no readable content is a failure, not an empty answer. Returning an
        // empty message here would let a run carry on with nothing and call it a success.
        if blocks.is_empty() {
            return Err(Error::Unreadable(
                "the reply carried no content this crate could read".into(),
            ));
        }

        Ok(ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: blocks,
            },
            stop_reason: read_stop(parsed.get("stop_reason").and_then(Value::as_str)),
            usage: read_usage(parsed.get("usage")),
            model: parsed
                .get("model")
                .and_then(Value::as_str)
                .map(ModelId::from)
                .unwrap_or_else(|| request.model.clone()),
        })
    }
}

/// The models this release knows about.
///
/// A starting point, not a source of truth. Vendors add and retire models on their own
/// schedule, and every row here carries the date somebody last checked it. Supply your own
/// [`Registry`] when you need one that is current.
pub fn shipped_registry() -> Registry {
    Registry::parse(include_str!("../../models/anthropic.toml"))
        .unwrap_or_else(|_| Registry::empty("anthropic", crate::Reach::FirstPartyApi))
}
