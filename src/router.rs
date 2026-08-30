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
//!
//! # Streaming, and the one place falling through stops working
//!
//! [`Router::stream`] routes a streamed call the same way [`Router::chat`] routes a whole
//! one, with one rule that only applies here: **a route can be replaced right up until the
//! caller has seen something, and never afterwards.**
//!
//! Once a chunk has reached you, moving to another provider means handing half a sentence to
//! a second model and asking it to continue. What comes out is text nobody wrote, in one
//! voice, and nothing downstream can tell. So a failure while the stream is opening falls
//! through normally, and a failure after that arrives as an `Err` item inside the stream and
//! stays there.

use crate::chat::request::ChatRequest;
use crate::chat::response::ChatResponse;
use crate::chat::stream::EventStream;
use crate::error::{Error, Result};
use crate::model::{ModelCapabilities, ModelId};
use crate::observe;
use crate::provider::{Access, Provider};
use crate::retry::Retry;
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
    /// The model must accept an image.
    pub images: bool,
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
            images: needs.images,
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
            && (!self.images || have.images)
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
        if self.images && !have.images {
            missing.push("images");
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
#[non_exhaustive]
pub struct Attempted {
    /// Which route, as [`Route::name`] writes it.
    pub route: String,
    /// Why it did not serve the request, in one line.
    pub why: String,
}

/// A reply, and what it took to get one.
///
/// Generic in what came back, and [`ChatResponse`] unless you say otherwise, so `Routed`
/// means what it always did. [`Router::stream`] answers a `Routed<()>` beside the stream
/// itself: an [`EventStream`] is neither `Debug` nor `Clone` and putting
/// one in here would take both away from every caller of [`Router::chat`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Routed<T = ChatResponse> {
    /// The reply.
    pub response: T,
    /// The route that produced it.
    pub route: String,
    /// The routes tried first, in order, and why each one did not answer.
    ///
    /// Empty on the common path. A non empty list on a successful call is the most useful
    /// thing in a log: it is a provider degrading while nothing is failing.
    pub fell_through: Vec<Attempted>,
    /// How many times a provider was actually called.
    ///
    /// One on the common path. More than one means a [`Retry`] policy repeated something,
    /// and that is worth seeing: a call that succeeded on the third attempt cost three, and
    /// nothing else in a successful reply says so. Routes skipped for want of a capability
    /// are not counted here — nothing was sent.
    pub attempts: u32,
}

impl Routed<()> {
    /// The same journey, carrying something.
    ///
    /// One place where a `Routed` is rebuilt around a result, so the two entry points cannot
    /// come to different conclusions about what they travelled through.
    fn carrying<T>(self, response: T) -> Routed<T> {
        Routed {
            response,
            route: self.route,
            fell_through: self.fell_through,
            attempts: self.attempts,
        }
    }
}

/// Several providers, tried in order.
///
/// Immutable once built, like a provider. No lock, nothing to contend on, safe to share
/// across as many tasks as you like.
pub struct Router {
    routes: Vec<Route>,
    retry: Option<Retry>,
}

impl Router {
    /// A router over these routes, tried in this order.
    ///
    /// Nothing is retried. Say so with [`Router::retrying`] if you want it.
    pub fn new(routes: Vec<Route>) -> Self {
        Self {
            routes,
            retry: None,
        }
    }

    /// Repeat a failed call, on the terms this policy sets.
    ///
    /// Applied per route: a route that fails is tried again before the next one is tried at
    /// all. A rate limit is usually the same account whichever route you take, so falling
    /// through to the next provider on the first 429 spends a second provider's budget to
    /// learn what waiting would have told you for free.
    ///
    /// A refusal still stops everything, retries included. It is an answer.
    #[must_use]
    pub fn retrying(mut self, policy: Retry) -> Self {
        self.retry = Some(policy);
        self
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

    /// Asks every route whether it can be reached, once, at startup.
    ///
    /// The other half of [`Router::unusable`]. That one catches a route whose provider does
    /// not know its model, which is a typo. This catches the route whose credential was
    /// rejected, whose tool is not installed, or whose account cannot reach the model, which
    /// are the same question asked of the outside world.
    ///
    /// One entry per route, named as [`Route::name`] writes it, in the order the routes were
    /// given. No route is dropped: a [`Access::Denied`] one stays selectable, exactly as
    /// [`crate::Registry::stale`] reports a row and does not remove it. Pruning on the
    /// strength of a check is a decision somebody should make, and an [`Access::Unknown`] is
    /// not grounds for it at all.
    ///
    /// # Where to call it
    ///
    /// At startup, beside `unusable`, and nowhere near [`Router::chat`]. Validating per
    /// request doubles every round trip to learn something that was almost always true, and
    /// the answer would be stale by the time the request went out anyway.
    ///
    /// Routes are asked one after another rather than together. This runs once, against a
    /// handful of providers, and running them concurrently would mean a join primitive and
    /// an assumption about the runtime for a saving nobody can measure.
    pub async fn preflight(&self) -> Vec<(String, Access)> {
        let mut reached = Vec::with_capacity(self.routes.len());
        for route in &self.routes {
            reached.push((route.name(), route.provider.validate(&route.model).await));
        }
        reached
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
        let span = observe::routing(&request.model);
        let journey = self.attempt(request, needs, |provider, request| provider.chat(request));
        let (response, routed) = observe::inside(span.clone(), journey).await?;

        observe::routed(
            &span,
            &routed.route,
            response.usage.coverage(),
            routed.attempts,
            routed.fell_through.len(),
        );
        Ok(routed.carrying(response))
    }

    /// Starts a stream on the first route that can serve it.
    ///
    /// The streaming half of [`Router::chat`], and the same request rewriting applies: the
    /// route's model replaces the request's, because the router is choosing the model.
    ///
    /// # Falling through stops at the first event
    ///
    /// **A route is only replaceable before the caller has seen anything.** Once a chunk has
    /// reached you, moving to another provider would mean handing half a sentence to a
    /// second model and asking it to continue: text nobody wrote, in one voice, with nothing
    /// downstream able to detect it.
    ///
    /// So a provider that fails while the stream is being opened falls through normally, and
    /// a provider that fails after that does not. The second kind arrives as an `Err` item
    /// inside the stream and stays there, which is what [`crate::Transcript::drain`] is for:
    /// what already arrived is still yours.
    ///
    /// The seam is real rather than assumed.
    /// [`HttpTransport::send_streaming`](crate::HttpTransport::send_streaming) checks the
    /// status before handing over any bytes, so a 429 or a 503 is an `Err` from this method
    /// and is fallen through, not an error mid-answer.
    ///
    /// # What comes back
    ///
    /// The stream, and a `Routed<()>` carrying which route won and what fell through, exactly
    /// as [`Router::chat`] does. They are two values rather than one because an
    /// [`EventStream`] is neither `Debug` nor `Clone`.
    ///
    /// There is no usage yet when this returns. A streamed call reports its tokens in its
    /// final frame, so what you want is [`crate::Transcript`], and it is the transcript that
    /// knows what the call consumed.
    ///
    /// ```no_run
    /// # use llmr::{ChatRequest, Message, Requirements, Router, Transcript};
    /// # async fn example(router: &Router) -> llmr::Result<()> {
    /// let request = ChatRequest::new("gpt-5", vec![Message::user("Hello")]);
    /// let needs = Requirements::of(&request).streaming();
    ///
    /// let mut transcript = Transcript::new(request.model.clone());
    /// let (events, routed) = router.stream(request, needs).await?;
    /// let outcome = transcript.drain(events).await;
    ///
    /// println!("{} answered", routed.route);
    /// let reply = transcript.finish();
    /// if let Err(cut_short) = outcome {
    ///     eprintln!("{} arrived before {cut_short}", reply.text().len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # A provider that does not really stream
    ///
    /// [`crate::Provider::stream`] has a default that calls `chat` and replays the finished
    /// reply as one burst. Routing to one of those is not an error and this method does not
    /// prevent it, for the reason the trait does not: it is a real answer with the same text
    /// and the same usage, just all at once.
    ///
    /// It does mean the window in which this can fall through covers the whole call rather
    /// than the first byte, which is more forgiving and not less. Say
    /// [`Requirements::streaming`] when a person is watching a screen, and a pairing that
    /// only pretends is skipped before it is asked.
    ///
    /// # Errors
    ///
    /// The same as [`Router::chat`]: [`Error::Unsupported`] when no route meets the
    /// requirements, and otherwise the last error from the routes that were tried.
    pub async fn stream(
        &self,
        request: ChatRequest,
        needs: Requirements,
    ) -> Result<(EventStream<'_>, Routed<()>)> {
        let span = observe::routing(&request.model);
        let journey = self.attempt(request, needs, |provider, request| provider.stream(request));
        let (events, routed) = observe::inside(span.clone(), journey).await?;

        observe::routed_stream(
            &span,
            &routed.route,
            routed.attempts,
            routed.fell_through.len(),
        );
        Ok((events, routed))
    }

    /// Picks a route and calls it, retrying and falling through on the terms this router was
    /// built with.
    ///
    /// One body for [`Router::chat`] and [`Router::stream`], because the interesting part is
    /// identical and the two things it must get right are the ones that rot when copied: a
    /// refusal stops everything, and every skipped route is reported. The only difference is
    /// which method of the provider is called, so that is the argument.
    ///
    /// Written as its own function rather than inlined so the span can wrap it rather than be
    /// entered around it. A span guard held across an await attaches to whatever the thread
    /// does next.
    async fn attempt<'a, T, Fut>(
        &'a self,
        request: ChatRequest,
        needs: Requirements,
        call: impl Fn(&'a dyn Provider, ChatRequest) -> Fut,
    ) -> Result<(T, Routed<()>)>
    where
        Fut: std::future::Future<Output = Result<T>> + 'a,
    {
        let mut fell_through = Vec::new();
        let mut last: Option<Error> = None;
        let mut attempts = 0;

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

            // One pass per attempt this route is allowed. Without a policy that is one.
            let allowed = self.retry.as_ref().map_or(1, Retry::attempts);
            for attempt in 1..=allowed {
                attempts += 1;
                match call(route.provider.as_ref(), sending.clone()).await {
                    // Answered. For a stream this is the seam the whole method rests on:
                    // the provider has handed over a stream and not yet a single event, so
                    // this is the last moment anything could have been served by somebody
                    // else.
                    Ok(response) => {
                        return Ok((
                            response,
                            Routed {
                                response: (),
                                route: route.name(),
                                fell_through,
                                attempts,
                            },
                        ));
                    }
                    Err(error) => {
                        // A refusal is an answer about the work, not a provider being
                        // unreachable. Asking the next model the same question is how a
                        // policy decision gets shopped around until something agrees, so it
                        // stops here — and a retry would be the same thing against one
                        // model rather than several.
                        if matches!(error, Error::Refused { .. }) {
                            return Err(error);
                        }

                        let waiting = self
                            .retry
                            .as_ref()
                            .and_then(|policy| policy.wait_before(attempt + 1, &error));

                        fell_through.push(Attempted {
                            route: route.name(),
                            why: match waiting {
                                // Recorded before the wait, and said out loud. A retry that
                                // left no trace is a call somebody paid for twice with one
                                // line in the log to show for it.
                                Some(wait) => format!(
                                    "{error} (attempt {attempt} of {allowed}, waiting {}ms)",
                                    wait.as_millis()
                                ),
                                None => error.to_string(),
                            },
                        });
                        last = Some(error);

                        match waiting {
                            Some(wait) => {
                                if let Some(policy) = &self.retry {
                                    policy.sleep(wait).await;
                                }
                            }
                            // Settled, or out of attempts. On to the next route.
                            None => break,
                        }
                    }
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
