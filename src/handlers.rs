//! HTTP request handlers for the proxy server
//!
//! This module contains the main Axum handlers that process incoming requests,
//! route them to appropriate targets, and handle authentication and rate limiting.

use crate::AppState;
use crate::auth;
use crate::client::HttpClient;
use crate::errors::OnwardsErrorResponse;
use crate::models::ListModelResponse;
use axum::{
    Json,
    extract::Request,
    extract::State,
    http::{
        Uri,
        header::{CONTENT_LENGTH, TRANSFER_ENCODING},
    },
    response::{IntoResponse, Response},
};
use serde_json::map::Entry;
use tracing::{debug, error, instrument, trace};

/// The main handler responsible for forwarding requests to targets
/// TODO(fergus): Better error messages beyond raw status codes.
#[instrument(skip(state, req))]
pub async fn target_message_handler<T: HttpClient>(
    State(state): State<AppState<T>>,
    mut req: axum::extract::Request,
) -> Result<Response, OnwardsErrorResponse> {
    // Extract the request body. TODO(fergus): make this step conditional: its not necessary if we
    // extract the model from the header.
    let mut body_bytes =
        match axum::body::to_bytes(std::mem::take(req.body_mut()), usize::MAX).await {
            Ok(bytes) => bytes,
            Err(_) => return Err(OnwardsErrorResponse::internal()), // since there's no body limit,
                                                                    // this should never fail.
        };

    // Apply body transformation if provided
    if let Some(ref transform_fn) = state.body_transform_fn {
        let path = req.uri().path();
        if let Some(transformed_body) = transform_fn(path, req.headers(), &body_bytes) {
            debug!("Applied body transformation for path: {}", path);
            body_bytes = transformed_body;
        }
    }

    // Log full incoming request details for debugging
    trace!(
        "Incoming request details:\n  Method: {}\n  URI: {}\n  Headers: {:?}\n  Body: {}",
        req.method(),
        req.uri(),
        req.headers(),
        String::from_utf8_lossy(&body_bytes)
    );

    // Extract the model using the shared function
    let model_name = match crate::extract_model_from_request(req.headers(), &body_bytes) {
        Ok(model) => model,
        Err(_) => {
            return Err(OnwardsErrorResponse::bad_request(
                "Could not parse onwards model from request. 'model' parameter must be supplied in either the body or in the Model-Override header.",
                Some("model"),
            ));
        }
    };

    trace!("Received request for model: {}", model_name);
    trace!(
        "Available targets: {:?}",
        state
            .targets
            .targets
            .iter()
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>()
    );

    let target = match state.targets.targets.get(&model_name) {
        Some(target) => {
            debug!("Found target for model '{}': {:?}", model_name, target.url);
            target
        }
        None => {
            debug!("No target found for model: {}", model_name);
            return Err(OnwardsErrorResponse::model_not_found(model_name.as_str()));
        }
    };

    if let Some(ref limiter) = target.limiter {
        if limiter.check().is_err() {
            return Err(OnwardsErrorResponse::rate_limited());
        }
    }

    // Extract bearer token for authentication and rate limiting
    let bearer_token = req
        .headers()
        .get("authorization")
        .and_then(|auth_header| auth_header.to_str().ok())
        .and_then(|auth_value| auth_value.strip_prefix("Bearer "));

    // Validate API key if target has keys configured
    if let Some(ref keys) = target.keys {
        match bearer_token {
            Some(token) => {
                trace!("Validating bearer token");
                if auth::validate_bearer_token(keys, token) {
                    debug!("Bearer token validation successful");
                } else {
                    debug!("Bearer token validation failed - token not in key set");
                    return Err(OnwardsErrorResponse::forbidden());
                }
            }
            None => {
                debug!("No bearer token found in authorization header");
                return Err(OnwardsErrorResponse::unauthorized());
            }
        }
    } else {
        debug!(
            "Target '{}' has no keys configured - allowing request",
            model_name
        );
    }

    // Check per-key rate limits if bearer token is present
    if let Some(token) = bearer_token {
        if let Some(limiter) = state.targets.key_rate_limiters.get(token) {
            if limiter.check().is_err() {
                debug!("Per-key rate limit exceeded for token: {}", token);
                return Err(OnwardsErrorResponse::rate_limited());
            }
        }
    }

    // Users can specify the onwards value of the model field in the target
    // config. If not supplied, its left as is.
    if let Some(rewrite) = target.onwards_model.clone()
        && !body_bytes.is_empty()
    {
        debug!("Rewriting model key to: {}", rewrite);
        let error = OnwardsErrorResponse::bad_request(
            "Could not parse onwards model from request. 'model' parameter must be supplied in either the body or in the Model-Override header.",
            Some("model"),
        );
        let mut body_serialized: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(value) => value,
            Err(_) => return Err(error.clone()),
        };
        let entry = body_serialized
            .as_object_mut()
            .ok_or(error.clone())? // if the body is not an object (we know its not empty), return 400
            .entry("model");
        match entry {
            Entry::Occupied(mut entry) => {
                // If the model key already exists, we overwrite it
                entry.insert(serde_json::Value::String(rewrite));
            }
            Entry::Vacant(_entry) => {
                // If the body didn't have a model key, then 400 (header shouldn't have been
                // provided)
                return Err(error.clone());
            }
        }
        body_bytes = match serde_json::to_vec(&body_serialized) {
            Ok(bytes) => axum::body::Bytes::from(bytes),
            Err(_) => return Err(OnwardsErrorResponse::internal()),
        };
    }

    // Build the onwards URI
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(req.uri().path());
    let upstream_uri = target
        .url
        .join(path_and_query.strip_prefix('/').unwrap_or(path_and_query))
        .map_err(|_| OnwardsErrorResponse::internal())?
        .to_string();
    let upstream_uri_parsed = match Uri::try_from(&upstream_uri) {
        Ok(uri) => uri,
        Err(_) => {
            error!("Invalid URI: {}", upstream_uri);
            return Err(OnwardsErrorResponse::internal());
        }
    };

    *req.uri_mut() = upstream_uri_parsed.clone();

    // Update the host header to match the target server (otherwise cloudflare gets mad).
    if let Some(host) = upstream_uri_parsed.host() {
        let host_value = if let Some(port) = upstream_uri_parsed.port_u16() {
            format!("{host}:{port}")
        } else {
            host.to_string()
        };
        req.headers_mut()
            .insert("host", host_value.parse().unwrap());
    }

    if let Some(key) = &target.onwards_key {
        debug!("Adding authorization header for {}", target.url);
        req.headers_mut()
            .insert("Authorization", format!("Bearer {key}").parse().unwrap());
    } else {
        debug!("No key configured for target {}", target.url);
    }

    // Always set Content-Length and remove Transfer-Encoding since we buffer the full body
    req.headers_mut().insert(
        CONTENT_LENGTH,
        body_bytes
            .len()
            .to_string()
            .parse()
            .expect("Content-Length should be valid"),
    );
    req.headers_mut().remove(TRANSFER_ENCODING);

    // Log full outgoing request details for debugging
    trace!(
        "Outgoing request details:\n  Method: {}\n  URI: {}\n  Headers: {:?}\n  Body: {}",
        req.method(),
        req.uri(),
        req.headers(),
        String::from_utf8_lossy(&body_bytes)
    );

    *req.body_mut() = axum::body::Body::from(body_bytes);

    // forward the request to the target, returning the response as-is
    match state.http_client.request(req).await {
        Ok(response) => Ok(response),
        Err(e) => {
            error!(
                "Error forwarding request to target url {}: {}",
                upstream_uri, e
            );
            Err(OnwardsErrorResponse::bad_gateway())
        }
    }
}

#[instrument(skip(state, req))]
pub async fn models<T: HttpClient>(
    State(state): State<AppState<T>>,
    req: Request,
) -> impl IntoResponse {
    // Extract bearer token from Authorization header
    let bearer_token = req
        .headers()
        .get("authorization")
        .and_then(|auth_header| auth_header.to_str().ok())
        .and_then(|auth_value| auth_value.strip_prefix("Bearer "));

    // Filter targets based on bearer token permissions
    let accessible_targets = state
        .targets
        .targets
        .iter()
        .filter(|entry| {
            let target = entry.value();

            // If target has no keys configured, it's publicly accessible
            if target.keys.is_none() {
                return true;
            }

            // If target has keys but no bearer token provided, deny access
            let Some(token) = bearer_token else {
                return false;
            };

            // Validate bearer token against target's keys
            auth::validate_bearer_token(target.keys.as_ref().unwrap(), token)
        })
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect::<std::collections::HashMap<_, _>>();

    // Create filtered response
    Json(ListModelResponse::from_filtered_targets(
        &accessible_targets,
    ))
}
