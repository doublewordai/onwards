//! HTTP request handlers for the proxy server
//!
//! This module contains the main Axum handlers that process incoming requests,
//! route them to appropriate targets, and handle authentication and rate limiting.

use crate::AppState;
use crate::auth;
use crate::client::HttpClient;
use crate::errors::OnwardsErrorResponse;
use crate::models::ListModelResponse;
use crate::target::Target;
use axum::{
    Json,
    extract::Request,
    extract::State,
    http::{
        HeaderMap, Uri,
        header::{CONTENT_LENGTH, TRANSFER_ENCODING},
    },
    response::{IntoResponse, Response},
};
use serde_json::map::Entry;
use tracing::{debug, error, instrument, trace};

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
        debug!("Adding authorization header for upstream {}", target.url);
        headers.insert("Authorization", format!("Bearer {key}").parse().unwrap());
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

    // Filter headers for upstream forwarding (RFC 7230 compliance, security, etc.)
    filter_headers_for_upstream(req.headers_mut(), &target);

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
}
