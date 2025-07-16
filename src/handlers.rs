/// Axum handlers for the proxy server
use crate::auth;
use crate::client::HttpClient;
use crate::models::ListModelResponse;
use crate::{AppState, models::ExtractedModel};
use axum::{
    Json,
    extract::State,
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::map::Entry;
use tracing::{debug, error, info, instrument, trace};

const ONWARD_MODEL_HEADER: &str = "onwards-model";

const MODEL_OVERRIDE_HEADER: &str = "model-override";

/// The main handler responsible for forwarding requests to targets
/// TODO(fergus): Better error messages beyond raw status codes.
#[instrument(skip(state, req))]
pub async fn target_message_handler<T: HttpClient>(
    State(state): State<AppState<T>>,
    mut req: axum::extract::Request,
) -> Result<Response, StatusCode> {
    info!("=== Incoming Request ===");
    info!("Method: {}, Path: {}", req.method(), req.uri().path());
    // Extract the request body. TODO(fergus): make this step conditional: its not necessary if we
    // extract the model from the header.
    let mut body_bytes =
        match axum::body::to_bytes(std::mem::take(req.body_mut()), usize::MAX).await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to extract request body: {}", e);
                return Err(StatusCode::BAD_REQUEST);
            }
        };

    // Log full incoming request details for debugging
    trace!(
        "Incoming request details:\n  Method: {}\n  URI: {}\n  Headers: {:?}\n  Body: {}",
        req.method(),
        req.uri(),
        req.headers(),
        String::from_utf8_lossy(&body_bytes)
    );

    // Order of precedence for the target to use:
    // 1. supplied as a header (model-override)
    // 2. Available in the request body as JSON
    // If neither is present, return a 400 Bad Request.
    let model = match req.headers().get(MODEL_OVERRIDE_HEADER) {
        Some(header_value) => {
            let model_str = match header_value.to_str() {
                Ok(value) => value,
                Err(_) => return Err(StatusCode::BAD_REQUEST),
            };
            debug!("Using model override from header: {}", model_str);
            ExtractedModel { model: model_str }
        }
        None => {
            debug!("Received request body of size: {}", body_bytes.len());
            debug!("Request body content: {}", String::from_utf8_lossy(&body_bytes));
            match serde_json::from_slice::<ExtractedModel>(&body_bytes) {
                Ok(model) => {
                    debug!("Successfully extracted model from body: {}", model.model);
                    model
                }
                Err(e) => {
                    error!("Failed to parse model from request body: {}", e);
                    error!("Body was: {}", String::from_utf8_lossy(&body_bytes));
                    return Err(StatusCode::BAD_REQUEST);
                }
            }
        }
    };

    info!("Received request for model: {}", model.model);

    let target = match state.targets.targets.get(model.model) {
        Some(target) => {
            debug!("Found target for model '{}': {:?}", model.model, target);
            target
        }
        None => {
            error!("No target found for model: '{}'", model.model);
            error!("Available targets: {:?}", 
                state.targets.targets.iter()
                    .map(|entry| entry.key().clone())
                    .collect::<Vec<_>>()
            );
            return Err(StatusCode::NOT_FOUND);
        }
    };

    // Validate API key if target has keys configured
    if let Some(ref keys) = target.keys {
        let bearer_token = req
            .headers()
            .get("authorization")
            .and_then(|auth_header| auth_header.to_str().ok())
            .and_then(|auth_value| auth_value.strip_prefix("Bearer "));

        match bearer_token {
            Some(token) if auth::validate_bearer_token(keys, token) => {} // Valid token, continue
            _ => return Err(StatusCode::UNAUTHORIZED),
        }
    }

    // Users can specify the onwards value of the model field via a header, or it can be specified in the target
    // config. If neither is supplied, its left as is.
    if let Some(rewrite) = req
        .headers()
        .get(ONWARD_MODEL_HEADER)
        .and_then(|x| x.to_str().ok())
        .map(|x| x.to_owned())
        .or(target.onwards_model.clone())
    {
        if !body_bytes.is_empty() {
        info!("Rewriting model in request body from '{}' to '{}'", model.model, rewrite);
        let mut body_serialized: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(value) => value,
            Err(e) => {
                error!("Failed to parse body for rewriting: {}", e);
                return Err(StatusCode::BAD_REQUEST);
            }
        };
        let entry = body_serialized
            .as_object_mut()
            .ok_or(StatusCode::BAD_REQUEST)? // if the body is not an object (we know its not empty), return 400
            .entry("model");
        match entry {
            Entry::Occupied(mut entry) => {
                // If the model key already exists, we overwrite it
                entry.insert(serde_json::Value::String(rewrite));
            }
            Entry::Vacant(_entry) => {
                // If the body didn't have a model key, then 400 (header shouldn't have been
                // provided)
                return Err(StatusCode::BAD_REQUEST);
            }
        }
        body_bytes = match serde_json::to_vec(&body_serialized) {
            Ok(bytes) => axum::body::Bytes::from(bytes),
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        };

        // Update Content-Length header to match the new body size
        req.headers_mut().insert(
            "content-length",
            body_bytes
                .len()
                .to_string()
                .parse()
                .expect("Content-Length should be valid"),
        );
        }
    }

    // Build the onwards URI
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(req.uri().path());
    info!("Building upstream URI - base: {}, path: {}", target.url, path_and_query);
    let upstream_uri = target
        .url
        .join(path_and_query.strip_prefix('/').unwrap_or(path_and_query))
        .map_err(|e| {
            error!("Failed to join URL: {} with path: {} - error: {}", target.url, path_and_query, e);
            StatusCode::BAD_REQUEST
        })?
        .to_string();
    info!("Upstream URI: {}", upstream_uri);
    let upstream_uri_parsed = match Uri::try_from(&upstream_uri) {
        Ok(uri) => uri,
        Err(e) => {
            error!("Invalid URI: {} - error: {}", upstream_uri, e);
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    *req.uri_mut() = upstream_uri_parsed.clone();
    info!("Set request URI to: {}", req.uri());

    // Update the host header to match the target server (otherwise cloudflare gets mad).
    if let Some(host) = upstream_uri_parsed.host() {
        let host_value = if let Some(port) = upstream_uri_parsed.port_u16() {
            format!("{host}:{port}")
        } else {
            host.to_string()
        };
        info!("Setting host header to: {}", host_value);
        req.headers_mut()
            .insert("host", host_value.parse().unwrap());
    }

    if let Some(key) = &target.onwards_key {
        info!("Adding authorization header for target");
        req.headers_mut()
            .insert("Authorization", format!("Bearer {key}").parse().unwrap());
    } else {
        info!("No authorization key configured for target {}", target.url);
    }

    // Log full outgoing request details for debugging
    trace!(
        "Outgoing request details:\n  Method: {}\n  URI: {}\n  Headers: {:?}\n  Body: {}",
        req.method(),
        req.uri(),
        req.headers(),
        String::from_utf8_lossy(&body_bytes)
    );

    *req.body_mut() = axum::body::Body::from(body_bytes);
    
    info!("Forwarding request to upstream URL: {}", upstream_uri);
    info!("Request method: {}, Headers count: {}", req.method(), req.headers().len());

    // forward the request to the target, returning the response as-is
    match state.http_client.request(req).await {
        Ok(response) => {
            info!("Successfully forwarded request to {}", upstream_uri);
            Ok(response)
        }
        Err(e) => {
            error!(
                "Error forwarding request to target url {}: {}",
                upstream_uri, e
            );
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

#[instrument(skip(state))]
pub async fn models<T: HttpClient>(State(state): State<AppState<T>>) -> impl IntoResponse {
    Json(ListModelResponse::from_targets(&state.targets))
}
