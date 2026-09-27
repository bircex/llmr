//! The OpenAI chat completions shape, read from a client and written back to one.
//!
//! The inverse of `llmr::providers::openai::api`: that module writes this shape to a vendor
//! and reads the vendor's reply, this one reads what a client sent and writes the reply the
//! client expects. Every OpenAI SDK, LangChain, LlamaIndex and most editors can then reach
//! any configured provider by changing a base URL.
//!
//! # What is refused rather than dropped
//!
//! A field the router cannot honour is a 400 naming it, never silently ignored. A client
//! that asked for `n = 3` and got one answer, or for stop sequences that were never sent,
//! has been billed for a reply that ignored half the request. The crate this is built on
//! refuses the same way, and a gateway that did less would undo it.
//!
//! Fields that only tune how a reply is produced and cannot change what a client is owed
//! (`user`, `seed`, `metadata`, `parallel_tool_calls`, `store`) are accepted and not sent.

use crate::error::ApiError;
use llmr::chat::{ContentBlock, ImageSource, Message, Role, StopReason, ToolSchema};
use llmr::{ChatRequest, ChatResponse, Effort, Event, Thinking, Usage};
use serde_json::{json, Map, Value};

/// A request, read.
#[derive(Debug)]
pub struct Incoming {
    /// The request, with the model as the client named it.
    pub request: ChatRequest,
    /// Whether the client wants server sent events.
    pub stream: bool,
    /// Whether a streamed reply should end with a usage chunk.
    pub include_usage: bool,
}

/// Reads a chat completions request.
///
/// # Errors
///
/// A 400 naming the field, for anything malformed or anything the router cannot honour.
pub fn read_request(body: &Value) -> Result<Incoming, ApiError> {
    let object = body
        .as_object()
        .ok_or_else(|| ApiError::invalid("the body must be a JSON object"))?;

    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ApiError::invalid_param("model", "model is required"))?;

    refuse_what_cannot_be_honoured(object)?;

    let (system, messages) = read_messages(
        object
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| ApiError::invalid_param("messages", "messages must be an array"))?,
    )?;
    if messages.is_empty() {
        return Err(ApiError::invalid_param(
            "messages",
            "at least one non system message is required",
        ));
    }

    let mut request = ChatRequest::new(model, messages);
    request.system = system;

    // `max_completion_tokens` is the current name and `max_tokens` the old one. Both arrive
    // from real clients; the newer wins when both are set, as it does at OpenAI.
    let max = object
        .get("max_completion_tokens")
        .filter(|v| !v.is_null())
        .or_else(|| object.get("max_tokens").filter(|v| !v.is_null()));
    if let Some(max) = max {
        let max = max
            .as_u64()
            .and_then(|m| u32::try_from(m).ok())
            .filter(|m| *m > 0)
            .ok_or_else(|| {
                ApiError::invalid_param("max_tokens", "max_tokens must be a positive integer")
            })?;
        request.generation.max_tokens = Some(max);
    }
    request.generation.temperature = read_float(object, "temperature")?;
    request.generation.top_p = read_float(object, "top_p")?;

    if let Some(tools) = object.get("tools").filter(|v| !v.is_null()) {
        request.tools = read_tools(tools)?;
    }
    if let Some(format) = object.get("response_format").filter(|v| !v.is_null()) {
        request.response_schema = read_response_format(format)?;
    }
    if let Some(effort) = object.get("reasoning_effort").filter(|v| !v.is_null()) {
        request.thinking = read_effort(effort)?;
    }

    let stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_usage = object
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(Incoming {
        request,
        stream,
        include_usage,
    })
}

/// The fields a request may carry that this gateway cannot carry through.
fn refuse_what_cannot_be_honoured(object: &Map<String, Value>) -> Result<(), ApiError> {
    if let Some(n) = object.get("n").filter(|v| !v.is_null()) {
        if n.as_u64() != Some(1) {
            return Err(ApiError::invalid_param(
                "n",
                "only n = 1 is supported: one request is routed to one model",
            ));
        }
    }
    if object.get("stop").is_some_and(|v| !v.is_null()) {
        return Err(ApiError::invalid_param(
            "stop",
            "stop sequences are not supported by this gateway yet, and a request sent \
             without them would not stop where you asked",
        ));
    }
    for field in ["presence_penalty", "frequency_penalty"] {
        if let Some(value) = object.get(field).filter(|v| !v.is_null()) {
            if value.as_f64() != Some(0.0) {
                return Err(ApiError::invalid_param(
                    field,
                    format!("{field} is not supported by this gateway"),
                ));
            }
        }
    }
    if object.get("logprobs").and_then(Value::as_bool) == Some(true) {
        return Err(ApiError::invalid_param(
            "logprobs",
            "logprobs are not supported by this gateway",
        ));
    }
    if object.get("logit_bias").is_some_and(|v| !v.is_null()) {
        return Err(ApiError::invalid_param(
            "logit_bias",
            "logit_bias is not supported by this gateway",
        ));
    }
    if let Some(choice) = object.get("tool_choice").filter(|v| !v.is_null()) {
        if choice.as_str() != Some("auto") {
            return Err(ApiError::invalid_param(
                "tool_choice",
                "only tool_choice = \"auto\" is supported: forcing or forbidding a tool \
                 cannot be carried to every provider yet",
            ));
        }
    }
    for field in [
        "audio",
        "modalities",
        "prediction",
        "web_search_options",
        "functions",
    ] {
        if object.get(field).is_some_and(|v| !v.is_null()) {
            return Err(ApiError::invalid_param(
                field,
                format!("{field} is not supported by this gateway"),
            ));
        }
    }
    Ok(())
}

fn read_float(object: &Map<String, Value>, field: &str) -> Result<Option<f32>, ApiError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            // Sampling settings are small numbers; the narrowing is the precision the
            // providers themselves accept.
            .map(|v| Some(v as f32))
            .ok_or_else(|| ApiError::invalid_param(field, format!("{field} must be a number"))),
    }
}

/// The conversation, and the system text pulled out of it.
///
/// Tool results are their own messages in this shape and blocks inside a user turn in the
/// crate's, so consecutive tool messages become one user turn. A user message straight after
/// them joins that turn too, which is the only arrangement Anthropic accepts.
fn read_messages(raw: &[Value]) -> Result<(Option<String>, Vec<Message>), ApiError> {
    let mut system: Vec<String> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();
    // Whether the last message pushed is a user turn built from tool results.
    let mut last_holds_results = false;

    for (index, message) in raw.iter().enumerate() {
        let at = |what: &str| format!("messages[{index}]: {what}");
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid_param("messages", at("role is required")))?;

        match role {
            "system" | "developer" => {
                system.push(text_of(message.get("content"), &at)?);
            }
            "user" => {
                let content = user_content(message.get("content"), &at)?;
                match messages.last_mut() {
                    Some(previous) if last_holds_results => previous.content.extend(content),
                    _ => messages.push(Message {
                        role: Role::User,
                        content,
                    }),
                }
                last_holds_results = false;
            }
            "assistant" => {
                let mut content = Vec::new();
                match message.get("content") {
                    None | Some(Value::Null) => {}
                    other => {
                        let text = text_of(other, &at)?;
                        if !text.is_empty() {
                            content.push(ContentBlock::Text(text));
                        }
                    }
                }
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    content.push(read_tool_call(call, &at)?);
                }
                if content.is_empty() {
                    return Err(ApiError::invalid_param(
                        "messages",
                        at("an assistant message needs content or tool_calls"),
                    ));
                }
                messages.push(Message {
                    role: Role::Assistant,
                    content,
                });
                last_holds_results = false;
            }
            "tool" => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ApiError::invalid_param("messages", at("tool_call_id is required"))
                    })?;
                let block = ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content: text_of(message.get("content"), &at)?,
                    is_error: false,
                };
                match messages.last_mut() {
                    Some(previous) if last_holds_results => previous.content.push(block),
                    _ => messages.push(Message {
                        role: Role::User,
                        content: vec![block],
                    }),
                }
                last_holds_results = true;
            }
            other => {
                return Err(ApiError::invalid_param(
                    "messages",
                    at(&format!("role {other:?} is not supported")),
                ))
            }
        }
    }

    let system = (!system.is_empty()).then(|| system.join("\n\n"));
    Ok((system, messages))
}

/// Text content, as a string or as an array of text parts.
fn text_of(content: Option<&Value>, at: &dyn Fn(&str) -> String) -> Result<String, ApiError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut texts = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => texts.push(
                        part.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    // An assistant turn the client is replaying. It carried no answer.
                    Some("refusal") => {}
                    other => {
                        return Err(ApiError::invalid_param(
                            "messages",
                            at(&format!(
                                "only text parts are accepted here, not {}",
                                other.unwrap_or("an untyped part")
                            )),
                        ))
                    }
                }
            }
            Ok(texts.join("\n"))
        }
        Some(_) => Err(ApiError::invalid_param(
            "messages",
            at("content must be a string or an array of parts"),
        )),
    }
}

/// A user turn, which is the one place an image, a document or a recording may appear.
fn user_content(
    content: Option<&Value>,
    at: &dyn Fn(&str) -> String,
) -> Result<Vec<ContentBlock>, ApiError> {
    let Some(Value::Array(parts)) = content else {
        return Ok(vec![ContentBlock::Text(text_of(content, at)?)]);
    };
    let mut blocks = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => blocks.push(ContentBlock::Text(
                part.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )),
            Some("image_url") => {
                let url = part
                    .get("image_url")
                    .and_then(|i| i.get("url").or(Some(i)))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ApiError::invalid_param("messages", at("image_url needs a url"))
                    })?;
                blocks.push(read_image(url, at)?);
            }
            Some("file") => blocks.push(read_file(part.get("file"), at)?),
            Some("input_audio") => blocks.push(read_audio(part.get("input_audio"), at)?),
            other => {
                return Err(ApiError::invalid_param(
                    "messages",
                    at(&format!(
                        "content part {} is not supported",
                        other.unwrap_or("without a type")
                    )),
                ))
            }
        }
    }
    Ok(blocks)
}

/// The media type and bytes of a `data:` URL, when it is one.
///
/// `Ok(None)` for anything that is not a data URL, so the caller decides what else it takes.
fn data_url(
    url: &str,
    what: &str,
    at: &dyn Fn(&str) -> String,
) -> Result<Option<(String, Vec<u8>)>, ApiError> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Ok(None);
    };
    let (header, data) = rest
        .split_once(',')
        .ok_or_else(|| ApiError::invalid_param("messages", at("malformed data URL")))?;
    let media_type = header.strip_suffix(";base64").ok_or_else(|| {
        ApiError::invalid_param("messages", at(&format!("a data URL {what} must be base64")))
    })?;
    Ok(Some((media_type.to_string(), decode(data, what, at)?)))
}

fn decode(data: &str, what: &str, at: &dyn Fn(&str) -> String) -> Result<Vec<u8>, ApiError> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|_| ApiError::invalid_param("messages", at(&format!("{what} data is not base64"))))
}

/// A document from a `file` part: its bytes, inline.
///
/// `file_data` is a data URL, whose type is read from it; bare base64 is taken too, with the
/// type read from the file name's extension. A `file_id` names an upload to a files endpoint
/// this gateway does not have, so it is refused rather than forwarded to a provider that has
/// never seen it.
fn read_file(file: Option<&Value>, at: &dyn Fn(&str) -> String) -> Result<ContentBlock, ApiError> {
    let file =
        file.ok_or_else(|| ApiError::invalid_param("messages", at("a file part needs a file")))?;
    let name = file
        .get("filename")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(data) = file.get("file_data").and_then(Value::as_str) else {
        let why = if file.get("file_id").is_some() {
            "file_id names an upload this gateway does not hold; send the file as file_data"
        } else {
            "a file part needs file_data"
        };
        return Err(ApiError::invalid_param("messages", at(why)));
    };
    let (media_type, bytes) = match data_url(data, "file", at)? {
        Some(read) => read,
        None => {
            let by_name = name
                .as_deref()
                .map(str::to_ascii_lowercase)
                .and_then(|name| {
                    [(".pdf", "application/pdf"), (".txt", "text/plain")]
                        .iter()
                        .find(|(extension, _)| name.ends_with(extension))
                        .map(|(_, media_type)| (*media_type).to_string())
                })
                .ok_or_else(|| {
                    ApiError::invalid_param(
                        "messages",
                        at("send file_data as a data URL, or name the file .pdf or .txt so its                             type can be read"),
                    )
                })?;
            (by_name, decode(data, "file", at)?)
        }
    };
    Ok(ContentBlock::Document {
        media_type,
        source: ImageSource::Bytes(bytes),
        name,
    })
}

/// A recording from an `input_audio` part, which names a format rather than a media type.
fn read_audio(
    audio: Option<&Value>,
    at: &dyn Fn(&str) -> String,
) -> Result<ContentBlock, ApiError> {
    let audio = audio.ok_or_else(|| {
        ApiError::invalid_param("messages", at("an input_audio part needs input_audio"))
    })?;
    let data = audio
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::invalid_param("messages", at("input_audio needs data")))?;
    let format = audio
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let media_type = match format {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "webm" => "audio/webm",
        _ => {
            return Err(ApiError::invalid_param(
                "messages",
                at("input_audio format must be wav, mp3, flac, ogg, aac, m4a or webm"),
            ))
        }
    };
    Ok(ContentBlock::Audio {
        media_type: media_type.to_string(),
        data: decode(data, "audio", at)?,
    })
}

/// An image, from a data URL or a link.
///
/// The media type is read, never guessed: from the data URL itself, or from a link's file
/// extension. A link with no recognisable extension is refused, because a provider told the
/// wrong type rejects the request or decodes it wrongly.
fn read_image(url: &str, at: &dyn Fn(&str) -> String) -> Result<ContentBlock, ApiError> {
    if let Some((media_type, bytes)) = data_url(url, "image", at)? {
        return Ok(ContentBlock::Image {
            media_type,
            source: ImageSource::Bytes(bytes),
        });
    }

    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(ApiError::invalid_param(
            "messages",
            at("an image must be a data URL or an http(s) link"),
        ));
    }
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    let media_type = [
        (".png", "image/png"),
        (".jpg", "image/jpeg"),
        (".jpeg", "image/jpeg"),
        (".gif", "image/gif"),
        (".webp", "image/webp"),
    ]
    .iter()
    .find(|(extension, _)| path.ends_with(extension))
    .map(|(_, media_type)| *media_type)
    .ok_or_else(|| {
        ApiError::invalid_param(
            "messages",
            at(
                "the image link has no .png, .jpg, .gif or .webp extension to read its type \
                from; send it as a data URL instead",
            ),
        )
    })?;
    Ok(ContentBlock::Image {
        media_type: media_type.into(),
        source: ImageSource::Url(url.to_string()),
    })
}

fn read_tool_call(call: &Value, at: &dyn Fn(&str) -> String) -> Result<ContentBlock, ApiError> {
    let function = call
        .get("function")
        .ok_or_else(|| ApiError::invalid_param("messages", at("a tool call needs a function")))?;
    let arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    Ok(ContentBlock::ToolUse {
        id: call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid_param("messages", at("a tool call needs a name")))?
            .to_string(),
        // Kept with the raw text when it does not parse, the same fallback the providers
        // use, so a replayed call is never silently emptied.
        input: serde_json::from_str(arguments).unwrap_or_else(|_| json!({ "raw": arguments })),
    })
}

fn read_tools(tools: &Value) -> Result<Vec<ToolSchema>, ApiError> {
    let tools = tools
        .as_array()
        .ok_or_else(|| ApiError::invalid_param("tools", "tools must be an array"))?;
    tools
        .iter()
        .map(|tool| {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(ApiError::invalid_param(
                    "tools",
                    "only function tools are supported",
                ));
            }
            let function = tool
                .get("function")
                .ok_or_else(|| ApiError::invalid_param("tools", "a tool needs a function"))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::invalid_param("tools", "a tool needs a name"))?;
            Ok(ToolSchema::new(
                name,
                function
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            ))
        })
        .collect()
}

fn read_response_format(format: &Value) -> Result<Option<Value>, ApiError> {
    match format.get("type").and_then(Value::as_str) {
        Some("text") => Ok(None),
        Some("json_schema") => format
            .get("json_schema")
            .and_then(|s| s.get("schema"))
            .cloned()
            .map(Some)
            .ok_or_else(|| {
                ApiError::invalid_param("response_format", "json_schema needs a schema")
            }),
        Some("json_object") => Err(ApiError::invalid_param(
            "response_format",
            "json_object is not supported: send json_schema with the shape you want, which \
             every structured output provider can enforce",
        )),
        _ => Err(ApiError::invalid_param(
            "response_format",
            "response_format.type must be text or json_schema",
        )),
    }
}

fn read_effort(effort: &Value) -> Result<Thinking, ApiError> {
    Ok(match effort.as_str() {
        Some("none") => Thinking::Off,
        Some("minimal" | "low") => Thinking::On(Effort::Low),
        Some("medium") => Thinking::On(Effort::Medium),
        Some("high") => Thinking::On(Effort::High),
        Some("xhigh") => Thinking::On(Effort::XHigh),
        Some("max") => Thinking::On(Effort::Max),
        _ => {
            return Err(ApiError::invalid_param(
                "reasoning_effort",
                "reasoning_effort must be none, minimal, low, medium, high, xhigh or max",
            ))
        }
    })
}

/// How a stop reason is written in this shape.
///
/// The shape has five words and the crate has more reasons, so the crate's own name is
/// written beside it as `llmr_stop_reason`. A paused turn written as a bare `stop` would
/// read as finished.
pub fn finish_reason(reason: StopReason) -> &'static str {
    match reason {
        StopReason::ToolUse => "tool_calls",
        StopReason::MaxTokens | StopReason::ContextWindowExceeded => "length",
        StopReason::Refusal => "content_filter",
        _ => "stop",
    }
}

/// The crate's own name for a stop reason.
pub fn stop_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::StopSequence => "stop_sequence",
        StopReason::MaxTokens => "max_tokens",
        StopReason::Refusal => "refusal",
        StopReason::PauseTurn => "pause_turn",
        StopReason::ContextWindowExceeded => "context_window_exceeded",
        StopReason::Interrupted => "interrupted",
        _ => "other",
    }
}

/// Usage in this shape, or nothing when it cannot be written honestly.
///
/// `prompt_tokens` here is the whole prompt, cached part included, which is the crate's
/// [`Usage::prompt_tokens`]. When the provider reported no prompt or no output count at all
/// there is no usage object: a zero would make the call free in whatever adds it up.
pub fn usage(usage: &Usage) -> Option<Value> {
    let prompt = usage.prompt_tokens()?;
    let completion = usage.output_tokens?;
    let mut value = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": prompt + completion,
    });
    if let Some(cached) = usage.cache_read_tokens {
        value["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    Some(value)
}

/// A whole reply, as `chat.completion`.
pub fn write_response(id: &str, created: u64, reply: &ChatResponse) -> Value {
    let text = reply.text();
    let reasoning: String = reply
        .message
        .content
        .iter()
        .filter_map(ContentBlock::reasoning)
        .collect();
    let calls: Vec<Value> = reply
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                },
            })),
            _ => None,
        })
        .collect();

    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { json!(text) },
    });
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if reply.stop_reason == StopReason::Refusal {
        message["refusal"] = json!(reply
            .stop_details
            .clone()
            .unwrap_or_else(|| "the model declined".into()));
    }

    let mut body = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": reply.model.as_str(),
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason(reply.stop_reason),
            "llmr_stop_reason": stop_name(reply.stop_reason),
        }],
    });
    if let Some(usage) = usage(&reply.usage) {
        body["usage"] = usage;
    }
    body
}

/// A refusal, written the way this shape writes one: a successful reply with no content.
pub fn write_refusal(id: &str, created: u64, model: &str, category: Option<&str>) -> Value {
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": Value::Null,
                "refusal": category.unwrap_or("the model declined"),
            },
            "finish_reason": "content_filter",
            "llmr_stop_reason": "refusal",
        }],
    })
}

/// Turns events into `chat.completion.chunk` frames.
///
/// Holds the few things a chunk needs that no single event carries: which model, and which
/// index the tool call in progress has.
#[derive(Debug)]
pub struct ChunkWriter {
    id: String,
    created: u64,
    model: String,
    opened: bool,
    tool_calls: usize,
}

impl ChunkWriter {
    /// A writer for one streamed reply.
    pub fn new(id: String, created: u64, model: String) -> Self {
        Self {
            id,
            created,
            model,
            opened: false,
            tool_calls: 0,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<StopReason>) -> Value {
        let mut choice = json!({
            "index": 0,
            "delta": delta,
            "finish_reason": finish.map(finish_reason),
        });
        if let Some(reason) = finish {
            choice["llmr_stop_reason"] = json!(stop_name(reason));
        }
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [choice],
        })
    }

    /// The chunks one event becomes. Often none.
    pub fn write(&mut self, event: &Event) -> Vec<Value> {
        let mut out = Vec::new();
        if let Event::Started { model } = event {
            self.model = model.as_str().to_string();
        }
        // The first chunk names the role, as every client of this shape expects.
        if !self.opened {
            self.opened = true;
            out.push(self.chunk(json!({ "role": "assistant", "content": "" }), None));
        }
        match event {
            Event::TextDelta(text) if !text.is_empty() => {
                out.push(self.chunk(json!({ "content": text }), None));
            }
            Event::ThinkingDelta(text) if !text.is_empty() => {
                out.push(self.chunk(json!({ "reasoning_content": text }), None));
            }
            Event::ToolUseStarted { id, name } => {
                let index = self.tool_calls;
                self.tool_calls += 1;
                out.push(self.chunk(
                    json!({ "tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": "" },
                    }]}),
                    None,
                ));
            }
            Event::ToolArgumentsDelta(fragment) if !fragment.is_empty() => {
                let index = self.tool_calls.saturating_sub(1);
                out.push(self.chunk(
                    json!({ "tool_calls": [{
                        "index": index,
                        "function": { "arguments": fragment },
                    }]}),
                    None,
                ));
            }
            Event::Stopped { reason, .. } => {
                out.push(self.chunk(json!({}), Some(*reason)));
            }
            // Signatures and opaque blocks have no place in this shape. Usage is written
            // once, at the end, from everything that arrived.
            _ => {}
        }
        out
    }

    /// The closing usage chunk, when the client asked for one and there is usage to write.
    pub fn usage_chunk(&self, total: &Usage) -> Option<Value> {
        let usage = usage(total)?;
        Some(json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": usage,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(body: Value) -> Result<Incoming, ApiError> {
        read_request(&body)
    }

    #[test]
    fn a_plain_request_reads() {
        let incoming = read(json!({
            "model": "default",
            "messages": [
                { "role": "system", "content": "Be brief." },
                { "role": "user", "content": "Hello" },
            ],
            "max_tokens": 64,
            "temperature": 0.2,
            "stream": true,
            "stream_options": { "include_usage": true },
        }))
        .expect("reads");
        assert_eq!(incoming.request.model.as_str(), "default");
        assert_eq!(incoming.request.system.as_deref(), Some("Be brief."));
        assert_eq!(incoming.request.messages.len(), 1);
        assert_eq!(incoming.request.generation.max_tokens, Some(64));
        assert!(incoming.stream && incoming.include_usage);
    }

    #[test]
    fn the_newer_token_limit_wins() {
        let incoming = read(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "max_tokens": 10,
            "max_completion_tokens": 20,
        }))
        .expect("reads");
        assert_eq!(incoming.request.generation.max_tokens, Some(20));
    }

    #[test]
    fn tool_results_and_the_user_turn_after_them_become_one_user_turn() {
        // Anthropic accepts tool results only inside the user turn that follows the call,
        // and rejects two user turns in a row.
        let incoming = read(json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "weather in two cities" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "a", "type": "function", "function": { "name": "w", "arguments": "{\"c\":\"x\"}" } },
                    { "id": "b", "type": "function", "function": { "name": "w", "arguments": "{\"c\":\"y\"}" } },
                ]},
                { "role": "tool", "tool_call_id": "a", "content": "sunny" },
                { "role": "tool", "tool_call_id": "b", "content": "rain" },
                { "role": "user", "content": "and tomorrow?" },
            ],
        }))
        .expect("reads");
        let messages = &incoming.request.messages;
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].tool_calls().len(), 2);
        assert_eq!(messages[2].role, Role::User);
        assert_eq!(messages[2].content.len(), 3);
        match &messages[1].content[0] {
            ContentBlock::ToolUse { input, .. } => assert_eq!(input["c"], "x"),
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn an_image_keeps_the_type_it_was_sent_with() {
        let incoming = read(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "what is this" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" } },
            ]}],
        }))
        .expect("reads");
        assert!(incoming.request.needs().images);
        match &incoming.request.messages[0].content[1] {
            ContentBlock::Image {
                media_type,
                source: ImageSource::Bytes(bytes),
            } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(&bytes[..4], b"\x89PNG");
            }
            other => panic!("expected image bytes, got {other:?}"),
        }
    }

    #[test]
    fn an_image_link_without_a_type_is_refused_rather_than_guessed() {
        let error = read(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": [
                { "type": "image_url", "image_url": { "url": "https://example.com/picture" } },
            ]}],
        }))
        .expect_err("no extension");
        assert_eq!(error.status.as_u16(), 400);
    }

    #[test]
    fn a_pdf_and_a_recording_arrive_as_a_document_and_audio() {
        let incoming = read(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": [
                { "type": "file", "file": {
                    "filename": "report.pdf",
                    "file_data": "data:application/pdf;base64,JVBERi0xLjc=",
                } },
                { "type": "input_audio", "input_audio": { "data": "UklGRg==", "format": "wav" } },
            ]}],
        }))
        .expect("reads");
        let needs = incoming.request.needs();
        assert!(needs.documents && needs.audio);
        match &incoming.request.messages[0].content[..] {
            [ContentBlock::Document {
                media_type,
                source: ImageSource::Bytes(pdf),
                name,
            }, ContentBlock::Audio {
                media_type: audio_type,
                data,
            }] => {
                assert_eq!(media_type, "application/pdf");
                assert_eq!(pdf, b"%PDF-1.7");
                assert_eq!(name.as_deref(), Some("report.pdf"));
                assert_eq!(audio_type, "audio/wav");
                assert_eq!(data, b"RIFF");
            }
            other => panic!("expected a document and audio, got {other:?}"),
        }
    }

    #[test]
    fn bare_file_data_takes_its_type_from_the_name_and_a_file_id_is_refused() {
        let incoming = read(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": [
                { "type": "file", "file": { "filename": "notes.txt", "file_data": "aGk=" } },
            ]}],
        }))
        .expect("reads");
        assert!(matches!(
            &incoming.request.messages[0].content[0],
            ContentBlock::Document { media_type, .. } if media_type == "text/plain"
        ));

        for part in [
            json!({ "type": "file", "file": { "file_id": "file-123" } }),
            json!({ "type": "file", "file": { "filename": "x.bin", "file_data": "aGk=" } }),
            json!({ "type": "input_audio", "input_audio": { "data": "aGk=", "format": "midi" } }),
        ] {
            let error = read(json!({
                "model": "m",
                "messages": [{ "role": "user", "content": [part.clone()] }],
            }))
            .expect_err("refused");
            assert_eq!(error.status.as_u16(), 400, "{part}");
        }
    }

    #[test]
    fn what_cannot_be_honoured_is_refused_by_name() {
        for (field, value) in [
            ("n", json!(2)),
            ("stop", json!(["\n"])),
            ("tool_choice", json!("required")),
            ("response_format", json!({ "type": "json_object" })),
            ("presence_penalty", json!(0.5)),
            ("logprobs", json!(true)),
        ] {
            let mut body =
                json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] });
            body[field] = value;
            let error = read(body).expect_err(field);
            assert_eq!(error.param.as_deref(), Some(field), "{field}");
        }
    }

    #[test]
    fn harmless_defaults_that_clients_always_send_are_accepted() {
        read(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "n": 1,
            "stop": null,
            "presence_penalty": 0,
            "frequency_penalty": 0.0,
            "tool_choice": "auto",
            "user": "someone",
            "parallel_tool_calls": true,
        }))
        .expect("accepted");
    }

    #[test]
    fn reasoning_effort_maps_onto_the_crates_levels() {
        let incoming = read(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "reasoning_effort": "high",
        }))
        .expect("reads");
        assert_eq!(incoming.request.thinking, Thinking::On(Effort::High));
        assert!(incoming.request.needs().thinking);
    }

    #[test]
    fn usage_nobody_reported_is_left_out_rather_than_zero() {
        assert_eq!(usage(&Usage::absent()), None);
        let reported = usage(
            &Usage::absent()
                .with_input(10)
                .with_cache_read(90)
                .with_output(5),
        )
        .expect("complete");
        assert_eq!(reported["prompt_tokens"], 100);
        assert_eq!(reported["completion_tokens"], 5);
        assert_eq!(reported["total_tokens"], 105);
        assert_eq!(reported["prompt_tokens_details"]["cached_tokens"], 90);
    }

    #[test]
    fn a_reply_with_a_tool_call_writes_null_content_and_the_call() {
        let reply = ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "search".into(),
                    input: json!({ "q": "rust" }),
                }],
            },
            StopReason::ToolUse,
            Usage::absent(),
            "served".into(),
        );
        let body = write_response("id", 1, &reply);
        let choice = &body["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert!(choice["message"]["content"].is_null());
        assert_eq!(
            choice["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\"rust\"}"
        );
        assert!(body.get("usage").is_none());
        assert_eq!(body["model"], "served");
    }

    #[test]
    fn a_paused_turn_does_not_read_as_finished() {
        assert_eq!(finish_reason(StopReason::PauseTurn), "stop");
        assert_eq!(stop_name(StopReason::PauseTurn), "pause_turn");
    }

    #[test]
    fn a_stream_opens_with_the_role_and_numbers_its_tool_calls() {
        let mut writer = ChunkWriter::new("id".into(), 1, "asked".into());
        let first = writer.write(&Event::Started {
            model: "served".into(),
        });
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(first[0]["model"], "served");

        writer.write(&Event::ToolUseStarted {
            id: "a".into(),
            name: "x".into(),
        });
        let args = writer.write(&Event::ToolArgumentsDelta("{}".into()));
        assert_eq!(args[0]["choices"][0]["delta"]["tool_calls"][0]["index"], 0);
        let second = writer.write(&Event::ToolUseStarted {
            id: "b".into(),
            name: "y".into(),
        });
        assert_eq!(
            second[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
            1
        );

        let stop = writer.write(&Event::Stopped {
            reason: StopReason::ToolUse,
            details: None,
        });
        assert_eq!(stop[0]["choices"][0]["finish_reason"], "tool_calls");
    }
}
