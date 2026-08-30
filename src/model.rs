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
/// It answers two questions that are easy to confuse, and a single boolean gets one of them
/// wrong:
///
/// * [`Reach::is_on_device`] asks whether the data stays on your hardware.
/// * [`Reach::uses_local_credential`] asks whether the key lives on this machine.
///
/// A vendor CLI is the case that separates them. It signs in on your laptop and still sends
/// every prompt to the vendor. Code that treats "local credential" as "local data" will send
/// a customer record to a third party and log it as private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Reach {
    /// The vendor's own hosted API.
    FirstPartyApi,
    /// The same model served by a cloud partner, such as a hyperscaler's model service.
    CloudPartner,
    /// A private deployment you control, reached over the network.
    PrivateEndpoint,
    /// A vendor command line tool on this machine, using the login it already has.
    LocalCli,
    /// Weights you run yourself. The only reach where nothing leaves the hardware.
    SelfHosted,
}

impl Reach {
    /// Every reach this crate knows, in the order above.
    pub const ALL: [Reach; 5] = [
        Reach::FirstPartyApi,
        Reach::CloudPartner,
        Reach::PrivateEndpoint,
        Reach::LocalCli,
        Reach::SelfHosted,
    ];

    /// Whether the data stays on your own machines.
    pub fn is_on_device(self) -> bool {
        matches!(self, Reach::SelfHosted)
    }

    /// Whether the credential is local even though the data is not.
    pub fn uses_local_credential(self) -> bool {
        matches!(self, Reach::LocalCli | Reach::SelfHosted)
    }

    /// How a reach is written down, in configuration and in records.
    ///
    /// One spelling in one place. Two copies of this mapping is two chances for a config
    /// file and a database column to disagree about what `local-cli` means.
    pub fn as_str(self) -> &'static str {
        match self {
            Reach::FirstPartyApi => "first-party-api",
            Reach::CloudPartner => "cloud-partner",
            Reach::PrivateEndpoint => "private-endpoint",
            Reach::LocalCli => "local-cli",
            Reach::SelfHosted => "self-hosted",
        }
    }

    /// Reads a reach from its written form.
    ///
    /// Accepts the short forms `api` and `cli`, and treats underscores and hyphens as the
    /// same character, because configuration files disagree about which one to use.
    pub fn parse(name: &str) -> Option<Reach> {
        let name = name.trim().to_ascii_lowercase().replace('_', "-");
        Some(match name.as_str() {
            "first-party-api" | "api" => Reach::FirstPartyApi,
            "cloud-partner" => Reach::CloudPartner,
            "private-endpoint" => Reach::PrivateEndpoint,
            "local-cli" | "cli" => Reach::LocalCli,
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
/// model behind a vendor CLI often cannot take a tool schema or return a cache breakpoint,
/// because the CLI does not expose those, and the model has nothing to do with it.
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
    /// Whether the reply can be read as it arrives rather than all at once.
    ///
    /// A fact about the pairing, not the model. A command line tool that prints one JSON
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
    fn a_local_cli_keeps_the_key_and_still_sends_the_data_away() {
        assert!(Reach::LocalCli.uses_local_credential());
        assert!(!Reach::LocalCli.is_on_device());
    }

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
        assert_eq!(Reach::parse("local_cli"), Some(Reach::LocalCli));
        assert_eq!(Reach::parse("  Local-CLI "), Some(Reach::LocalCli));
        assert_eq!(Reach::parse("cli"), Some(Reach::LocalCli));
    }

    #[test]
    fn a_reach_this_crate_does_not_know_is_none_rather_than_a_default() {
        assert_eq!(Reach::parse("carrier-pigeon"), None);
    }
}
