//! HTTP error responses. Git-protocol user errors (non-fast-forward, bad ref)
//! are *not* mapped here: they are reported as `unpack`/`ng` pkt-lines inside a
//! 200 response per the smart HTTP contract. Only transport/auth/routing errors
//! become HTTP error statuses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    Unauthorized,
    Forbidden,
    Conflict(String),
    PayloadTooLarge,
    UnsupportedMediaType(String),
    Store(Box<gitcask_store::StoreError>),
    Wal(Box<gitcask_wal::WalError>),
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden => StatusCode::FORBIDDEN,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ApiError::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiError::Store(error) => match error.as_ref() {
                gitcask_store::StoreError::Retryable(_) => StatusCode::SERVICE_UNAVAILABLE,
                gitcask_store::StoreError::NotFound { .. } => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            ApiError::Wal(error) => {
                if error.is_retryable() {
                    StatusCode::SERVICE_UNAVAILABLE
                } else if matches!(error.as_ref(), gitcask_wal::WalError::NotFound) {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn store_unavailable(&self) -> bool {
        self.status() == StatusCode::SERVICE_UNAVAILABLE
    }
}

impl ApiError {
    /// The plain-text body / SSE `error` packet message.
    pub fn message(&self) -> String {
        match self {
            ApiError::NotFound(m) => format!("not found: {m}"),
            ApiError::BadRequest(m) => format!("bad request: {m}"),
            ApiError::Unauthorized => "unauthorized".to_string(),
            ApiError::Forbidden => "forbidden".to_string(),
            ApiError::Conflict(m) => format!("conflict: {m}"),
            ApiError::PayloadTooLarge => "payload too large".to_string(),
            ApiError::UnsupportedMediaType(m) => format!("unsupported media type: {m}"),
            ApiError::Store(error) => match error.as_ref() {
                gitcask_store::StoreError::NotFound { key } => format!("not found: {key}"),
                _ => format!("internal error: {error}"),
            },
            ApiError::Wal(error) => match error.as_ref() {
                gitcask_wal::WalError::NotFound => "not found: repository not found".to_string(),
                _ => format!("internal error: wal: {error}"),
            },
            ApiError::Internal(m) => format!("internal error: {m}"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = self.message();
        let status = self.status();
        // Every 5xx is an operator-facing event: log it with its text (the
        // access log carries status only; a 500 whose reason lives solely in
        // the client's terminal is undebuggable).
        if status.is_server_error() {
            tracing::warn!(status = status.as_u16(), error = %msg, "request failed");
        }
        let mut resp = if self.store_unavailable() {
            (
                status,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"error":"store_unavailable","retryable":true}"#,
            )
                .into_response()
        } else {
            (status, msg).into_response()
        };
        // 503s are transient by contract (a store deadline or a warming copy):
        // say when to come back.
        if status == StatusCode::SERVICE_UNAVAILABLE {
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("15"),
            );
        }
        if status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Basic realm=\"gitcask\""),
            );
        }
        resp
    }
}

impl From<gitcask_store::StoreError> for ApiError {
    fn from(e: gitcask_store::StoreError) -> Self {
        ApiError::Store(Box::new(e))
    }
}

impl From<gitcask_wal::WalError> for ApiError {
    fn from(e: gitcask_wal::WalError) -> Self {
        ApiError::Wal(Box::new(e))
    }
}

impl From<crate::auth::AuthError> for ApiError {
    fn from(error: crate::auth::AuthError) -> Self {
        match error {
            crate::auth::AuthError::Unauthorized => ApiError::Unauthorized,
            crate::auth::AuthError::Forbidden => ApiError::Forbidden,
            crate::auth::AuthError::NotFound => ApiError::NotFound("repository".into()),
        }
    }
}

impl From<gitcask_git::GitError> for ApiError {
    fn from(error: gitcask_git::GitError) -> Self {
        let message = error.to_string();
        match error {
            gitcask_git::GitError::InvalidInput(_) | gitcask_git::GitError::Protocol(_) => {
                ApiError::BadRequest(message)
            }
            gitcask_git::GitError::RefConflict { .. } => ApiError::Conflict(message),
            _ => {
                tracing::error!(error = message, "git operation failed");
                ApiError::Internal(format!("git: {message}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_errors_keep_their_http_status() {
        assert_eq!(
            ApiError::from(crate::auth::AuthError::Unauthorized).status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::from(crate::auth::AuthError::Forbidden).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ApiError::from(crate::auth::AuthError::NotFound).status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn unauthorized_response_carries_the_git_basic_challenge() {
        let response = ApiError::Unauthorized.into_response();
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            Some("Basic realm=\"gitcask\"")
        );
    }

    #[test]
    fn git_errors_separate_client_input_from_server_failures() {
        assert_eq!(
            ApiError::from(gitcask_git::GitError::Protocol("bad pkt-line".into())).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::from(gitcask_git::GitError::Io(std::io::Error::other(
                "git missing"
            )))
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
