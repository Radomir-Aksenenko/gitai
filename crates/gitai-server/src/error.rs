//! HTTP error mapping.
//!
//! A webhook endpoint is public, so the status code it returns is information
//! an attacker can read. A bad signature and an unknown forge both come back as
//! flat refusals with nothing about which secret or which repository was
//! involved; the detail goes to the log instead.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use gitai_core::error::Error;
use serde_json::json;

pub struct ApiError {
    status: StatusCode,
    /// Safe to return over HTTP.
    public: String,
}

impl ApiError {
    pub fn new(status: StatusCode, public: impl Into<String>) -> Self {
        Self {
            status,
            public: public.into(),
        }
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, what)
    }

    pub fn bad_request(what: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, what)
    }

    /// Logs the real reason and returns nothing useful to the caller.
    pub fn unauthorized(detail: impl std::fmt::Display) -> Self {
        tracing::warn!(reason = %detail, "rejected a webhook delivery");
        Self::new(StatusCode::UNAUTHORIZED, "signature verification failed")
    }

    pub fn internal(detail: impl std::fmt::Display) -> Self {
        tracing::error!(error = %detail, "request failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        match e {
            Error::NotFound(what) => Self::not_found(what),
            Error::Config(msg) => Self::new(StatusCode::BAD_REQUEST, msg),
            other => Self::internal(other),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.public }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_task_becomes_404() {
        let e: ApiError = Error::NotFound("task 1".into()).into();
        assert_eq!(e.status(), StatusCode::NOT_FOUND);
        assert_eq!(e.public, "task 1");
    }

    #[test]
    fn internal_failures_do_not_leak_their_detail() {
        let e: ApiError = Error::store("connection to /var/lib/gitai.db refused").into();
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.public, "internal error");
        assert!(!e.public.contains("gitai.db"));
    }

    #[test]
    fn a_bad_signature_says_nothing_about_which_secret_failed() {
        let e = ApiError::unauthorized(Error::forge("gitea", "webhook signature mismatch"));
        assert_eq!(e.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(e.public, "signature verification failed");
        assert!(!e.public.contains("gitea"));
    }
}
