/// Axum handlers for the proxy server
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
use tracing::{debug, error, info, instrument};

const ONWARD_MODEL_HEADER: &str = "onwards-model";

const MODEL_OVERRIDE_HEADER: &str = "model-override";

/// The main handler responsible for forwarding requests to targets
/// TODO(fergus): Better error messages beyond raw status codes.
#[instrument(skip(state, req))]
pub async fn target_message_handler<T: HttpClient, C>(
    State(state): State<AppState<T, C>>,
    mut req: axum::extract::Request,
) -> Result<Response, StatusCode>
where
    C: governor::clock::Clock + Clone,
{
    // Extract the request body. TODO(fergus): make this step conditional: its not necessary if we
    // extract the model from the header.
    let mut body_bytes =
        match axum::body::to_bytes(std::mem::take(req.body_mut()), usize::MAX).await {
            Ok(bytes) => bytes,
            Err(_) => return Err(StatusCode::BAD_REQUEST),
        };

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
            match serde_json::from_slice(&body_bytes) {
                Ok(model) => model,
                Err(_) => return Err(StatusCode::BAD_REQUEST),
            }
        }
    };

    info!("Received request for model: {}", model.model);

    let target = match state.targets.targets.get(model.model) {
        Some(target) => target,
        None => return Err(StatusCode::NOT_FOUND),
    };

    // Check rate limit if configured for this target
    if let Some(rate_limiter) = state.targets.rate_limiters.get(model.model) {
        if rate_limiter.check().is_err() {
            debug!("Rate limit exceeded for model: {}", model.model);
            return Err(StatusCode::TOO_MANY_REQUESTS);
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
        && !body_bytes.is_empty()
    {
        debug!("Rewriting model key to: {}", rewrite);
        let mut body_serialized: serde_json::Value = match serde_json::from_slice(&body_bytes) {
            Ok(value) => value,
            Err(_) => return Err(StatusCode::BAD_REQUEST),
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

    *req.body_mut() = axum::body::Body::from(body_bytes);

    // Build the onwards URI
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(req.uri().path());
    let upstream_uri = target
        .url
        .join(path_and_query.strip_prefix('/').unwrap_or(path_and_query))
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .to_string();
    let upstream_uri_parsed = match Uri::try_from(&upstream_uri) {
        Ok(uri) => uri,
        Err(_) => {
            error!("Invalid URI: {}", upstream_uri);
            return Err(StatusCode::BAD_REQUEST);
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

    if let Some(key) = &target.key {
        debug!("Adding authorization header for {}", target.url);
        req.headers_mut()
            .insert("Authorization", format!("Bearer {key}").parse().unwrap());
    } else {
        debug!("No key configured for target {}", target.url);
    }

    // forward the request to the target, returning the response as-is
    match state.http_client.request(req).await {
        Ok(response) => Ok(response),
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
pub async fn models<T: HttpClient, C>(State(state): State<AppState<T, C>>) -> impl IntoResponse
where
    C: governor::clock::Clock + Clone,
{
    Json(ListModelResponse::from_targets(&state.targets))
}
