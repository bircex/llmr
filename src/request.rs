//! What you ask a model for.

use crate::message::Message;
use crate::model::ModelId;
use serde::{Deserialize, Serialize};

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    /// The name the model uses to call it.
    pub name: String,
    /// What it does, in the model's terms. This is prompt text and it decides whether the
    /// tool gets called correctly.
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: serde_json::Value,
}

/// How hard to think.
///
/// A named level rather than a token count. Providers scale these differently, and a number
/// tuned against one model is quietly wrong on the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Effort {
    /// Barely any.
    Low,
    /// A working default.
    Medium,
    /// As much as the model will spend.
    High,
}

/// Whether the model should reason before answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Thinking {
    /// Do not reason.
    Off,
    /// Reason at this level.
    On(Effort),
    /// This model has no reasoning to ask for.
    ///
    /// Not the same as [`Thinking::Off`]. Off is a choice you made, and this is the absence
    /// of the option. Keeping them apart is what lets a table describing one vendor say
    /// something true about another.
    Unavailable,
}

/// Sampling settings.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Generation {
    /// How random the output is. Providers use different ranges, so leaving this unset and
    /// taking the provider's default is usually right.
    pub temperature: Option<f32>,
    /// Nucleus sampling cutoff.
    pub top_p: Option<f32>,
    /// The most tokens to produce.
    pub max_tokens: Option<u32>,
}

/// One request to one model.
///
/// Build it with [`ChatRequest::new`] and the `with_` methods. The struct is marked
/// non exhaustive, so fields can be added without breaking your code, and constructing it
/// with a literal will not compile from outside this crate.
///
/// ```
/// use modelreach::{ChatRequest, Message};
///
/// let request = ChatRequest::new("claude-sonnet-5", vec![Message::user("Hello")])
///     .with_system("You answer in one sentence.")
///     .with_max_tokens(256);
///
/// assert_eq!(request.model.as_str(), "claude-sonnet-5");
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChatRequest {
    /// Which model.
    pub model: ModelId,
    /// Instructions that are not part of the conversation.
    pub system: Option<String>,
    /// The conversation so far, oldest first.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolSchema>,
    /// Sampling settings.
    pub generation: Generation,
    /// Whether to reason before answering.
    pub thinking: Thinking,
    /// A JSON Schema the reply must match.
    ///
    /// Providers that cannot do this say so through
    /// [`crate::ModelCapabilities::structured_output`]. Check before you set it.
    pub response_schema: Option<serde_json::Value>,
    /// Where to place cache breakpoints, counted in messages from the start.
    ///
    /// A provider without prompt caching ignores this. It costs money to get wrong in
    /// either direction, so it is explicit rather than inferred.
    pub cache_breakpoints: Vec<usize>,
}

impl ChatRequest {
    /// A request with nothing set beyond the model and the conversation.
    pub fn new(model: impl Into<ModelId>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages,
            tools: Vec::new(),
            generation: Generation::default(),
            thinking: Thinking::Off,
            response_schema: None,
            cache_breakpoints: Vec::new(),
        }
    }

    /// Sets the system instructions.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Offers the model a set of tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = tools;
        self
    }

    /// Caps the reply length.
    #[must_use]
    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.generation.max_tokens = Some(max);
        self
    }

    /// Sets the sampling temperature.
    #[must_use]
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.generation.temperature = Some(temperature);
        self
    }

    /// Asks the model to reason before answering.
    #[must_use]
    pub fn with_thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = thinking;
        self
    }

    /// Requires the reply to match a schema.
    #[must_use]
    pub fn with_response_schema(mut self, schema: serde_json::Value) -> Self {
        self.response_schema = Some(schema);
        self
    }

    /// Places a cache breakpoint after the message at this index.
    #[must_use]
    pub fn with_cache_breakpoint(mut self, after_message: usize) -> Self {
        self.cache_breakpoints.push(after_message);
        self
    }

    /// What this request needs a model to support.
    ///
    /// Compare it against [`crate::Provider::capabilities`] to find out what will be
    /// dropped before you send anything.
    pub fn needs(&self) -> Needs {
        Needs {
            tools: !self.tools.is_empty(),
            structured_output: self.response_schema.is_some(),
            prompt_caching: !self.cache_breakpoints.is_empty(),
            thinking: matches!(self.thinking, Thinking::On(_)),
        }
    }
}

/// What a request needs, as a set of yes or no answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Needs {
    /// The request offers tools.
    pub tools: bool,
    /// The request asks for output matching a schema.
    pub structured_output: bool,
    /// The request places cache breakpoints.
    pub prompt_caching: bool,
    /// The request asks the model to reason.
    pub thinking: bool,
}

impl Needs {
    /// What this request needs and the model cannot do, by name.
    ///
    /// An empty list means the request will be sent as written. A non empty one lists what
    /// the provider will silently drop, which is worth knowing before you are billed for a
    /// reply that ignored half your request.
    pub fn unmet_by(self, have: &crate::model::ModelCapabilities) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.tools && !have.tools {
            missing.push("tools");
        }
        if self.structured_output && !have.structured_output {
            missing.push("structured_output");
        }
        if self.prompt_caching && !have.prompt_caching {
            missing.push("prompt_caching");
        }
        if self.thinking && !have.thinking {
            missing.push("thinking");
        }
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelCapabilities, Reach};

    #[test]
    fn a_plain_request_needs_nothing_special() {
        let request = ChatRequest::new("m", vec![Message::user("hi")]);
        assert_eq!(request.needs(), Needs::default());
    }

    #[test]
    fn what_a_provider_will_drop_is_listed_before_the_call() {
        let request = ChatRequest::new("m", vec![Message::user("hi")])
            .with_tools(vec![ToolSchema {
                name: "search".into(),
                description: "look it up".into(),
                parameters: serde_json::json!({}),
            }])
            .with_thinking(Thinking::On(Effort::High));

        let plain = ModelCapabilities::none(Reach::LocalCli);
        assert_eq!(request.needs().unmet_by(&plain), vec!["tools", "thinking"]);
    }

    #[test]
    fn a_capable_model_leaves_nothing_unmet() {
        let request = ChatRequest::new("m", vec![Message::user("hi")])
            .with_response_schema(serde_json::json!({ "type": "object" }));
        let capable = ModelCapabilities {
            structured_output: true,
            ..ModelCapabilities::none(Reach::FirstPartyApi)
        };
        assert!(request.needs().unmet_by(&capable).is_empty());
    }

    #[test]
    fn thinking_off_is_not_thinking_unavailable() {
        // Off is a choice. Unavailable is the absence of the option, and a table that
        // could not say so would describe one vendor and mislead about another.
        assert_ne!(Thinking::Off, Thinking::Unavailable);
    }
}
