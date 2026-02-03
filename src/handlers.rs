//! HTTP request handlers for the proxy server
//!
//! This module contains the main Axum handlers that process incoming requests,
//! route them to appropriate targets, and handle authentication and rate limiting.

use crate::AppState;
use crate::auth;
use crate::client::HttpClient;
use crate::errors::OnwardsErrorResponse;
use crate::models::ListModelResponse;
use crate::sse::SseBufferedStream;
use crate::target::Target;
use axum::{
    Json,
    extract::Request,
    extract::State,
    http::{
        HeaderMap, HeaderName, HeaderValue, Uri,
        header::{CONTENT_LENGTH, TRANSFER_ENCODING},
    },
    response::{IntoResponse, Response},
};
use serde_json::map::Entry;
use tracing::{debug, error, instrument, trace};

/// Record HTTP response status code on the current span
fn record_response_status(status_code: u16) {
    tracing::Span::current().record("http.response.status_code", status_code);
}

/// Stores the original model name requested by the client
///
/// This is used to rewrite the model field in responses when `onwards_model` is configured
#[derive(Clone, Debug)]
struct OriginalModel(String);

/// Filters and modifies headers before forwarding to upstream
///
/// This function implements RFC 7230 compliant proxy behavior by:
/// - Removing hop-by-hop headers (connection, keep-alive, etc.)
/// - Stripping authentication headers to prevent credential leakage
/// - Removing browser-specific context headers (sec-*, origin, referer)
/// - Adding upstream authentication if configured
/// - Adding X-Forwarded-* headers for transparency
fn filter_headers_for_upstream(headers: &mut HeaderMap, target: &Target) {
    // Headers to remove: hop-by-hop (RFC 7230), auth, browser context, and routing headers
    const HEADERS_TO_STRIP: &[&str] = &[
        // RFC 7230 hop-by-hop headers (MUST remove per spec)
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "upgrade",
        // Authentication headers (prevent credential leakage to downstream)
        "authorization",
        "x-api-key",
        "api-key",
        // Browser security context headers (meaningless to upstream APIs)
        "sec-fetch-site",
        "sec-fetch-mode",
        "sec-fetch-dest",
        "sec-fetch-user",
        // Browser context leakage
        "origin",
        "referer",
        // Client state (security)
        "cookie",
        // HTTP caching (irrelevant since we buffer full responses)
        "if-modified-since",
        "if-none-match",
        "if-match",
        "if-unmodified-since",
        "if-range",
        // Our routing headers (already consumed)
        "model-override",
    ];

    for header in HEADERS_TO_STRIP {
        headers.remove(*header);
    }

    // Remove all sec-ch-ua* headers (Chrome User-Agent Client Hints)
    // These have many variants (sec-ch-ua, sec-ch-ua-mobile, sec-ch-ua-platform, etc.)
    let sec_ch_ua_headers: Vec<_> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("sec-ch-ua"))
        .cloned()
        .collect();
    for header in sec_ch_ua_headers {
        headers.remove(header);
    }

    // Add Authorization header if target requires authentication to upstream
    if let Some(key) = &target.onwards_key {
        let header_name_str = target
            .upstream_auth_header_name
            .as_deref()
            .unwrap_or("Authorization");
        let header_name = HeaderName::from_bytes(header_name_str.as_bytes()).unwrap();
        let prefix = target
            .upstream_auth_header_prefix
            .as_deref()
            .unwrap_or("Bearer ");
        let header_value = format!("{}{}", prefix, key);
        debug!(
            "Adding {} header for upstream {}: {}",
            header_name_str, target.url, header_value
        );
        headers.insert(header_name, header_value.parse().unwrap());
    } else {
        debug!(
            "No upstream authentication configured for target {}",
            target.url
        );
    }

    // Add X-Forwarded headers for transparency (preserve original request context)
    // Note: We don't have access to the original client IP in this handler,
    // so we only set X-Forwarded-Proto for now
    headers.insert("x-forwarded-proto", "https".parse().unwrap());
}

/// The main handler responsible for forwarding requests to targets
/// TODO(fergus): Better error messages beyond raw status codes.
#[instrument(skip(state, req), fields(
    gen_ai.request.model = tracing::field::Empty,
    http.response.status_code = tracing::field::Empty,
))]
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
        Some(model) => model,
        None => {
            record_response_status(400);
            return Err(OnwardsErrorResponse::bad_request(
                "Could not parse onwards model from request. 'model' parameter must be supplied in either the body or in the Model-Override header.",
                Some("model"),
            ));
        }
    };

    // Record model in span for trace correlation
    tracing::Span::current().record("gen_ai.request.model", &model_name);

    // Store original model in request extensions for response sanitization
    req.extensions_mut()
        .insert(OriginalModel(model_name.clone()));

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

    let pool = match state.targets.targets.get(&model_name) {
        Some(pool) => pool,
        None => {
            debug!("No target found for model: {}", model_name);
            record_response_status(404);
            return Err(OnwardsErrorResponse::model_not_found(model_name.as_str()));
        }
    };

    // Check if pool has no providers (e.g., composite model with no components)
    if pool.is_empty() {
        debug!("Pool for model '{}' has no providers", model_name);
        return Err(OnwardsErrorResponse::bad_gateway());
    }

    // Extract bearer token for authentication and rate limiting
    let bearer_token = req
        .headers()
        .get("authorization")
        .and_then(|auth_header| auth_header.to_str().ok())
        .and_then(|auth_value| auth_value.strip_prefix("Bearer "));

    // Validate API key using pool-level keys
    if let Some(keys) = pool.keys() {
        match bearer_token {
            Some(token) => {
                trace!("Validating bearer token against pool keys");
                if auth::validate_bearer_token(keys, token) {
                    debug!("Bearer token validation successful");
                } else {
                    debug!("Bearer token validation failed - token not in key set");
                    record_response_status(403);
                    return Err(OnwardsErrorResponse::forbidden());
                }
            }
            None => {
                debug!("No bearer token found in authorization header");
                record_response_status(401);
                return Err(OnwardsErrorResponse::unauthorized());
            }
        }
    } else {
        debug!(
            "Pool '{}' has no keys configured - allowing request",
            model_name
        );
    }

    // Check pool-level rate limit before selecting a provider
    if let Some(limiter) = pool.pool_limiter()
        && limiter.check().is_err()
    {
        debug!("Pool-level rate limit exceeded for model: {}", model_name);
        record_response_status(429);
        return Err(OnwardsErrorResponse::rate_limited());
    }

    // Check per-key rate limits if bearer token is present
    if let Some(token) = bearer_token
        && let Some(limiter) = state.targets.key_rate_limiters.get(token)
        && limiter.check().is_err()
    {
        debug!("Per-key rate limit exceeded for token: {}", token);
        record_response_status(429);
        return Err(OnwardsErrorResponse::rate_limited());
    }

    // Acquire pool-level concurrency permit
    let _pool_concurrency_guard = if let Some(limiter) = pool.pool_concurrency_limiter() {
        match limiter.acquire().await {
            Ok(guard) => Some(guard),
            Err(_) => {
                debug!(
                    "Pool-level concurrency limit exceeded for model: {}",
                    model_name
                );
                return Err(OnwardsErrorResponse::concurrency_limited());
            }
        }
    } else {
        None
    };

    // Acquire per-key concurrency permit
    let _key_concurrency_guard = if let Some(token) = bearer_token {
        if let Some(limiter) = state.targets.key_concurrency_limiters.get(token) {
            match limiter.acquire().await {
                Ok(guard) => Some(guard),
                Err(_) => {
                    debug!("Per-key concurrency limit exceeded for token: {}", token);
                    return Err(OnwardsErrorResponse::concurrency_limited());
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // Extract path info once (used for each provider attempt)
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or(req.uri().path())
        .to_string();

    // Prepare original headers and method for potential retries
    let original_headers = req.headers().clone();
    let method = req.method().clone();

    // Track last error for fallback scenarios
    let mut last_error: Option<OnwardsErrorResponse> = None;

    // Iterate through providers (with fallback support)
    for (_idx, target) in pool.select_ordered() {
        // Check provider-level rate limit (skip to next if configured for rate limit fallback)
        if let Some(ref limiter) = target.limiter
            && limiter.check().is_err()
        {
            debug!("Provider rate limited: {:?}", target.url);
            last_error = Some(OnwardsErrorResponse::rate_limited());
            if pool.should_fallback_on_rate_limit() {
                debug!("Fallback on rate limit enabled, trying next provider");
                continue;
            } else {
                return Err(OnwardsErrorResponse::rate_limited());
            }
        }

        // Acquire provider-level concurrency permit
        let _target_concurrency_guard = if let Some(ref limiter) = target.concurrency_limiter {
            match limiter.acquire().await {
                Ok(guard) => Some(guard),
                Err(_) => {
                    debug!("Provider concurrency limit exceeded: {:?}", target.url);
                    last_error = Some(OnwardsErrorResponse::concurrency_limited());
                    // Concurrency limits are treated like rate limits for fallback purposes
                    if pool.should_fallback_on_rate_limit() {
                        debug!("Fallback on rate limit enabled, trying next provider");
                        continue;
                    } else {
                        return Err(OnwardsErrorResponse::concurrency_limited());
                    }
                }
            }
        } else {
            None
        };

        // Clone response headers for later use
        let response_headers = target.response_headers.clone();

        // Prepare body for this attempt (may need model rewrite)
        let mut attempt_body = body_bytes.clone();

        // Rewrite model field if configured
        if let Some(ref rewrite) = target.onwards_model
            && !attempt_body.is_empty()
        {
            debug!("Rewriting model key to: {}", rewrite);
            let error = OnwardsErrorResponse::bad_request(
                "Could not parse onwards model from request. 'model' parameter must be supplied in either the body or in the Model-Override header.",
                Some("model"),
            );
            let mut body_serialized: serde_json::Value = match serde_json::from_slice(&attempt_body)
            {
                Ok(value) => value,
                Err(_) => return Err(error.clone()),
            };
            let entry = body_serialized
                .as_object_mut()
                .ok_or(error.clone())?
                .entry("model");
            match entry {
                Entry::Occupied(mut entry) => {
                    entry.insert(serde_json::Value::String(rewrite.clone()));
                }
                Entry::Vacant(_entry) => {
                    return Err(error.clone());
                }
            }
            attempt_body = match serde_json::to_vec(&body_serialized) {
                Ok(bytes) => axum::body::Bytes::from(bytes),
                Err(_) => return Err(OnwardsErrorResponse::internal()),
            };
        }

        // Build the upstream URI for this target
        let request_path = path_and_query.strip_prefix('/').unwrap_or(&path_and_query);
        let target_path = target.url.path().trim_end_matches('/');

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let upstream_uri = match target.url.join(path_to_join) {
            Ok(url) => url.to_string(),
            Err(_) => return Err(OnwardsErrorResponse::internal()),
        };
        let upstream_uri_parsed = match Uri::try_from(&upstream_uri) {
            Ok(uri) => uri,
            Err(_) => {
                error!("Invalid URI: {}", upstream_uri);
                return Err(OnwardsErrorResponse::internal());
            }
        };

        // Build request for this provider
        let mut attempt_headers = original_headers.clone();

        // Update host header
        if let Some(host) = upstream_uri_parsed.host() {
            let host_value = if let Some(port) = upstream_uri_parsed.port_u16() {
                format!("{host}:{port}")
            } else {
                host.to_string()
            };
            attempt_headers.insert("host", host_value.parse().unwrap());
        }

        // Set Content-Length and remove Transfer-Encoding
        attempt_headers.insert(
            CONTENT_LENGTH,
            attempt_body
                .len()
                .to_string()
                .parse()
                .expect("Content-Length should be valid"),
        );
        attempt_headers.remove(TRANSFER_ENCODING);

        // Filter headers for upstream forwarding
        filter_headers_for_upstream(&mut attempt_headers, target);

        // Build the request
        let attempt_req = axum::extract::Request::builder()
            .method(method.clone())
            .uri(upstream_uri_parsed)
            .body(axum::body::Body::from(attempt_body))
            .unwrap();
        let (mut parts, body) = attempt_req.into_parts();
        parts.headers = attempt_headers;
        let attempt_req = axum::extract::Request::from_parts(parts, body);

        trace!(
            "Outgoing request to provider:\n  URI: {}\n  Headers: {:?}",
            upstream_uri,
            attempt_req.headers()
        );

        // Make the request with optional timeout
        // Note: Timeout only applies to receiving response headers, not reading the full body.
        // For streaming responses, the body may take much longer than the timeout.
        let request_result = if let Some(timeout_secs) = target.request_timeout_secs {
            let timeout_duration = std::time::Duration::from_secs(timeout_secs);
            match tokio::time::timeout(timeout_duration, state.http_client.request(attempt_req)).await {
                Err(_) => {
                    // Timeout occurred
                    debug!(
                        "Request to {} timed out after {:?}",
                        upstream_uri, timeout_duration
                    );
                    last_error = Some(OnwardsErrorResponse::gateway_timeout());
                    // Only continue to next provider if fallback is enabled
                    if pool.fallback_enabled() {
                        continue;
                    } else {
                        return Err(last_error.unwrap());
                    }
                }
                Ok(result) => result,
            }
        } else {
            // No timeout configured
            state.http_client.request(attempt_req).await
        };

        // Handle request errors
        let mut response = match request_result {
            Err(e) => {
                error!(
                    "Error forwarding request to target url {}: {}",
                    upstream_uri, e
                );
                last_error = Some(OnwardsErrorResponse::bad_gateway());
                // Only continue to next provider if fallback is enabled
                if pool.fallback_enabled() {
                    continue;
                } else {
                    return Err(last_error.unwrap());
                }
            }
            Ok(response) => response,
        };

        let status = response.status().as_u16();

        // Check if we should fallback based on status code
        if pool.should_fallback_on_status(status) {
            debug!(
                "Provider returned fallback status {}, trying next: {:?}",
                status, target.url
            );
            last_error = Some(OnwardsErrorResponse::bad_gateway());
            continue;
        }

        // Check content type for SSE handling
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let is_sse = content_type.contains("text/event-stream");

        // Wrap SSE streams with buffering to handle incomplete chunks from providers.
        // This must happen before any stream processing (e.g., sanitization) to ensure
        // downstream consumers receive complete SSE events with valid JSON.
        if is_sse {
            debug!("Wrapping SSE response with buffered stream");
            let (parts, body) = response.into_parts();
            let byte_stream = body.into_data_stream();
            let buffered = SseBufferedStream::new(byte_stream);
            let new_body = axum::body::Body::from_stream(buffered);
            response = Response::from_parts(parts, new_body);
        }

        // Apply response transformation if configured
        // Per-target opt-in via sanitize_response flag, only for 2xx responses
        if let Some(ref transform_fn) = state.response_transform_fn
            && target.sanitize_response
            && (200..300).contains(&status)
        {
            debug!(
                "Attempting response sanitization for status {}, path {}",
                status, path_and_query
            );

            // Extract original model from request extensions
            let original_model = req
                .extensions()
                .get::<OriginalModel>()
                .map(|m| m.0.to_string());

            if is_sse {
                // Streaming SSE response - transform chunk-by-chunk
                // Note: stream is already buffered above
                debug!("Applying streaming sanitization");

                let sanitizer = crate::response_sanitizer::ResponseSanitizer {
                    original_model: original_model.clone(),
                };

                use futures_util::StreamExt;

                let body_stream = http_body_util::BodyExt::into_data_stream(
                    std::mem::take(response.body_mut()),
                );

                let transformed_stream = body_stream.map(move |chunk_result| {
                    match chunk_result {
                        Ok(chunk) => {
                            // Sanitize this chunk
                            match sanitizer.sanitize_streaming(&chunk) {
                                Ok(Some(sanitized)) => Ok::<_, std::io::Error>(sanitized),
                                Ok(None) => Ok(chunk),
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to sanitize streaming chunk: {}",
                                        e
                                    );
                                    Ok(chunk) // Pass through on error
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Stream error: {}", e);
                            Err(std::io::Error::other(e))
                        }
                    }
                });

                *response.body_mut() =
                    axum::body::Body::from_stream(transformed_stream);
            } else {
                // Non-streaming response - buffer and transform
                debug!("Applying non-streaming sanitization");

                let response_body = axum::body::to_bytes(
                    std::mem::take(response.body_mut()),
                    usize::MAX,
                )
                .await
                .map_err(|e| {
                    error!("Failed to buffer response body: {}", e);
                    OnwardsErrorResponse::internal()
                })?;

                debug!(
                    "Response body buffered: {} bytes, content-type: {}",
                    response_body.len(),
                    content_type
                );
                trace!(
                    "Response body content: {}",
                    String::from_utf8_lossy(&response_body)
                );

                // Apply transformation
                match transform_fn(
                    &path_and_query,
                    response.headers(),
                    &response_body,
                    original_model.as_deref(),
                ) {
                    Ok(Some(transformed_body)) => {
                        // Update response with sanitized body
                        let content_length = transformed_body.len();
                        debug!(
                            "Sanitization successful: {} bytes -> {} bytes",
                            response_body.len(),
                            content_length
                        );
                        trace!(
                            "Sanitized body: {}",
                            String::from_utf8_lossy(&transformed_body)
                        );
                        *response.body_mut() =
                            axum::body::Body::from(transformed_body);

                        // Remove transfer-encoding since we're setting content-length
                        response.headers_mut().remove(TRANSFER_ENCODING);
                        response.headers_mut().insert(
                            CONTENT_LENGTH,
                            HeaderValue::from(content_length),
                        );
                    }
                    Ok(None) => {
                        // No transformation applied, restore original body
                        debug!(
                            "Sanitization returned None, restoring original {} bytes",
                            response_body.len()
                        );
                        let content_length = response_body.len();
                        *response.body_mut() =
                            axum::body::Body::from(response_body);

                        // Ensure proper headers even when not transforming
                        response.headers_mut().remove(TRANSFER_ENCODING);
                        response.headers_mut().insert(
                            CONTENT_LENGTH,
                            HeaderValue::from(content_length),
                        );
                    }
                    Err(e) => {
                        error!("Response sanitization failed: {}", e);
                        return Err(OnwardsErrorResponse::internal());
                    }
                }
            }
        }

        // Add custom response headers
        if let Some(headers) = response_headers {
            for (key, value) in headers.iter() {
                if let (Ok(header_name), Ok(header_value)) =
                    (key.parse::<HeaderName>(), value.parse::<HeaderValue>())
                {
                    response.headers_mut().insert(header_name, header_value);
                }
            }
            trace!(
                model = %model_name,
                headers = ?headers,
                "Added custom response headers"
            );
        }

        record_response_status(response.status().as_u16());
        debug!(
            "Returning response with status {}, content-length: {:?}",
            response.status(),
            response.headers().get(CONTENT_LENGTH)
        );
        return Ok(response);
    }

    // All providers exhausted
    record_response_status(502);
    Err(last_error.unwrap_or_else(|| OnwardsErrorResponse::model_not_found(model_name.as_str())))
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

    // Filter targets based on bearer token permissions using pool-level keys
    let accessible_models: Vec<String> = state
        .targets
        .targets
        .iter()
        .filter(|entry| {
            let pool = entry.value();

            // If pool has no keys configured, it's publicly accessible
            let Some(keys) = pool.keys() else {
                return true;
            };

            // If pool has keys but no bearer token provided, deny access
            let Some(token) = bearer_token else {
                return false;
            };

            // Validate bearer token against pool's keys
            auth::validate_bearer_token(keys, token)
        })
        .map(|entry| entry.key().clone())
        .collect();

    // Create filtered response
    Json(ListModelResponse::from_model_names(&accessible_models))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_headers_strips_hop_by_hop_headers() {
        let mut headers = HeaderMap::new();

        // Add hop-by-hop headers that should be removed
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("proxy-authenticate", "Basic".parse().unwrap());
        headers.insert("proxy-authorization", "Basic abc123".parse().unwrap());
        headers.insert("te", "trailers".parse().unwrap());
        headers.insert("trailer", "Expires".parse().unwrap());
        headers.insert("upgrade", "HTTP/2.0".parse().unwrap());

        // Add a safe header that should be kept
        headers.insert("content-type", "application/json".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify hop-by-hop headers were removed
        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("proxy-authenticate"));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key("te"));
        assert!(!headers.contains_key("trailer"));
        assert!(!headers.contains_key("upgrade"));

        // Verify safe header was kept
        assert!(headers.contains_key("content-type"));
    }

    #[test]
    fn test_filter_headers_strips_auth_headers() {
        let mut headers = HeaderMap::new();

        // Add auth headers that should be removed
        headers.insert("authorization", "Bearer client-token".parse().unwrap());
        headers.insert("x-api-key", "client-api-key".parse().unwrap());
        headers.insert("api-key", "another-key".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify all auth headers were removed (client credentials stripped)
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-api-key"));
        assert!(!headers.contains_key("api-key"));
    }

    #[test]
    fn test_filter_headers_strips_browser_security_headers() {
        let mut headers = HeaderMap::new();

        // Add browser security headers
        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        headers.insert("sec-fetch-mode", "cors".parse().unwrap());
        headers.insert("sec-fetch-dest", "empty".parse().unwrap());
        headers.insert("sec-fetch-user", "?1".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify all sec-fetch-* headers were removed
        assert!(!headers.contains_key("sec-fetch-site"));
        assert!(!headers.contains_key("sec-fetch-mode"));
        assert!(!headers.contains_key("sec-fetch-dest"));
        assert!(!headers.contains_key("sec-fetch-user"));
    }

    #[test]
    fn test_filter_headers_strips_all_sec_ch_ua_variants() {
        let mut headers = HeaderMap::new();

        // Add various sec-ch-ua headers
        headers.insert("sec-ch-ua", "\"Chrome\";v=\"120\"".parse().unwrap());
        headers.insert("sec-ch-ua-mobile", "?0".parse().unwrap());
        headers.insert("sec-ch-ua-platform", "\"macOS\"".parse().unwrap());
        headers.insert("sec-ch-ua-arch", "\"arm64\"".parse().unwrap());
        headers.insert("sec-ch-ua-model", "\"\"".parse().unwrap());

        // Add a safe header
        headers.insert("user-agent", "Mozilla/5.0...".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify all sec-ch-ua* headers were removed
        assert!(!headers.contains_key("sec-ch-ua"));
        assert!(!headers.contains_key("sec-ch-ua-mobile"));
        assert!(!headers.contains_key("sec-ch-ua-platform"));
        assert!(!headers.contains_key("sec-ch-ua-arch"));
        assert!(!headers.contains_key("sec-ch-ua-model"));

        // Verify user-agent was kept
        assert!(headers.contains_key("user-agent"));
    }

    #[test]
    fn test_filter_headers_strips_browser_context_headers() {
        let mut headers = HeaderMap::new();

        headers.insert("origin", "http://localhost:3000".parse().unwrap());
        headers.insert("referer", "http://localhost:3000/chat".parse().unwrap());
        headers.insert("cookie", "session=abc123".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        assert!(!headers.contains_key("origin"));
        assert!(!headers.contains_key("referer"));
        assert!(!headers.contains_key("cookie"));
    }

    #[test]
    fn test_filter_headers_strips_caching_headers() {
        let mut headers = HeaderMap::new();

        headers.insert(
            "if-modified-since",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        headers.insert("if-none-match", "\"abc123\"".parse().unwrap());
        headers.insert("if-match", "\"xyz789\"".parse().unwrap());
        headers.insert(
            "if-unmodified-since",
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        headers.insert("if-range", "\"abc123\"".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        assert!(!headers.contains_key("if-modified-since"));
        assert!(!headers.contains_key("if-none-match"));
        assert!(!headers.contains_key("if-match"));
        assert!(!headers.contains_key("if-unmodified-since"));
        assert!(!headers.contains_key("if-range"));
    }

    #[test]
    fn test_filter_headers_strips_model_override_header() {
        let mut headers = HeaderMap::new();

        headers.insert("model-override", "gpt-4".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // model-override should be removed (already consumed for routing)
        assert!(!headers.contains_key("model-override"));
        // content-type should be kept
        assert!(headers.contains_key("content-type"));
    }

    #[test]
    fn test_filter_headers_keeps_safe_headers() {
        let mut headers = HeaderMap::new();

        // Add headers that should be kept
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());
        headers.insert("user-agent", "MyClient/1.0".parse().unwrap());
        headers.insert("accept-language", "en-US,en;q=0.9".parse().unwrap());
        headers.insert("accept-encoding", "gzip, deflate, br".parse().unwrap());
        headers.insert("x-stainless-lang", "js".parse().unwrap());
        headers.insert("x-stainless-os", "macOS".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // All these headers should be kept
        assert!(headers.contains_key("content-type"));
        assert!(headers.contains_key("accept"));
        assert!(headers.contains_key("user-agent"));
        assert!(headers.contains_key("accept-language"));
        assert!(headers.contains_key("accept-encoding"));
        assert!(headers.contains_key("x-stainless-lang"));
        assert!(headers.contains_key("x-stainless-os"));
    }

    #[test]
    fn test_filter_headers_adds_authorization_when_onwards_key_present() {
        let mut headers = HeaderMap::new();

        // Add client authorization that should be stripped
        headers.insert("authorization", "Bearer client-token".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("sk-upstream-key".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Client auth should be removed and replaced with onwards_key
        assert!(headers.contains_key("authorization"));
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer sk-upstream-key"
        );
    }

    #[test]
    fn test_filter_headers_no_authorization_when_onwards_key_absent() {
        let mut headers = HeaderMap::new();

        // Add client authorization
        headers.insert("authorization", "Bearer client-token".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            // No onwards_key configured
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Client auth should be removed and NOT replaced (no onwards_key)
        assert!(!headers.contains_key("authorization"));
    }

    #[test]
    fn test_filter_headers_custom_auth_header_name() {
        let mut headers = HeaderMap::new();

        // Add client authorization that should be stripped
        headers.insert("authorization", "Bearer client-token".parse().unwrap());

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("my-api-key-123".to_string())
            .upstream_auth_header_name("X-API-Key".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Client auth should be removed
        assert!(!headers.contains_key("authorization"));

        // Custom header should be added with default Bearer prefix
        assert!(headers.contains_key("x-api-key"));
        assert_eq!(
            headers.get("x-api-key").unwrap().to_str().unwrap(),
            "Bearer my-api-key-123"
        );
    }

    #[test]
    fn test_filter_headers_custom_auth_header_prefix() {
        let mut headers = HeaderMap::new();

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("token-xyz".to_string())
            .upstream_auth_header_prefix("ApiKey ".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Should use custom prefix with default Authorization header
        assert!(headers.contains_key("authorization"));
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "ApiKey token-xyz"
        );
    }

    #[test]
    fn test_filter_headers_empty_auth_header_prefix() {
        let mut headers = HeaderMap::new();

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("plain-api-key-456".to_string())
            .upstream_auth_header_prefix("".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Should use empty prefix (just the key value)
        assert!(headers.contains_key("authorization"));
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "plain-api-key-456"
        );
    }

    #[test]
    fn test_filter_headers_custom_header_name_and_prefix() {
        let mut headers = HeaderMap::new();

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .onwards_key("secret-key".to_string())
            .upstream_auth_header_name("X-Custom-Auth".to_string())
            .upstream_auth_header_prefix("Token ".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Should use both custom header name and custom prefix
        assert!(!headers.contains_key("authorization"));
        assert!(headers.contains_key("x-custom-auth"));
        assert_eq!(
            headers.get("x-custom-auth").unwrap().to_str().unwrap(),
            "Token secret-key"
        );
    }

    #[test]
    fn test_filter_headers_adds_x_forwarded_proto() {
        let mut headers = HeaderMap::new();

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        assert!(headers.contains_key("x-forwarded-proto"));
        assert_eq!(
            headers.get("x-forwarded-proto").unwrap().to_str().unwrap(),
            "https"
        );
    }

    #[test]
    fn test_filter_headers_comprehensive_browser_request() {
        // Simulate a real browser request with all the problematic headers
        let mut headers = HeaderMap::new();

        // Hop-by-hop
        headers.insert("connection", "keep-alive".parse().unwrap());

        // Auth (should be stripped)
        headers.insert("authorization", "Bearer client-secret".parse().unwrap());

        // Browser security
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        headers.insert("sec-fetch-mode", "cors".parse().unwrap());
        headers.insert("sec-fetch-dest", "empty".parse().unwrap());
        headers.insert("sec-ch-ua", "\"Chrome\";v=\"120\"".parse().unwrap());
        headers.insert("sec-ch-ua-mobile", "?0".parse().unwrap());
        headers.insert("sec-ch-ua-platform", "\"macOS\"".parse().unwrap());

        // Browser context
        headers.insert("origin", "http://localhost:5173".parse().unwrap());
        headers.insert(
            "referer",
            "http://localhost:5173/playground".parse().unwrap(),
        );
        headers.insert("cookie", "session=xyz; token=abc".parse().unwrap());

        // Caching
        headers.insert("if-none-match", "\"abc123\"".parse().unwrap());

        // Safe headers that should be kept
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("accept", "application/json".parse().unwrap());
        headers.insert("user-agent", "Mozilla/5.0...".parse().unwrap());

        let target = Target::builder()
            .url("https://api.anthropic.com".parse().unwrap())
            .onwards_key("sk-ant-upstream-key".to_string())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // Verify all problematic headers were removed
        assert!(!headers.contains_key("connection"));
        assert!(!headers.contains_key("sec-fetch-site"));
        assert!(!headers.contains_key("sec-fetch-mode"));
        assert!(!headers.contains_key("sec-fetch-dest"));
        assert!(!headers.contains_key("sec-ch-ua"));
        assert!(!headers.contains_key("sec-ch-ua-mobile"));
        assert!(!headers.contains_key("sec-ch-ua-platform"));
        assert!(!headers.contains_key("origin"));
        assert!(!headers.contains_key("referer"));
        assert!(!headers.contains_key("cookie"));
        assert!(!headers.contains_key("if-none-match"));

        // Verify safe headers were kept
        assert!(headers.contains_key("content-type"));
        assert!(headers.contains_key("accept"));
        assert!(headers.contains_key("user-agent"));

        // Verify Authorization was replaced with upstream key
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer sk-ant-upstream-key"
        );

        // Verify X-Forwarded-Proto was added
        assert_eq!(
            headers.get("x-forwarded-proto").unwrap().to_str().unwrap(),
            "https"
        );
    }

    #[test]
    fn test_filter_headers_preserves_provider_specific_headers() {
        let mut headers = HeaderMap::new();

        // Provider-specific headers that should be kept
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        headers.insert("openai-organization", "org-123".parse().unwrap());
        headers.insert("x-stainless-lang", "js".parse().unwrap());
        headers.insert("x-stainless-runtime", "browser:chrome".parse().unwrap());

        let target = Target::builder()
            .url("https://api.anthropic.com".parse().unwrap())
            .build();

        filter_headers_for_upstream(&mut headers, &target);

        // All provider-specific headers should be kept
        assert!(headers.contains_key("anthropic-version"));
        assert!(headers.contains_key("openai-organization"));
        assert!(headers.contains_key("x-stainless-lang"));
        assert!(headers.contains_key("x-stainless-runtime"));
    }

    #[test]
    fn test_path_stripping_with_duplicate_prefix() {
        // Test the logic: target URL has "/v1", request path has "/v1/chat/completions"
        // Should result in "https://api.openai.com/v1/chat/completions" not "/v1/v1/..."

        let target_url = url::Url::parse("https://api.openai.com/v1/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/v1"
        let request_path = "v1/chat/completions"; // already stripped leading /

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(
            result.as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_path_stripping_without_duplicate() {
        // Test: target URL has no path, request path has "/v1/chat/completions"
        // Should result in "https://api.openai.com/v1/chat/completions"

        let target_url = url::Url::parse("https://api.openai.com/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/"
        let request_path = "v1/chat/completions";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(
            result.as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_path_stripping_with_actual_duplicate_paths() {
        // Edge case: API actually has /v1/v1/something
        // Target has no path, request has "v1/v1/something"

        let target_url = url::Url::parse("https://api.example.com/").unwrap();
        let target_path = target_url.path().trim_end_matches('/');
        let request_path = "v1/v1/something";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(result.as_str(), "https://api.example.com/v1/v1/something");
    }

    #[test]
    fn test_path_stripping_with_different_prefix() {
        // Test: target has "/v2", request has "/v1/chat/completions"
        // Should not strip, result in "https://api.example.com/v2/v1/chat/completions"

        let target_url = url::Url::parse("https://api.example.com/v2/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/v2"
        let request_path = "v1/chat/completions";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(
            result.as_str(),
            "https://api.example.com/v2/v1/chat/completions"
        );
    }

    #[test]
    fn test_path_stripping_with_query_params() {
        // Test: path with query parameters should work correctly

        let target_url = url::Url::parse("https://api.openai.com/v1/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/v1"
        let request_path = "v1/chat/completions?stream=true";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        assert_eq!(
            result.as_str(),
            "https://api.openai.com/v1/chat/completions?stream=true"
        );
    }

    #[test]
    fn test_path_stripping_false_positive() {
        // Test: path starts with target prefix but no slash after (e.g., "v1x")
        // Should NOT strip since it's not a real path match

        let target_url = url::Url::parse("https://api.example.com/v1/").unwrap();
        let target_path = target_url.path().trim_end_matches('/'); // "/v1"
        let request_path = "v1x/something";

        let path_to_join = if !target_path.is_empty() && target_path != "/" {
            let target_path_no_slash = &target_path[1..];
            if let Some(rest) = request_path.strip_prefix(target_path_no_slash) {
                if rest.is_empty() || rest.starts_with('/') {
                    rest.strip_prefix('/').unwrap_or(rest)
                } else {
                    request_path
                }
            } else {
                request_path
            }
        } else {
            request_path
        };

        let result = target_url.join(path_to_join).unwrap();
        // Should NOT strip "v1" since "v1x" is not the same as "v1/"
        assert_eq!(result.as_str(), "https://api.example.com/v1/v1x/something");
    }

    // Timeout behavior tests
    // These tests document the expected behavior but can't easily test the actual timeout
    // functionality without a mock HTTP client. They serve as documentation and type-checking.

    #[test]
    fn test_timeout_config_can_be_set() {
        // Test that timeout can be configured on target
        use crate::target::Target;

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .request_timeout_secs(30)
            .build();

        assert_eq!(target.request_timeout_secs, Some(30));
    }

    #[test]
    fn test_timeout_defaults_to_none() {
        // Test that timeout defaults to None (unlimited)
        use crate::target::Target;

        let target = Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build();

        assert_eq!(target.request_timeout_secs, None);
    }

    #[test]
    fn test_gateway_timeout_error_response() {
        // Test that gateway_timeout returns 504 status
        let error = OnwardsErrorResponse::gateway_timeout();
        let response = error.into_response();
        
        assert_eq!(response.status().as_u16(), 504);
    }

    #[tokio::test]
    async fn test_gateway_timeout_error_body() {
        // Test that gateway_timeout has appropriate error message
        use http_body_util::BodyExt;
        
        let error = OnwardsErrorResponse::gateway_timeout();
        let response = error.into_response();
        
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
        
        // Should contain error information about timeout
        assert!(body_str.contains("error"));
        assert!(body_str.contains("timeout") || body_str.contains("took too long"));
    }
}