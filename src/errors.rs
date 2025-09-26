//! Error handling and response structures
//!
//! This module provides standardized error responses that are compatible with
//! OpenAI's API format, ensuring consistent error handling across the proxy.

use axum::{
    Json,
    response::{IntoResponse, Response},
};
use bon::Builder;
use hyper::StatusCode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponseBody {
    pub message: String,
    pub r#type: String,
    pub param: Option<String>,
    pub code: String,
}

#[derive(Debug, Clone, Builder)]
pub struct OnwardsErrorResponse {
    pub body: Option<ErrorResponseBody>,
    pub status: StatusCode,
}

impl OnwardsErrorResponse {
    pub fn model_not_found(model: &str) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: format!(
                    "The model `{model}` does not exist or you do not have access to it."
                ),
                r#type: "invalid_request_error".to_string(),
                param: None,
                code: "model_not_found".to_string(),
            }),
            status: StatusCode::NOT_FOUND,
        }
    }

    pub fn rate_limited() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "You are sending requests too quickly. Please slow down.".to_string(),
                r#type: "rate_limit_error".to_string(),
                param: None,
                code: "rate_limit".to_string(),
            }),
            status: StatusCode::TOO_MANY_REQUESTS,
        }
    }

    pub fn internal() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "An internal error occurred. Please try again later.".to_string(),
                r#type: "internal_error".to_string(),
                param: None,
                code: "internal_error".to_string(),
            }),
            status: StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn bad_gateway() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "An internal error occurred. Please try again later.".to_string(),
                r#type: "internal_error".to_string(),
                param: None,
                code: "internal_error".to_string(),
            }),
            status: StatusCode::BAD_GATEWAY,
        }
    }

    pub fn unprocessable_request(message: &str, param: Option<&str>) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: message.to_owned(),
                r#type: "invalid_request_error".to_string(),
                param: param.map(|s| s.to_string()),
                code: "unprocessable_request".to_string(),
            }),
            status: StatusCode::UNPROCESSABLE_ENTITY,
        }
    }

    pub fn bad_request(message: &str, param: Option<&str>) -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: message.to_owned(),
                r#type: "invalid_request_error".to_string(),
                param: param.map(|s| s.to_string()),
                code: "bad_request".to_string(),
            }),
            status: StatusCode::BAD_REQUEST,
        }
    }

    pub fn forbidden() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "Unauthorized".to_string(),
                r#type: "invalid_request_error".to_string(),
                param: None,
                code: "unauthorized".to_string(),
            }),
            status: StatusCode::FORBIDDEN,
        }
    }

    pub fn unauthorized() -> Self {
        OnwardsErrorResponse {
            body: Some(ErrorResponseBody {
                message: "Please supply an authentication token to access this resource"
                    .to_string(),
                r#type: "invalid_request_error".to_string(),
                param: None,
                code: "unauthenticated".to_string(),
            }),
            status: StatusCode::UNAUTHORIZED,
        }
    }
}

impl IntoResponse for OnwardsErrorResponse {
    fn into_response(self) -> Response {
        match self.body {
            Some(body) => (self.status, Json(body)).into_response(),
            None => self.status.into_response(), // No body, just status
        }
    }
}
