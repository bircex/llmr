//! Which model, where it runs, and what it can do.

use serde::{Deserialize, Serialize};

/// A model name as its provider spells it.
///
/// Kept as a string on purpose. Vendors add and retire models on their own schedule, and an
/// enum of model names would need a release of this crate every time one of them shipped.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelId(pub String);

impl ModelId {
    /// Borrows the name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ModelId {
    fn from(id: &str) -> Self {
        ModelId(id.to_string())
    }
}

impl From<String> for ModelId {
    fn from(id: String) -> Self {
        ModelId(id)
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a model is reached.
///
/// This is the axis that decides where your prompt goes and whose credential pays for it.
/// The question it answers is where a prompt goes: [`Reach::is_on_device`] says whether the
/// data stays on your hardware. Everything but a self hosted model sends it to somebody.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Reach {
    /// The vendor's own hosted API.
    FirstPartyApi,
    /// The same model served by a cloud partner, such as a hyperscaler's model service.
    CloudPartner,
    /// A private deployment you control, reached over the network.
    PrivateEndpoint,
    /// Weights you run yourself. The only reach where nothing leaves the hardware.
    SelfHosted,
}

impl Reach {
    /// Every reach this crate knows, in the order above.
    pub const ALL: [Reach; 4] = [
        Reach::FirstPartyApi,
        Reach::CloudPartner,
        Reach::PrivateEndpoint,
        Reach::SelfHosted,
    ];

    /// Whether the data stays on your own machines.
    pub fn is_on_device(self) -> bool {
        matches!(self, Reach::SelfHosted)
    }

    /// How a reach is written down, in configuration and in records.
    ///
    /// One spelling in one place. Two copies of this mapping is two chances for a config
    /// file and a database column to disagree about what `self-hosted` means.
    pub fn as_str(self) -> &'static str {
        match self {
            Reach::FirstPartyApi => "first-party-api",
            Reach::CloudPartner => "cloud-partner",
            Reach::PrivateEndpoint => "private-endpoint",
            Reach::SelfHosted => "self-hosted",
        }
    }

    /// Reads a reach from its written form.
    ///
    /// Accepts the short form `api`, and treats underscores and hyphens as the
    /// same character, because configuration files disagree about which one to use.
    pub fn parse(name: &str) -> Option<Reach> {
        let name = name.trim().to_ascii_lowercase().replace('_', "-");
        Some(match name.as_str() {
            "first-party-api" | "api" => Reach::FirstPartyApi,
            "cloud-partner" => Reach::CloudPartner,
            "private-endpoint" => Reach::PrivateEndpoint,
            "self-hosted" => Reach::SelfHosted,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Reach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a model can do, as reached this way.
///
/// Read this before you build a request. The point of having it is that a caller finds out
/// what is missing by asking, rather than by sending a request and reading the error.
///
/// Capabilities belong to the pair of model and reach, not to the model alone. The same
/// model behind a gateway often cannot take a tool schema or return a cache breakpoint,
/// because the gateway does not expose those, and the model has nothing to do with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelCapabilities {
    /// How many tokens fit in one request.
    pub context_window: u32,
    /// The most tokens the model will produce in one reply.
    pub max_output: u32,
    /// Whether the model can be given tools and asked to call them.
    pub tools: bool,
    /// Whether the model can be asked for output matching a schema.
    pub structured_output: bool,
    /// Whether repeated prefixes can be cached, so you are billed less for them.
    pub prompt_caching: bool,
    /// Whether the model can be asked to reason before answering.
    pub thinking: bool,
    /// Whether a request may carry an image.
    ///
    /// A fact about the pairing. Some models take images and some do not, and no reach that
    /// speaks only text can carry one whatever the model could do.
    pub images: bool,
    /// Whether a request may carry a document, such as a PDF.
    pub documents: bool,
    /// Whether a request may carry a recording for the model to listen to.
    pub audio: bool,
    /// Whether the reply can be read as it arrives rather than all at once.
    ///
    /// A fact about the pairing, not the model. An endpoint that answers with one JSON
    /// document when it finishes cannot stream whatever model is behind it.
    pub streaming: bool,
    /// Where this pairing runs.
    pub reach: Reach,
}

impl ModelCapabilities {
    /// A capability set with everything off and no room, for a provider to fill in.
    ///
    /// Zeros rather than plausible defaults. A guessed context window is a request that
    /// fails far from the guess, and the caller has no way to know the number was invented.
    pub fn none(reach: Reach) -> Self {
        Self {
            context_window: 0,
            max_output: 0,
            tools: false,
            structured_output: false,
            prompt_caching: false,
            thinking: false,
            images: false,
            documents: false,
            audio: false,
            streaming: false,
            reach,
        }
    }

    /// Sets how much fits in one request and how much comes back.
    #[must_use]
    pub fn with_window(mut self, context_window: u32, max_output: u32) -> Self {
        self.context_window = context_window;
        self.max_output = max_output;
        self
    }

    /// Says the model can be given tools.
    #[must_use]
    pub fn with_tools(mut self) -> Self {
        self.tools = true;
        self
    }

    /// Says the model can be asked for output matching a schema.
    #[must_use]
    pub fn with_structured_output(mut self) -> Self {
        self.structured_output = true;
        self
    }

    /// Says repeated prefixes can be cached.
    #[must_use]
    pub fn with_prompt_caching(mut self) -> Self {
        self.prompt_caching = true;
        self
    }

    /// Says the model can be asked to reason.
    #[must_use]
    pub fn with_thinking(mut self) -> Self {
        self.thinking = true;
        self
    }

    /// Says a request may carry an image.
    #[must_use]
    pub fn with_images(mut self) -> Self {
        self.images = true;
        self
    }

    /// Says a request may carry a document.
    #[must_use]
    pub fn with_documents(mut self) -> Self {
        self.documents = true;
        self
    }

    /// Says a request may carry audio.
    #[must_use]
    pub fn with_audio(mut self) -> Self {
        self.audio = true;
        self
    }

    /// Says the reply can be read as it arrives.
    #[must_use]
    pub fn with_streaming(mut self) -> Self {
        self.streaming = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_self_hosted_keeps_the_data() {
        for reach in Reach::ALL {
            assert_eq!(reach.is_on_device(), reach == Reach::SelfHosted);
        }
    }

    #[test]
    fn every_reach_reads_back_from_how_it_is_written() {
        for reach in Reach::ALL {
            assert_eq!(Reach::parse(reach.as_str()), Some(reach));
        }
    }

    #[test]
    fn configuration_may_spell_it_either_way() {
        assert_eq!(Reach::parse("self_hosted"), Some(Reach::SelfHosted));
        assert_eq!(Reach::parse("  Self-Hosted "), Some(Reach::SelfHosted));
        assert_eq!(Reach::parse("api"), Some(Reach::FirstPartyApi));
    }

    #[test]
    fn a_reach_this_crate_does_not_know_is_none_rather_than_a_default() {
        assert_eq!(Reach::parse("carrier-pigeon"), None);
    }
}
