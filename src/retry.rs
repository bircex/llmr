//! Trying again, when trying again is the right thing to do.
//!
//! This crate already knows which failures are worth repeating
//! ([`Error::is_retryable`]) and what the provider asked you to wait
//! ([`Error::retry_after`]). A caller reconstructing either from a message will get it
//! wrong, so the policy lives here and [`crate::Router`] applies it.
//!
//! # What stays yours
//!
//! **Whether a request is safe to repeat.** That is a question about your request, not about
//! the failure, and nothing here can answer it. A [`Error::Timeout`] is retryable and may
//! still leave you paying for two answers, which is why timeouts are **not** repeated unless
//! you say so with [`Retry::repeating_timeouts`].
//!
//! Nothing retries by default. A router with no policy behaves exactly as it did before this
//! module existed.
//!
//! ```no_run
//! use llmr::retry::Retry;
//! use std::time::Duration;
//!
//! # #[cfg(feature = "retry")]
//! # fn example(router: llmr::Router) -> llmr::Router {
//! router.retrying(Retry::new(3).with_base(Duration::from_millis(200)))
//! # }
//! ```

use crate::error::Error;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// Something that can wait.
///
/// A trait for the same reason [`crate::HttpTransport`] is one: waiting is the part that
/// needs a runtime, and this crate does not choose yours. It also makes the policy testable
/// without a clock — the tests here record what they were asked to wait rather than waiting.
#[async_trait]
pub trait Delay: Send + Sync {
    /// Waits, roughly this long.
    async fn sleep(&self, how_long: Duration);
}

/// A [`Delay`] backed by `tokio`.
#[cfg(feature = "retry")]
#[cfg_attr(docsrs, doc(cfg(feature = "retry")))]
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioDelay;

#[cfg(feature = "retry")]
#[async_trait]
impl Delay for TokioDelay {
    async fn sleep(&self, how_long: Duration) {
        tokio::time::sleep(how_long).await;
    }
}

/// When to try again, and how long to wait first.
///
/// Immutable once built, like everything else here, so one policy can be shared by any
/// number of concurrent calls.
#[derive(Clone)]
pub struct Retry {
    attempts: u32,
    base: Duration,
    ceiling: Duration,
    repeat_timeouts: bool,
    delay: Arc<dyn Delay>,
}

impl std::fmt::Debug for Retry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Retry")
            .field("attempts", &self.attempts)
            .field("base", &self.base)
            .field("ceiling", &self.ceiling)
            .field("repeat_timeouts", &self.repeat_timeouts)
            .finish_non_exhaustive()
    }
}

impl Retry {
    /// At most this many attempts in total, counting the first.
    ///
    /// `Retry::new(1)` never retries, which is the same as no policy at all. Zero is read as
    /// one: a call that was never made is not a policy anybody meant to write.
    #[cfg(feature = "retry")]
    #[cfg_attr(docsrs, doc(cfg(feature = "retry")))]
    pub fn new(attempts: u32) -> Self {
        Self::with_delay(attempts, Arc::new(TokioDelay))
    }

    /// The same, waiting through something you supply.
    ///
    /// For a runtime that is not `tokio`, or for a test that would rather not wait.
    pub fn with_delay(attempts: u32, delay: Arc<dyn Delay>) -> Self {
        Self {
            attempts: attempts.max(1),
            base: Duration::from_millis(250),
            ceiling: Duration::from_secs(30),
            repeat_timeouts: false,
            delay,
        }
    }

    /// The first wait, doubled on each attempt after that.
    #[must_use]
    pub fn with_base(mut self, base: Duration) -> Self {
        self.base = base;
        self
    }

    /// The longest this will ever wait on its own.
    ///
    /// It does not cap a wait the **provider** asked for. Shortening that would turn a rate
    /// limit into a longer one, which is the whole reason `Retry-After` is honoured exactly.
    #[must_use]
    pub fn with_ceiling(mut self, ceiling: Duration) -> Self {
        self.ceiling = ceiling;
        self
    }

    /// Repeat a call that timed out.
    ///
    /// Off by default, and this is the one setting worth thinking about. A timeout means the
    /// deadline passed, not that the work stopped: the provider may still be generating, and
    /// a second attempt can leave you billed for two answers to one question.
    ///
    /// Turn it on when your request is cheap, or idempotent on the provider's side, or when
    /// a missing answer costs more than a duplicated one. That is your call and it cannot be
    /// made here.
    #[must_use]
    pub fn repeating_timeouts(mut self) -> Self {
        self.repeat_timeouts = true;
        self
    }

    /// How many attempts this policy allows in total.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// How long to wait before attempt number `next`, or `None` to stop.
    ///
    /// `next` counts from one, so the wait before the second attempt is `wait_before(2, e)`.
    ///
    /// Four failures are never repeated, because each returns the same answer the second
    /// time: [`Error::Auth`], [`Error::InvalidRequest`], [`Error::Refused`] and
    /// [`Error::Unreadable`]. [`Error::is_retryable`] already says so; this asks it rather
    /// than keeping a second list that could disagree.
    pub fn wait_before(&self, next: u32, failure: &Error) -> Option<Duration> {
        if next > self.attempts || !failure.is_retryable() {
            return None;
        }
        if matches!(failure, Error::Timeout { .. }) && !self.repeat_timeouts {
            return None;
        }

        // Exactly what the provider said, uncapped and without jitter. It is telling you
        // when the limit clears, and any local number that fires sooner earns another 429.
        if let Some(asked) = failure.retry_after() {
            return Some(asked);
        }

        Some(self.backoff(next))
    }

    /// The wait this policy computes on its own, doubling and then jittered.
    ///
    /// Equal jitter: half the computed wait, plus a random share of the other half. Two
    /// callers that failed at the same moment must not come back at the same moment, which
    /// is how a provider recovering from a fault is knocked over again by everyone retrying
    /// in step.
    fn backoff(&self, next: u32) -> Duration {
        let doubled = self
            .base
            .saturating_mul(1u32 << (next.saturating_sub(2)).min(16));
        let capped = doubled.min(self.ceiling);

        let half = capped / 2;
        half + Duration::from_nanos(scatter(half.as_nanos() as u64))
    }

    /// Waits, through whatever the caller supplied.
    pub(crate) async fn sleep(&self, how_long: Duration) {
        self.delay.sleep(how_long).await;
    }
}

/// A number in `0..=span`, cheaply and without a random number generator.
///
/// Jitter needs to be unpredictable, not unbiased and not unguessable. Taking a dependency
/// on `rand` to spread retries apart would cost more crates than the whole OpenAI protocol,
/// and nothing here is a secret: the clock's nanoseconds, stirred, are enough to stop two
/// processes coming back in step.
fn scatter(span: u64) -> u64 {
    if span == 0 {
        return 0;
    }
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs().wrapping_mul(0x9E37_79B9_7F4A_7C15)))
        .unwrap_or(0x2545_F491_4F6C_DD1D);
    // xorshift64, so consecutive nanoseconds do not produce consecutive waits.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x % (span + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what it was asked to wait and returns at once.
    #[derive(Default)]
    struct Recording(Mutex<Vec<Duration>>);

    #[async_trait]
    impl Delay for Recording {
        async fn sleep(&self, how_long: Duration) {
            if let Ok(mut seen) = self.0.lock() {
                seen.push(how_long);
            }
        }
    }

    fn policy(attempts: u32) -> Retry {
        Retry::with_delay(attempts, Arc::new(Recording::default()))
    }

    #[test]
    fn the_wait_a_provider_named_is_used_exactly() {
        // Not doubled, not jittered, not capped. The provider is saying when the limit
        // clears, and a local timer that fires sooner earns another 429.
        let named = Error::RateLimited {
            retry_after: Some(Duration::from_secs(90)),
        };
        let policy = policy(5).with_ceiling(Duration::from_secs(2));
        assert_eq!(policy.wait_before(2, &named), Some(Duration::from_secs(90)));
    }

    #[test]
    fn the_four_settled_failures_are_never_repeated() {
        // Each returns the same answer the second time.
        let policy = policy(5);
        for settled in [
            Error::Auth("rejected".into()),
            Error::InvalidRequest("malformed".into()),
            Error::Refused { category: None },
            Error::Unreadable("no content".into()),
        ] {
            assert_eq!(
                policy.wait_before(2, &settled),
                None,
                "{settled} must never be retried"
            );
        }
    }

    #[test]
    fn a_timeout_is_left_alone_unless_you_ask_for_it() {
        // The deadline passed; the work may not have. Repeating it can buy two answers to
        // one question, and only the caller knows whether that is acceptable.
        let timed_out = Error::Timeout {
            elapsed: Duration::from_secs(60),
        };
        assert_eq!(policy(5).wait_before(2, &timed_out), None);
        assert!(policy(5)
            .repeating_timeouts()
            .wait_before(2, &timed_out)
            .is_some());
    }

    #[test]
    fn attempts_run_out() {
        let policy = policy(3);
        let transient = Error::Transient("503".into());
        assert!(policy.wait_before(2, &transient).is_some());
        assert!(policy.wait_before(3, &transient).is_some());
        assert_eq!(policy.wait_before(4, &transient), None, "three means three");
    }

    #[test]
    fn one_attempt_is_a_policy_that_never_retries() {
        assert_eq!(
            policy(1).wait_before(2, &Error::Transient("503".into())),
            None
        );
        // And zero is read as one rather than as a call that never happens.
        assert_eq!(policy(0).attempts(), 1);
    }

    #[test]
    fn the_computed_wait_doubles_and_then_stops_at_the_ceiling() {
        let policy = policy(9)
            .with_base(Duration::from_millis(100))
            .with_ceiling(Duration::from_millis(800));
        let transient = Error::Transient("503".into());

        let wait = |n| {
            policy
                .wait_before(n, &transient)
                .unwrap_or_default()
                .as_millis()
        };

        // Equal jitter: never below half the computed wait, never above it.
        assert!((50..=100).contains(&wait(2)), "{}", wait(2));
        assert!((100..=200).contains(&wait(3)), "{}", wait(3));
        assert!((200..=400).contains(&wait(4)), "{}", wait(4));
        for n in 6..9 {
            assert!((400..=800).contains(&wait(n)), "attempt {n}: {}", wait(n));
        }
    }

    #[test]
    fn jitter_does_not_return_the_same_number_every_time() {
        // Two callers that failed together must not come back together. A fixed backoff is
        // how a provider recovering from a fault gets knocked over by everyone at once.
        let policy = policy(9).with_base(Duration::from_secs(4));
        let transient = Error::Transient("503".into());
        let seen: std::collections::HashSet<_> =
            (0..64).map(|_| policy.wait_before(2, &transient)).collect();
        assert!(seen.len() > 1, "every wait was identical: {seen:?}");
    }
}
