//! Spans, when you asked for them.
//!
//! Behind the `tracing` feature. With it off this module compiles to nothing, the crate
//! gains no dependency, and no work is done on the path a request takes. A library that
//! emitted whether you asked or not is a library people work around.
//!
//! # What a span may carry
//!
//! Facts about the call: which provider, which model, which reach, how complete the usage
//! was, which route answered, how many attempts it took.
//!
//! **Never the prompt and never a credential.** That is not a convention here, it is the
//! shape of the code: the functions below take a [`ModelId`], a [`Reach`], a
//! [`UsageCoverage`] and a `&str` route name, and there is nowhere to pass a message even
//! by accident. [`crate::Secret`] does not implement `Display`, so a key cannot be recorded
//! either. A span that logged a request body would undo the whole point of `Secret` in one
//! line, and it would do it in the logs of every program that upgraded.
//!
//! # Why the router, and why `fell_through`
//!
//! `Routed::fell_through` is the most useful thing this crate produces and it lives in a
//! struct most callers never look at. A successful call that took the third route is a
//! provider going bad while every dashboard stays green, and the same is true of a call
//! that succeeded on its second attempt. Here is where those become visible without anybody
//! writing the logging themselves.

use crate::cost::usage::UsageCoverage;
use crate::model::{ModelId, Reach};
use std::future::Future;

/// The span a call runs inside.
///
/// A real one with the feature on, and a unit with it off, so the call sites read the same
/// either way and neither branch drifts.
#[cfg(feature = "tracing")]
pub(crate) type Span = tracing::Span;

/// The same, costing nothing.
///
/// A zero sized struct rather than `()`, so the call sites do not read as passing units
/// around — which is both what clippy sees and what a reader would.
///
/// Deliberately `Clone` and not `Copy`: [`tracing::Span`] is cloned at the call sites, and a
/// `Copy` stand-in would make the same line a lint under one feature set and correct under
/// the other.
#[cfg(not(feature = "tracing"))]
#[derive(Debug, Clone)]
pub(crate) struct Span;

/// A span for one routed request.
///
/// The model is the one asked for; the route and the outcome are recorded later, because
/// they are not known yet when a call starts.
#[cfg(feature = "tracing")]
pub(crate) fn routing(model: &ModelId) -> Span {
    tracing::info_span!(
        "llmr.route",
        model = model.as_str(),
        route = tracing::field::Empty,
        reach = tracing::field::Empty,
        usage_coverage = tracing::field::Empty,
        attempts = tracing::field::Empty,
        fell_through = tracing::field::Empty,
    )
}

#[cfg(not(feature = "tracing"))]
pub(crate) fn routing(_model: &ModelId) -> Span {
    Span
}

/// A span for one call to one provider.
#[cfg(feature = "tracing")]
pub(crate) fn calling(provider: &str, model: &ModelId, reach: Reach) -> Span {
    tracing::info_span!(
        "llmr.call",
        provider = provider,
        model = model.as_str(),
        reach = reach.as_str(),
        usage_coverage = tracing::field::Empty,
    )
}

#[cfg(not(feature = "tracing"))]
pub(crate) fn calling(_provider: &str, _model: &ModelId, _reach: Reach) -> Span {
    Span
}

/// Records how a routed call turned out.
///
/// Takes only the pieces that are safe to record. There is deliberately no argument here
/// that could hold a message.
#[cfg(feature = "tracing")]
pub(crate) fn routed(
    span: &Span,
    route: &str,
    coverage: UsageCoverage,
    attempts: u32,
    fell: usize,
) {
    span.record("route", route);
    span.record("usage_coverage", coverage.as_str());
    span.record("attempts", attempts);
    span.record("fell_through", fell);
    if fell > 0 {
        // The line worth having. Nothing failed, and something is going wrong.
        tracing::warn!(
            route,
            fell_through = fell,
            attempts,
            "answered, but not by the first route that was tried"
        );
    }
}

#[cfg(not(feature = "tracing"))]
pub(crate) fn routed(
    _span: &Span,
    _route: &str,
    _coverage: UsageCoverage,
    _attempts: u32,
    _fell: usize,
) {
}

/// Records how complete the usage on one provider call was.
#[cfg(feature = "tracing")]
pub(crate) fn measured(span: &Span, coverage: UsageCoverage) {
    span.record("usage_coverage", coverage.as_str());
    if coverage == UsageCoverage::Absent {
        // Somebody will try to price this later and find nothing to price it with.
        tracing::debug!("the provider reported no usage for this call");
    }
}

#[cfg(not(feature = "tracing"))]
pub(crate) fn measured(_span: &Span, _coverage: UsageCoverage) {}

/// Runs a future inside a span.
///
/// The span is attached to the future rather than entered around it, because a span guard
/// held across an await attaches the span to whatever else that thread picks up next.
pub(crate) async fn inside<F: Future>(span: Span, future: F) -> F::Output {
    #[cfg(feature = "tracing")]
    {
        use tracing::Instrument;
        future.instrument(span).await
    }
    #[cfg(not(feature = "tracing"))]
    {
        let Span = span;
        future.await
    }
}
