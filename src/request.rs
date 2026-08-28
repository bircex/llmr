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
///
/// Five levels rather than three, because vendors expose five and a library with fewer
/// makes the top two unreachable. A provider whose model stops at three maps the last two
/// onto its own highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Effort {
    /// Barely any.
    Low,
    /// A working default.
    Medium,
    /// More than the default.
    High,
    /// Well beyond the default.
    XHigh,
    /// As much as the model will spend.
    Max,
}

/// Whether the model should reason before answering.
///
/// Three states rather than a bool, and the third is the one that matters. On some models
/// reasoning is on by default and on others it is off, so collapsing "no opinion" into
/// "off" silently changes behaviour the moment a model id changes.
///
/// Whether a model *can* reason is a different question, answered by
/// [`crate::ModelCapabilities::thinking`]. It is deliberately not a variant here: a request
/// says what you want, a capability says what is possible, and one enum carrying both would
/// be two answers to two questions in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Thinking {
    /// No opinion. Whatever the model does by default.
    #[default]
    Unset,
    /// Do not reason.
    Off,
    /// Reason at this level.
    On(Effort),
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
/// use llmr::{ChatRequest, Message};
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
            thinking: Thinking::Unset,
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
    fn no_opinion_is_not_the_same_as_off() {
        // The distinction a bool cannot hold. On a model that reasons by default, sending
        // `Off` turns it off and sending `Unset` leaves it on, and a caller that meant
        // neither gets whichever the library chose for it.
        assert_ne!(Thinking::Unset, Thinking::Off);
        assert_eq!(Thinking::default(), Thinking::Unset);
    }

    #[test]
    fn no_opinion_asks_for_nothing() {
        // `Unset` must not read as a request for reasoning, or every default request would
        // be billed for thinking tokens nobody asked for.
        let request = ChatRequest::new("m", vec![Message::user("hi")]);
        assert!(!request.needs().thinking);
    }
}
