// (c) Cartesi and individual authors (see AUTHORS)
// SPDX-License-Identifier: Apache-2.0 (see LICENSE)

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

use crate::inclusion_lane::SequencerError;

#[derive(Debug, Error, Clone)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    InvalidSignature(String),
    #[error("{0}")]
    ExecutionRejected(String),
    #[error("{0}")]
    InternalError(String),
    #[error("{0}")]
    Overloaded(String),
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    ok: bool,
    code: &'static str,
    message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn invalid_signature(message: impl Into<String>) -> Self {
        Self::InvalidSignature(message.into())
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::InternalError(message.into())
    }

    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::Overloaded(message.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) | Self::InvalidSignature(_) => StatusCode::BAD_REQUEST,
            Self::ExecutionRejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Overloaded(_) => StatusCode::TOO_MANY_REQUESTS,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::InvalidSignature(_) => "INVALID_SIGNATURE",
            Self::ExecutionRejected(_) => "EXECUTION_REJECTED",
            Self::InternalError(_) => "INTERNAL_ERROR",
            Self::Overloaded(_) => "OVERLOADED",
        }
    }
}

impl From<SequencerError> for ApiError {
    fn from(value: SequencerError) -> Self {
        match value {
            SequencerError::Invalid(message) => Self::ExecutionRejected(message),
            SequencerError::Internal(message) => Self::InternalError(message),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            ok: false,
            code: self.code(),
            message: self.to_string(),
        };
        (self.status(), Json(body)).into_response()
    }
}
