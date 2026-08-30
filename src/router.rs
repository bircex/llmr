//! Several providers, and how a request finds one.
//!
//! A bag of providers is not a layer. This is the part that makes it one: you describe what
//! a request needs, and the router picks something that can serve it, in an order you chose,
//! falling through when one is unreachable.
//!
//! # What it routes on
//!
//! Three things, and no others.
//!
//! * **Capability.** A request offering tools needs a model that takes tools. Asking one
//!   that does not is paying for a reply that ignored half of what you sent.
//! * **Reach.** A caller can say the data must not leave this machine. Nothing below that
//!   floor is considered, whatever else it can do.
//! * **Order.** The routes are tried in the order you gave them. First that fits, wins.
//!
//! # What it deliberately does not route on
//!
//! Anything about *your* work. There is no notion here of a task being a security review or
//! a summary, because that is a fact about your system and not about a model. Decide which
//! [`Requirements`] a piece of work has, and this decides which provider meets them.
//!
//! That line is what keeps the router useful to more than one program. A router that knew
//! what a security review was would be one only its author could use.

use crate::chat::request::ChatRequest;
use crate::chat::response::ChatResponse;
use crate::error::{Error, Result};
use crate::model::{ModelCapabilities, ModelId};
use crate::provider::Provider;
use std::sync::Arc;

/// One way to reach one model.
///
/// A provider can serve several, so a route pairs the two and gives the pairing a name you
/// can read in a report.
pub struct Route {
    provider: Arc<dyn Provider>,
    model: ModelId,
}

impl Route {
    /// A model, through this provider.
    pub fn new(provider: Arc<dyn Provider>, model: impl Into<ModelId>) -> Self {
        Self {
            provider: provider.clone(),
            model: model.into(),
        }
    }

    /// What this pairing can do, as the provider describes it.
    ///
    /// `None` when the provider does not know this model, which is why a route can be
    /// configured and never chosen. [`Router::unusable`] reports those, because a route
    /// nothing can select is a typo somebody has not noticed.
    pub fn capabilities(&self) -> Option<ModelCapabilities> {
        self.provider.capabilities(&self.model)
    }

    /// Which provider and which model, for a report.
    pub fn name(&self) -> String {
        format!("{}/{}", self.provider.id(), self.model)
    }
}

/// What a request needs from whatever serves it.
///
/// Build one from a request with [`Requirements::of`], then add the constraints your
/// program has that a request cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Requirements {
    /// The model must accept tools.
    pub tools: bool,
    /// The model must accept a response schema.
    pub structured_output: bool,
    /// The model must support prompt caching.
    pub prompt_caching: bool,
    /// The model must be able to reason.
    pub thinking: bool,
    /// The reply must be readable as it arrives.
    ///
    /// Not something a request can express, because it is about how you intend to read the
    /// answer rather than about what you asked. Set it with [`Requirements::streaming`].
    pub streaming: bool,
    /// The data may not leave this machine.
    ///
    /// A floor rather than a preference. When this is set, only [`crate::Reach::SelfHosted`] is
    /// considered, and a router with nothing self hosted refuses rather than falling back.
    /// Falling back here would be sending private data to a vendor because the local model
    /// was busy.
    pub must_stay_on_device: bool,
}

impl Requirements {
    /// What this request needs, read from the request itself.
    ///
    /// Covers the four capability questions. It cannot know whether your data may leave the
    /// machine, so set [`Requirements::on_device`] when it may not.
    pub fn of(request: &ChatRequest) -> Requirements {
        let needs = request.needs();
        Requirements {
            tools: needs.tools,
            structured_output: needs.structured_output,
            prompt_caching: needs.prompt_caching,
            thinking: needs.thinking,
            // Neither of these is in the request. One is about how you will read the reply,
            // the other about where your data may go, and a request says nothing about
            // either.
            streaming: false,
            must_stay_on_device: false,
        }
    }

    /// Nothing but a self hosted model will do.
    #[must_use]
    pub fn on_device(mut self) -> Self {
        self.must_stay_on_device = true;
        self
    }

    /// The reply has to arrive in pieces.
    ///
    /// Worth setting when a person is watching. Every provider answers
    /// [`crate::Provider::stream`], but one that cannot really stream
    /// answers it all at once at the end, and routing to it is how a screen stays blank for
    /// thirty seconds while nothing appears to be wrong.
    #[must_use]
    pub fn streaming(mut self) -> Self {
        self.streaming = true;
        self
    }

    /// Whether these capabilities are enough.
    pub fn met_by(self, have: &ModelCapabilities) -> bool {
        if self.must_stay_on_device && !have.reach.is_on_device() {
            return false;
        }
        (!self.tools || have.tools)
            && (!self.structured_output || have.structured_output)
            && (!self.prompt_caching || have.prompt_caching)
            && (!self.thinking || have.thinking)
            && (!self.streaming || have.streaming)
    }

    /// What is missing, by name, for a message somebody reads.
    pub fn unmet_by(self, have: &ModelCapabilities) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.must_stay_on_device && !have.reach.is_on_device() {
            missing.push("on-device");
        }
        if self.tools && !have.tools {
            missing.push("tools");
        }
        if self.structured_output && !have.structured_output {
            missing.push("structured_output");
        }
        if self.prompt_caching && !have.prompt_caching {
            missing.push("prompt_caching");
        }
        if self.streaming && !have.streaming {
            missing.push("streaming");
        }
        if self.thinking && !have.thinking {
            missing.push("thinking");
        }
        missing
    }
}

/// What happened on the way to an answer.
///
/// Kept because a reply that arrived on the third route is a different fact from one that
/// arrived on the first, and a program that cannot tell them apart cannot see a provider
/// going bad.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempted {
    /// Which route, as [`Route::name`] writes it.
    pub route: String,
    /// Why it did not serve the request, in one line.
    pub why: String,
}

/// A reply, and what it took to get one.
#[derive(Debug, Clone)]
pub struct Routed {
    /// The reply.
    pub response: ChatResponse,
    /// The route that produced it.
    pub route: String,
    /// The routes tried first, in order, and why each one did not answer.
    ///
    /// Empty on the common path. A non empty list on a successful call is the most useful
    /// thing in a log: it is a provider degrading while nothing is failing.
    pub fell_through: Vec<Attempted>,
}

/// Several providers, tried in order.
///
/// Immutable once built, like a provider. No lock, nothing to contend on, safe to share
/// across as many tasks as you like.
pub struct Router {
    routes: Vec<Route>,
}

impl Router {
    /// A router over these routes, tried in this order.
    pub fn new(routes: Vec<Route>) -> Self {
        Self { routes }
    }

    /// Routes that no request can ever select.
    ///
    /// A route whose provider does not know its model, by name. Usually a typo, and without
    /// this it shows up as a fallback that quietly never fires. Worth calling at startup.
    pub fn unusable(&self) -> Vec<String> {
        self.routes
            .iter()
            .filter(|route| route.capabilities().is_none())
            .map(Route::name)
            .collect()
    }

    /// Every route, with what it can do.
    pub fn routes(&self) -> impl Iterator<Item = (String, Option<ModelCapabilities>)> + '_ {
        self.routes
            .iter()
            .map(|route| (route.name(), route.capabilities()))
    }

    /// Sends a request to the first route that can serve it.
    ///
    /// The request's own model is replaced by the route's, because the router is choosing
    /// the model. Everything else about the request is sent as you wrote it.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when no route meets the requirements, naming what each route
    /// was missing. That is a configuration problem, and a message that only said "no route
    /// available" would leave somebody comparing tables by hand.
    ///
    /// When routes were tried and all failed, the **last** error is returned rather than a
    /// summary. It is a real error from a real provider, with its own retry advice, and a
    /// wrapper around it would lose that.
    pub async fn chat(&self, request: ChatRequest, needs: Requirements) -> Result<Routed> {
        let mut fell_through = Vec::new();
        let mut last: Option<Error> = None;

        for route in &self.routes {
            let Some(capabilities) = route.capabilities() else {
                fell_through.push(Attempted {
                    route: route.name(),
                    why: "the provider does not know this model".into(),
                });
                continue;
            };

            let missing = needs.unmet_by(&capabilities);
            if !missing.is_empty() {
                fell_through.push(Attempted {
                    route: route.name(),
                    why: format!("cannot do {}", missing.join(", ")),
                });
                continue;
            }

            let mut sending = request.clone();
            sending.model = route.model.clone();

            match route.provider.chat(sending).await {
                Ok(response) => {
                    return Ok(Routed {
                        response,
                        route: route.name(),
                        fell_through,
                    })
                }
                Err(error) => {
                    // A refusal is an answer about the work, not a provider being
                    // unreachable. Asking the next model the same question is how a policy
                    // decision gets shopped around until something agrees, so it stops here.
                    if matches!(error, Error::Refused { .. }) {
                        return Err(error);
                    }
                    fell_through.push(Attempted {
                        route: route.name(),
                        why: error.to_string(),
                    });
                    last = Some(error);
                }
            }
        }

        match last {
            Some(error) => Err(error),
            None => Err(Error::Unsupported(format!(
                "no route can serve this request. Tried: {}",
                fell_through
                    .iter()
                    .map(|a| format!("{} ({})", a.route, a.why))
                    .collect::<Vec<_>>()
                    .join("; ")
            ))),
        }
    }
}
