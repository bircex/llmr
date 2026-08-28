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

use crate::error::{Error, Result};
use crate::http::{HttpRequest, HttpTransport};
use crate::message::{ContentBlock, Message, Role, StopReason};
use crate::model::{ModelCapabilities, ModelId, Reach};
use crate::provider::Provider;
use crate::registry::Registry;
use crate::request::ChatRequest;
use crate::response::ChatResponse;
use crate::secret::Secret;
use crate::usage::Usage;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// A provider speaking the OpenAI chat completions protocol.
///
/// Immutable once built. No lock, no interior mutability, safe to share across tasks.
///
/// ```no_run
/// use llmr::providers::openai::OpenAiCompatible;
/// use llmr::{ChatRequest, Message, Provider, Reach, Registry, Secret};
/// use std::sync::Arc;
/// use std::time::Duration;
///
/// # async fn example() -> llmr::Result<()> {
/// // A model running on this machine. Nothing leaves the hardware, and saying so is
/// // what lets a privacy rule downstream act on it.
/// let ollama = OpenAiCompatible::at(
///     "ollama",
///     "http://localhost:11434/v1",
///     Arc::new(llmr::http::Reqwest::new(Duration::from_secs(120))?),
///     Secret::new("ollama", "not-needed"),
///     Reach::SelfHosted,
///     Arc::new(Registry::empty("ollama", Reach::SelfHosted)),
/// );
///
/// let reply = ollama
///     .chat(ChatRequest::new("llama3", vec![Message::user("Hello")]))
///     .await?;
/// println!("{}", reply.text());
/// # Ok(())
/// # }
/// ```
pub struct OpenAiCompatible {
    id: String,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
    base_url: String,
    reach: Reach,
    registry: Arc<Registry>,
}

impl OpenAiCompatible {
    /// A provider at a base URL.
    ///
    /// The id is yours to choose and goes into every record beside the calls it made. Two
    /// providers reporting usage are only comparable if you can tell which is which.
    ///
    /// The base URL should include the version prefix the endpoint expects, usually
    /// `/v1`. A trailing slash is removed so both spellings behave the same.
    pub fn at(
        id: impl Into<String>,
        base_url: impl Into<String>,
        transport: Arc<dyn HttpTransport>,
        key: Secret,
        reach: Reach,
        registry: Arc<Registry>,
    ) -> Self {
        Self {
            id: id.into(),
            transport,
            key,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            reach,
            registry,
        }
    }

    /// OpenAI's own API, reading `OPENAI_API_KEY` from the environment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Auth`] when the variable is unset or blank, and
    /// [`Error::Transient`] when the HTTP client cannot be built.
    #[cfg(feature = "http")]
    #[cfg_attr(docsrs, doc(cfg(feature = "http")))]
    pub fn from_env(timeout: std::time::Duration) -> Result<Self> {
        Ok(Self::at(
            "openai",
            "https://api.openai.com/v1",
            Arc::new(crate::http::Reqwest::new(timeout)?),
            Secret::from_env("openai-api-key", "OPENAI_API_KEY")?,
            Reach::FirstPartyApi,
            // No shipped table. Writing rows nobody verified would be inventing exactly the
            // provenance the registry exists to record. Ask the endpoint, or supply one.
            Arc::new(Registry::empty("openai", Reach::FirstPartyApi)),
        ))
    }

    fn body(&self, request: &ChatRequest) -> Value {
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

        body
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

    for block in &message.content {
        match block {
            ContentBlock::Text(t) => text.push(t.clone()),
            // Reasoning is not sent back. This protocol has no place to put it, and there
            // is no signature to preserve, so dropping it changes nothing the model checks.
            ContentBlock::Thinking { .. } => {}
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

    if !text.is_empty() || !calls.is_empty() {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut turn = json!({ "role": role, "content": text.join("\n") });
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
    }
}

#[async_trait]
impl Provider for OpenAiCompatible {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        self.registry.capabilities(model)
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let body = serde_json::to_vec(&self.body(&request))
            .map_err(|e| Error::InvalidRequest(format!("building the request: {e}")))?;

        let key = self
            .key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;

        let response = self
            .transport
            .send(HttpRequest {
                url: format!("{}/chat/completions", self.base_url),
                headers: vec![
                    ("authorization".into(), format!("Bearer {key}")),
                    ("content-type".into(), "application/json".into()),
                ],
                body,
            })
            .await?;

        response.check()?;

        let parsed: Value = serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the reply was not JSON: {e}")))?;

        let choice = parsed
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
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
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
                name: name.to_string(),
                // Arguments arrive as a JSON string. A model can produce one that does not
                // parse, and the call is kept with the raw text rather than dropped, so the
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

        Ok(ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: blocks,
            },
            stop_reason: read_stop(choice.get("finish_reason").and_then(Value::as_str)),
            usage: read_usage(parsed.get("usage")),
            model: parsed
                .get("model")
                .and_then(Value::as_str)
                .map(ModelId::from)
                .unwrap_or_else(|| request.model.clone()),
        })
    }

    async fn catalogue(&self) -> Result<Vec<ModelId>> {
        let key = self
            .key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;

        let response = self
            .transport
            .send(HttpRequest {
                url: format!("{}/models", self.base_url),
                headers: vec![("authorization".into(), format!("Bearer {key}"))],
                body: Vec::new(),
            })
            .await?;

        response.check()?;

        let parsed: Value = serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the model list was not JSON: {e}")))?;

        let mut ids: Vec<ModelId> = parsed
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

/// The reach this provider was told it has.
impl OpenAiCompatible {
    /// Where this endpoint's data goes.
    pub fn reach(&self) -> Reach {
        self.reach
    }
}
