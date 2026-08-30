//! A contract suite for anyone writing a provider.
//!
//! Enable the `testkit` feature and point [`assert_provider_contract`] at your provider.
//! It checks the promises [`crate::Provider`] makes, which are the ones a caller relies on
//! without being able to see your code.
//!
//! Every provider in this crate passes it, including the local command line one. That is
//! the point: a suite only one implementation can pass has stopped being a specification
//! and become a description of that implementation.
//!
//! What it does not check is concurrency. Running calls at the same time needs a runtime to
//! run them on, and this module does not assume one. That check lives in this crate's own
//! `tests/concurrency.rs`, and the shape it uses is worth copying: spawn the calls, join
//! them, and put a timeout around the whole thing so a deadlock fails instead of hanging.
//!
//! ```no_run
//! # #[cfg(feature = "testkit")]
//! # async fn example(mine: &impl llmr::Provider) {
//! use llmr::testkit::assert_provider_contract;
//!
//! assert_provider_contract(mine, "the-model-you-serve").await;
//! # }
//! ```

// This module is assertion helpers. Failing loudly is the job, so the crate wide ban on
// panicking is lifted here and nowhere else.
#![allow(clippy::panic)]

use crate::chat::message::Message;
use crate::chat::request::ChatRequest;
use crate::chat::stream::Transcript;
use crate::cost::usage::UsageCoverage;
use crate::model::ModelId;
use crate::provider::{Access, Provider};

/// Checks the promises a provider makes.
///
/// `known_model` must be a model this provider serves. The suite sends one small request
/// to it, so it costs a call.
///
/// # Panics
///
/// Panics with a message naming the promise that was broken. It is a test helper, so
/// failing loudly is the job.
pub async fn assert_provider_contract(provider: &impl Provider, known_model: &str) {
    let id = provider.id().to_string();
    assert!(!id.trim().is_empty(), "a provider with no id");
    assert!(
        !id.contains(char::is_whitespace),
        "{id:?} has whitespace in it. This goes into records and reports beside every call, \
         and a name with a space in it splits when somebody parses a line"
    );

    // A model nobody serves must answer None rather than something plausible. The two are
    // different questions, and a caller that cannot tell them apart will send a request to
    // a model that does not exist and read the failure as a network problem.
    let unknown = ModelId::from("llmr-contract-no-such-model");
    assert!(
        provider.capabilities(&unknown).is_none(),
        "{id} claims to know a model that does not exist. None means unknown; a capability \
         set with everything off means known and unable"
    );

    let model = ModelId::from(known_model);
    if let Some(caps) = provider.capabilities(&model) {
        assert!(
            caps.max_output <= caps.context_window || caps.context_window == 0,
            "{id} says {known_model} produces more tokens than fit in its window"
        );
    }

    // Reachability, before anything is sent. A provider that has nothing free to ask may
    // answer `Unknown` to all of this and still pass: what it may not do is contradict
    // itself.
    let access = provider.validate(&model).await;
    assert!(
        !access.is_denied(),
        "{id} says {known_model} cannot be reached ({access}), and this suite is about to \
         reach it. Denied is for a settled no, so one of the two is wrong"
    );

    assert!(
        !provider.validate(&unknown).await.is_ready(),
        "{id} says a model that does not exist is reachable. That is the same failure as a \
         provider claiming to know every model name, one method along: it turns a typo into \
         something a router will happily select"
    );

    // Asked twice, for the same reason `chat` is below. An answer that changed here without
    // anything else changing means the provider is remembering, and a remembered answer is a
    // claim about a moment that has passed.
    let again = provider.validate(&model).await;
    assert_eq!(
        access.as_str(),
        again.as_str(),
        "{id} answered {access} and then {again} for the same model. Either it cached the \
         first answer or it is holding state between calls"
    );

    let reply = provider
        .chat(
            ChatRequest::new(
                model.clone(),
                vec![Message::user("Reply with the word ok.")],
            )
            .with_max_tokens(16),
        )
        .await;

    let reply = match reply {
        Ok(reply) => reply,
        Err(e) => panic!("{id} could not answer a one word request: {e}"),
    };

    assert!(
        !reply.message.content.is_empty(),
        "{id} returned a reply with no content. An unreadable reply is an error, never an \
         empty answer, because a caller cannot tell an empty answer from a failure"
    );

    assert!(
        !reply.model.as_str().trim().is_empty(),
        "{id} did not say which model served the request. Price against this one, not \
         against what was asked for, so it has to be there"
    );

    // Usage may be absent. What it must not be is invented. A provider that reports nothing
    // and writes zeros turns an unknown cost into a free one.
    if reply.usage.coverage() != UsageCoverage::Absent {
        assert!(
            reply.usage.output_tokens.unwrap_or(1) > 0 || !reply.text().is_empty(),
            "{id} reported zero output tokens for a reply that has text in it"
        );
    }

    // The same question, streamed. Every provider answers `stream`, whether it really
    // streams or hands the finished reply over in one burst, so this holds for all of them.
    let streamed = provider
        .stream(
            ChatRequest::new(
                model.clone(),
                vec![Message::user("Reply with the word ok.")],
            )
            .with_max_tokens(16),
        )
        .await;

    match streamed {
        Err(e) => panic!("{id} could not answer the same request as a stream: {e}"),
        Ok(stream) => {
            let mut transcript = Transcript::new(model.clone());
            if let Err(e) = transcript.drain(stream).await {
                panic!("{id} broke partway through a stream: {e}");
            }
            let assembled = transcript.finish();

            assert!(
                !assembled.message.content.is_empty(),
                "{id} streamed a reply with no content in it. The same rule as `chat`: an \
                 unreadable reply is an error, never an empty answer"
            );

            assert!(
                assembled.stop_reason != crate::chat::message::StopReason::Interrupted,
                "{id} ended a stream without ever saying why it stopped. That reads as a \
                 connection that broke, so a caller cannot tell this apart from one that did"
            );

            // The one that matters. Two ways to ask the same question that disagree about
            // what the call cost make every cost report depend on which one you used.
            assert_eq!(
                assembled.usage.coverage(),
                reply.usage.coverage(),
                "{id} reports usage differently depending on whether the reply was streamed. \
                 A streamed call that reports nothing where a whole one reports numbers \
                 becomes a free call in every report that adds it up"
            );

            assert!(
                !assembled.model.as_str().trim().is_empty(),
                "{id} did not say which model served the streamed request"
            );
        }
    }

    // Asked twice, because a provider holding state between calls is the bug this cannot
    // otherwise see. The answers may differ. What must not differ is that both arrive.
    let again = provider
        .chat(
            ChatRequest::new(model, vec![Message::user("Reply with the word ok.")])
                .with_max_tokens(16),
        )
        .await;
    assert!(
        again.is_ok(),
        "{id} answered once and failed the second time, which usually means state left \
         over from the first call"
    );
}

/// Checks that a provider built with a credential it will reject says so.
///
/// A second entry point rather than part of [`assert_provider_contract`], because the suite
/// cannot break your credential for you: only you can build the provider with the wrong key,
/// the wrong account, or a tool that is signed out.
///
/// It is worth the call it asks of you. [`Access::Unknown`] reads as "ask again later", so a
/// router keeps the provider, a retry loop keeps trying it, and the one failure that needs a
/// person is the one that never surfaces. `Denied` is what stops that.
///
/// ```no_run
/// # #[cfg(feature = "testkit")]
/// # async fn example(with_a_bad_key: &impl llmr::Provider) {
/// use llmr::testkit::assert_a_bad_credential_is_denied;
///
/// assert_a_bad_credential_is_denied(with_a_bad_key, "the-model-you-serve").await;
/// # }
/// ```
///
/// # Panics
///
/// Panics naming what was answered instead. It is a test helper, so failing loudly is the
/// job.
pub async fn assert_a_bad_credential_is_denied(provider: &impl Provider, model: &str) {
    let id = provider.id();
    let access = provider.validate(&ModelId::from(model)).await;

    match access {
        Access::Denied { .. } => {}
        Access::Ready => panic!(
            "{id} says it is ready with a credential that will be rejected. Whatever this \
             checked, it was not the credential"
        ),
        Access::Unknown { ref why } => panic!(
            "{id} reports a rejected credential as unknown ({why}). Unknown means ask again \
             later, so a router keeps this provider and a retry loop keeps trying it. A key \
             nobody is told about is a key nobody fixes"
        ),
    }
    // No catch all arm. `Access` is non exhaustive to the outside and not in here, so a
    // variant added later stops this compiling, which is where somebody decides what the
    // new answer means for a rejected credential.
}
