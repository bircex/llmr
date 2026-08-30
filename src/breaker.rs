//! Remembering that a provider has been failing, so it is not tried first every time.
//!
//! [`crate::Retry`] decides whether to ask the same provider again inside one request. This
//! decides whether to ask it at all in the *next* one. Without it, a provider that has been
//! answering 503 for ten minutes is tried first, waited on and fallen through for every
//! single request, and a router that does not learn is an ordered `try` list.
//!
//! # Handed in, never assumed
//!
//! Like [`crate::Retry`], and for the same reason. Skipping a provider is a decision with
//! consequences a library cannot weigh: a router that quietly stopped trying something would
//! be one people work around by not using the router. [`crate::Router::breaking`] turns it
//! on, and without it the behaviour is exactly what it was.
//!
//! # What opens it, and what does not
//!
//! One rule, and everything below follows from it: **the circuit opens for a failure about
//! the provider, and stays shut for a failure about the request.**
//!
//! | Error | Circuit | Why |
//! |---|---|---|
//! | [`Error::Transient`] | opens, backing off | the provider is having a bad time |
//! | [`Error::Timeout`] | opens, backing off | so is one that will not answer |
//! | [`Error::RateLimited`] | opens for exactly the `retry_after` | it said when it clears |
//! | [`Error::Auth`] | opens, and long | a person has to fix this |
//! | [`Error::NotFound`] | opens, and long | this provider does not have this model |
//! | [`Error::Refused`] | **no** | the model answered, about the work |
//! | [`Error::InvalidRequest`] | **no** | it will be malformed on the next route too |
//! | [`Error::Unsupported`] | **no** | same |
//! | [`Error::Unreadable`] | **no** | one unparseable reply is not a broken provider |
//!
//! [`Error::Unreadable`] is the one worth arguing about. The provider did answer, and a
//! single reply this crate could not read is as likely to be one odd body as a provider gone
//! bad. Taking a working provider out of a router on the strength of it would cost more than
//! it saves, and the call still falls through to the next route either way.

use crate::error::Error;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long a route is left alone after it has been failing.
///
/// Three numbers rather than one, because the failures are not the same kind of thing. A 503
/// clears on its own and should be re-checked soon. A rejected credential does not clear on
/// its own at all, and asking again in thirty seconds is asking a question that has been
/// answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Breaker {
    /// How long a route is skipped after one failure.
    ///
    /// Doubles with each consecutive failure, up to [`Breaker::longest`].
    pub first: Duration,
    /// The longest a route is skipped for a failure that clears on its own.
    ///
    /// A cap rather than a limit on doubling, so a provider that has been down for an hour
    /// is still re-checked every minute rather than every eighteen.
    pub longest: Duration,
    /// How long a route is skipped for a failure a person has to fix.
    ///
    /// A rejected key or a model this provider does not have. Neither improves by being
    /// asked again, and the point of the wait is to stop spending a call per request on a
    /// question that has been settled, not to wait for it to clear.
    pub settled: Duration,
}

impl Default for Breaker {
    /// A second, a minute, and five minutes.
    ///
    /// Arbitrary in the way any such numbers are. What makes them useful is that they are
    /// written down, and that the shape is right: quick to re-check something that clears
    /// itself, slow to re-check something that does not.
    fn default() -> Self {
        Self {
            first: Duration::from_secs(1),
            longest: Duration::from_secs(60),
            settled: Duration::from_secs(300),
        }
    }
}

impl Breaker {
    /// A policy with these three waits.
    pub fn new(first: Duration, longest: Duration, settled: Duration) -> Self {
        Self {
            first,
            longest,
            settled,
        }
    }

    /// How long this route should be skipped, given what went wrong and how often.
    ///
    /// `None` when the circuit must not open at all. See the table in the module
    /// documentation for which failures those are.
    ///
    /// `failures` counts *requests* this route failed to serve in a row, not attempts. A
    /// [`crate::Retry`] policy asking three times inside one request is one failure here:
    /// the wait is about how long to leave a provider alone, and three attempts against a
    /// provider that is down is one piece of evidence, not three.
    ///
    /// ```
    /// # use llmr::breaker::Breaker;
    /// # use llmr::Error;
    /// # use std::time::Duration;
    /// let breaker = Breaker::default();
    /// let down = Error::Transient("503".into());
    ///
    /// assert_eq!(breaker.opening_for(1, &down), Some(Duration::from_secs(1)));
    /// assert_eq!(breaker.opening_for(3, &down), Some(Duration::from_secs(4)));
    /// assert_eq!(breaker.opening_for(30, &down), Some(Duration::from_secs(60)));
    ///
    /// // The model answered, about the work. Nothing about the provider is wrong.
    /// assert_eq!(breaker.opening_for(9, &Error::Refused { category: None }), None);
    /// ```
    pub fn opening_for(&self, failures: u32, error: &Error) -> Option<Duration> {
        match error {
            // It said when it clears. A backoff guessed here would either re-ask too early,
            // which earns another limit, or too late, which throws away time the provider
            // already told us about. Honoured exactly, even when it is longer than
            // `longest`: `longest` is this crate's guess and `retry_after` is not a guess.
            Error::RateLimited {
                retry_after: Some(wait),
            } => Some(*wait),
            Error::RateLimited { retry_after: None } => Some(self.backoff(failures)),

            Error::Transient(_) | Error::Timeout { .. } => Some(self.backoff(failures)),

            // Settled. Not "wait for it to pass" but "stop asking every request".
            Error::Auth(_) | Error::NotFound(_) => Some(self.settled),

            // About the request or about the answer, not about the provider.
            Error::Refused { .. }
            | Error::InvalidRequest(_)
            | Error::Unsupported(_)
            | Error::Unreadable(_) => None,
            // Deliberately no wildcard. `Error` is non exhaustive to the outside world and
            // exhaustive in here, so a variant added later stops this compiling until
            // somebody decides which side of the rule above it falls on. That decision is
            // cheap to make now and invisible if it is defaulted.
        }
    }

    /// The wait after this many consecutive failures, doubling and then capped.
    fn backoff(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(32);
        let scaled = self
            .first
            .checked_mul(1u32.checked_shl(doublings).unwrap_or(u32::MAX))
            .unwrap_or(self.longest);
        scaled.min(self.longest)
    }
}

/// What one route has been doing lately.
///
/// Atomics rather than a lock. [`crate::Router::chat`] takes `&self` so that one router can
/// be shared across as many tasks as a program has, and this crate denies holding a lock
/// across an await crate wide. Two tasks racing to record a failure is fine: they are
/// recording the same fact.
#[derive(Debug, Default)]
pub(crate) struct Health {
    /// Milliseconds since the router was built, before which this route is skipped.
    ///
    /// Measured from the router's own start rather than from the wall clock, so a machine
    /// whose clock steps backwards does not close a circuit for a day. Zero means open, and
    /// a close that works out to zero has already expired, which is the same answer.
    closed_until: AtomicU64,
    /// How many requests in a row this route has failed to serve.
    consecutive_failures: AtomicU32,
}

impl Health {
    /// How much longer this route is being skipped, if it is.
    pub(crate) fn closed_for(&self, since: Instant) -> Option<Duration> {
        let until = self.closed_until.load(Ordering::Relaxed);
        if until == 0 {
            return None;
        }
        let now = millis(since);
        (until > now).then(|| Duration::from_millis(until - now))
    }

    /// How many requests in a row this route has failed.
    pub(crate) fn failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Records a request this route failed to serve, and says how long to skip it for.
    pub(crate) fn failed(&self, breaker: &Breaker, error: &Error, since: Instant) -> Option<u32> {
        let failures = self
            .consecutive_failures
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let wait = breaker.opening_for(failures, error)?;
        self.close_for(wait, since);
        Some(failures)
    }

    /// Skips this route for a while, whatever it has been doing.
    ///
    /// Used by the breaker above and by [`crate::Router::preflight`], which learns the same
    /// thing at startup without a request having failed.
    pub(crate) fn close_for(&self, wait: Duration, since: Instant) {
        let capped = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        self.closed_until
            .store(millis(since).saturating_add(capped), Ordering::Relaxed);
    }

    /// Records a request this route served, which clears everything.
    ///
    /// Both fields, not just the timer. A route that answers has no consecutive failures by
    /// definition, and leaving the count would make its next failure back off as though the
    /// run in between had not happened.
    pub(crate) fn served(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.closed_until.store(0, Ordering::Relaxed);
    }
}

/// Milliseconds since the router was built.
fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_about_the_request_never_opens_the_circuit() {
        // The distinction the whole type rests on. A refusal is the model answering about
        // the work, and a malformed request will be malformed on the next route too. Taking
        // a provider out of a router for either would remove a working provider for a
        // failure it had nothing to do with.
        let breaker = Breaker::default();
        assert_eq!(
            breaker.opening_for(5, &Error::Refused { category: None }),
            None
        );
        assert_eq!(
            breaker.opening_for(5, &Error::InvalidRequest("bad".into())),
            None
        );
        assert_eq!(
            breaker.opening_for(5, &Error::Unsupported("no tools".into())),
            None
        );
        assert_eq!(
            breaker.opening_for(5, &Error::Unreadable("no content".into())),
            None
        );
    }

    #[test]
    fn a_rejected_credential_is_left_alone_for_a_long_time() {
        // Nothing about it improves by being asked again. The wait is not for it to clear,
        // it is to stop spending a call per request on a question that has been answered.
        let breaker = Breaker::default();
        assert_eq!(
            breaker.opening_for(1, &Error::Auth("401".into())),
            Some(breaker.settled)
        );
        assert_eq!(
            breaker.opening_for(9, &Error::Auth("401".into())),
            Some(breaker.settled),
            "and it does not grow: it was already settled the first time"
        );
    }

    #[test]
    fn a_rate_limit_is_honoured_exactly_rather_than_guessed_at() {
        // The provider is already telling us when it clears. A local backoff would either
        // re-ask too early and earn another limit, or too late and waste time it was told
        // about. Longer than `longest` on purpose: that is this crate's guess, and this is
        // not a guess.
        let breaker = Breaker::default();
        let told = Error::RateLimited {
            retry_after: Some(Duration::from_secs(90)),
        };
        assert_eq!(breaker.opening_for(1, &told), Some(Duration::from_secs(90)));
        assert!(Duration::from_secs(90) > breaker.longest);

        let untold = Error::RateLimited { retry_after: None };
        assert_eq!(breaker.opening_for(1, &untold), Some(breaker.first));
    }

    #[test]
    fn the_backoff_doubles_and_then_stops() {
        let breaker = Breaker::default();
        let down = Error::Transient("503".into());
        let wait = |n| breaker.opening_for(n, &down);

        assert_eq!(wait(1), Some(Duration::from_secs(1)));
        assert_eq!(wait(2), Some(Duration::from_secs(2)));
        assert_eq!(wait(4), Some(Duration::from_secs(8)));
        assert_eq!(
            wait(1_000_000),
            Some(breaker.longest),
            "a provider down for an hour is still re-checked every minute"
        );
    }

    #[test]
    fn a_route_that_answers_forgets_everything_it_was_holding() {
        // Not just the timer. Leaving the count would make the next failure back off as
        // though the run of successes in between had not happened.
        let since = Instant::now();
        let health = Health::default();
        let breaker = Breaker::default();

        health.failed(&breaker, &Error::Transient("503".into()), since);
        health.failed(&breaker, &Error::Transient("503".into()), since);
        assert_eq!(health.failures(), 2);
        assert!(health.closed_for(since).is_some());

        health.served();
        assert_eq!(health.failures(), 0);
        assert_eq!(health.closed_for(since), None);
    }

    #[test]
    fn a_failure_that_does_not_open_the_circuit_still_leaves_the_route_usable() {
        // A refusal counts as a failure, because it is one, and it must not close anything.
        let since = Instant::now();
        let health = Health::default();

        assert_eq!(
            health.failed(
                &Breaker::default(),
                &Error::Refused { category: None },
                since
            ),
            None
        );
        assert_eq!(health.closed_for(since), None, "still selectable");
    }

    #[test]
    fn a_closed_circuit_opens_again_on_its_own() {
        let since = Instant::now();
        let health = Health::default();
        health.close_for(Duration::from_millis(0), since);

        // Zero is the smallest wait, and it has already passed. The point is that nothing
        // has to call anything to reopen a circuit: time does it.
        assert_eq!(health.closed_for(since), None);
    }
}
