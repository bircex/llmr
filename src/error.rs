//! What can go wrong, in the shapes a caller can act on.

use std::time::Duration;

/// The result type every provider call returns.
pub type Result<T> = std::result::Result<T, Error>;

/// Why a call did not produce an answer.
///
/// The variants are chosen so a caller can decide what to do without reading the message.
/// Retrying a bad credential earns a rate limit, and retrying a request the provider will
/// never accept just spends the budget twice, so those cases are separated from the ones
/// worth trying again.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The provider cannot do this at all. Ask [`crate::Provider::capabilities`] first.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The credential was rejected. Never retry this one.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The model, endpoint or resource does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// The request was malformed, or the provider refused its shape.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Too many requests.
    ///
    /// Honour `retry_after` when it is present. A backoff you guessed locally only makes
    /// the limit worse, because the provider is already telling you when it will clear.
    #[error("rate limited{}", match .retry_after {
        Some(d) => format!(", retry after {}ms", d.as_millis()),
        None => String::new(),
    })]
    RateLimited {
        /// How long the provider asked you to wait, when it said.
        retry_after: Option<Duration>,
    },

    /// A deadline passed.
    ///
    /// Different from [`Error::Transient`] on purpose. The work may still be running on the
    /// provider's side, so a retry can produce a second answer you are billed for.
    #[error("timed out after {}ms", .elapsed.as_millis())]
    Timeout {
        /// How long the call ran before it was given up on.
        elapsed: Duration,
    },

    /// The model declined to answer.
    ///
    /// Surfaced rather than retried. A refusal is an answer, and asking again in the hope
    /// of a different one wastes a call and usually gets the same reply.
    #[error("the model declined{}", match .category {
        Some(c) => format!(": {c}"),
        None => String::new(),
    })]
    Refused {
        /// What the provider said it was declining, when it said.
        category: Option<String>,
    },

    /// Something went wrong that is worth trying again.
    #[error("transient: {0}")]
    Transient(String),

    /// The provider answered, and the answer could not be read.
    ///
    /// A successful status code with a body this crate cannot parse is a failure. Treating
    /// it as an empty answer is how a run continues on nothing.
    #[error("the provider replied and the reply could not be read: {0}")]
    Unreadable(String),
}

impl Error {
    /// Whether trying the same call again is reasonable.
    ///
    /// This is a hint, not a promise. It says the failure was not the caller's fault and
    /// not permanent. It does not say the call is safe to repeat, which is a question about
    /// your request rather than about the error. A [`Error::Timeout`] is retryable and may
    /// still leave you paying for two answers.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::RateLimited { .. } | Error::Timeout { .. } | Error::Transient(_)
        )
    }

    /// How long the provider asked you to wait, when it said.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_credential_is_never_retried() {
        assert!(!Error::Auth("no key".into()).is_retryable());
    }

    #[test]
    fn a_refusal_is_an_answer_rather_than_a_failure_to_retry() {
        assert!(!Error::Refused { category: None }.is_retryable());
    }

    #[test]
    fn an_unreadable_reply_is_not_retried_either() {
        // The provider answered. Asking again gets the same body it could not parse.
        assert!(!Error::Unreadable("missing content".into()).is_retryable());
    }

    #[test]
    fn a_rate_limit_carries_the_wait_the_provider_asked_for() {
        let limited = Error::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        assert!(limited.is_retryable());
        assert_eq!(limited.retry_after(), Some(Duration::from_secs(30)));
        assert!(limited.to_string().contains("30000ms"));
    }
}
