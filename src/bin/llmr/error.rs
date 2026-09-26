//! Failures, in the envelope OpenAI clients already know how to read.
//!
//! The status code is chosen so a client's own retry logic does the right thing without
//! reading the message: a 429 is retried after the wait it names, a 400 is not retried at
//! all, and a 502 says the fault is behind the gateway rather than in the request.

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::time::Duration;

/// A failure, ready to be written to a client.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub code: &'static str,
    pub param: Option<String>,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl ApiError {
    fn new(status: StatusCode, kind: &'static str, code: &'static str, message: String) -> Self {
        Self {
            status,
            kind,
            code,
            param: None,
            message,
            retry_after: None,
        }
    }

    /// The request is malformed.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_request",
            message.into(),
        )
    }

    /// One field of the request is malformed or cannot be honoured.
    pub fn invalid_param(param: &str, message: impl Into<String>) -> Self {
        let mut error = Self::invalid(message);
        error.param = Some(param.to_string());
        error
    }

    /// No such model name.
    pub fn model_not_found(model: &str) -> Self {
        let mut error = Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            format!("the model {model:?} is not served by this gateway. GET /v1/models lists the names it serves"),
        );
        error.param = Some("model".into());
        error
    }

    /// A model that exists and is switched off.
    pub fn model_not_enabled(model: &str, why: String) -> Self {
        let mut error = Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_enabled",
            format!("the model {model:?} is not enabled: {why}"),
        );
        error.param = Some("model".into());
        error
    }

    /// No such thing, on the management API.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "not_found",
            message.into(),
        )
    }

    /// It exists already.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "invalid_request_error",
            "conflict",
            message.into(),
        )
    }

    /// The caller presented no key, or a wrong one.
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid_api_key",
            "a valid token is required, as `Authorization: Bearer <token>` or `x-api-key`".into(),
        )
    }

    /// Something inside the gateway broke.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "internal_error",
            message.into(),
        )
    }

    /// The body written for this failure.
    pub fn body(&self) -> serde_json::Value {
        json!({
            "error": {
                "message": self.message,
                "type": self.kind,
                "param": self.param,
                "code": self.code,
            }
        })
    }
}

impl From<llmr::Error> for ApiError {
    fn from(error: llmr::Error) -> Self {
        ApiError::from(&error)
    }
}

impl From<&llmr::Error> for ApiError {
    /// What a router failure means to the client that asked.
    ///
    /// The split follows whose problem it is. A request the providers will not accept is
    /// the client's (4xx). A key the vendor rejected, a model the vendor does not have and a
    /// reply nobody could read are the gateway operator's, and a client cannot fix them, so
    /// they are a 502 rather than a 401 or a 404 that would send somebody checking their own
    /// key.
    fn from(error: &llmr::Error) -> Self {
        use llmr::Error as E;
        let message = error.to_string();
        match error {
            E::Unsupported(_) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "no_route",
                message,
            ),
            E::InvalidRequest(_) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "upstream_rejected_request",
                message,
            ),
            E::RateLimited { retry_after } => {
                let mut error = Self::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limit_error",
                    "rate_limited",
                    message,
                );
                error.retry_after = *retry_after;
                error
            }
            E::OverBudget(_) => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "insufficient_quota",
                "over_budget",
                message,
            ),
            E::Timeout { .. } => Self::new(
                StatusCode::GATEWAY_TIMEOUT,
                "server_error",
                "timeout",
                message,
            ),
            E::Transient(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "upstream_unavailable",
                message,
            ),
            E::Auth(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "upstream_credential_rejected",
                message,
            ),
            E::NotFound(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "upstream_not_found",
                message,
            ),
            E::Refused { .. } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request_error",
                "refused",
                message,
            ),
            _ => Self::new(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "upstream_unreadable",
                message,
            ),
        }
    }
}

impl From<crate::store::StoreError> for ApiError {
    fn from(error: crate::store::StoreError) -> Self {
        use crate::store::StoreError as E;
        match error {
            E::NotFound(m) => ApiError::not_found(m),
            E::Conflict(m) => ApiError::conflict(m),
            E::Failed(m) => ApiError::internal(m),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, axum::Json(self.body())).into_response();
        if let Some(wait) = self.retry_after {
            // Rounded up: a client told to wait zero seconds for a wait of 400ms comes back
            // too early and earns another limit.
            let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert("retry-after", value);
            }
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vendor_rejecting_the_gateways_key_is_not_the_clients_401() {
        let error = ApiError::from(llmr::Error::Auth("bad key".into()));
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn a_rate_limit_carries_its_wait_rounded_up() {
        let response = ApiError::from(llmr::Error::RateLimited {
            retry_after: Some(Duration::from_millis(1500)),
        })
        .into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "2");
    }

    #[test]
    fn no_route_is_a_client_error_naming_itself() {
        let error = ApiError::from(llmr::Error::Unsupported("no route".into()));
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.body()["error"]["code"], "no_route");
    }
}
