//! Integration tests for the onwards proxy server
//!
//! These tests verify end-to-end behavior including request/response extensions, routing, authentication, and other features that require full server setup.

use axum::http::StatusCode;
use dashmap::DashMap;
use onwards::target::{Target, Targets, TokenPricing};
use onwards::test_utils::MockHttpClient;
use onwards::{build_router, AppState};
use serde_json::json;
use std::sync::Arc;
use tower::util::ServiceExt; // for oneshot()

#[tokio::test]
async fn test_pricing_added_to_response_extensions_when_configured() {
    // Create target WITH pricing
    let targets_map = Arc::new(DashMap::new());
    targets_map.insert(
        "gpt-4".to_string(),
        Target::builder()
            .url("https://api.openai.com".parse().unwrap())
            .pricing(TokenPricing {
                input_price_per_token: Some(0.00003),
                output_price_per_token: Some(0.00006),
            })
            .build(),
    );

    let targets = Targets {
        targets: targets_map,
        key_rate_limiters: Arc::new(DashMap::new()),
    };

    let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
    let app_state = AppState::with_client(targets, mock_client.clone());
    let app = build_router(app_state);

    // Build request
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .unwrap(),
        ))
        .unwrap();

    // Make request using oneshot to get access to response extensions
    let response = app.oneshot(request).await.unwrap();

    // Verify status
    assert_eq!(response.status(), StatusCode::OK);

    // Access and verify extensions
    let pricing = response.extensions().get::<TokenPricing>();
    assert!(pricing.is_some(), "Pricing should be present in extensions");

    let pricing = pricing.unwrap();
    assert_eq!(pricing.input_price_per_token, Some(0.00003));
    assert_eq!(pricing.output_price_per_token, Some(0.00006));
}

#[tokio::test]
async fn test_no_pricing_in_extensions_when_not_configured() {
    // Create target WITHOUT pricing
    let targets_map = Arc::new(DashMap::new());
    targets_map.insert(
        "free-model".to_string(),
        Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .build(),
    );

    let targets = Targets {
        targets: targets_map,
        key_rate_limiters: Arc::new(DashMap::new()),
    };

    let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
    let app_state = AppState::with_client(targets, mock_client.clone());
    let app = build_router(app_state);

    // Build request
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({
                "model": "free-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .unwrap(),
        ))
        .unwrap();

    // Make request using oneshot
    let response = app.oneshot(request).await.unwrap();

    // Verify status
    assert_eq!(response.status(), StatusCode::OK);

    // Verify no pricing in extensions
    let pricing = response.extensions().get::<TokenPricing>();
    assert!(
        pricing.is_none(),
        "Pricing should NOT be present in extensions when not configured"
    );
}

#[tokio::test]
async fn test_pricing_preserved_in_error_response_extensions() {
    // Create target with pricing that will return an error
    let targets_map = Arc::new(DashMap::new());
    targets_map.insert(
        "error-model".to_string(),
        Target::builder()
            .url("https://api.example.com".parse().unwrap())
            .pricing(TokenPricing {
                input_price_per_token: Some(0.00001),
                output_price_per_token: Some(0.00002),
            })
            .build(),
    );

    let targets = Targets {
        targets: targets_map,
        key_rate_limiters: Arc::new(DashMap::new()),
    };

    // Mock client returns 500 error
    let mock_client =
        MockHttpClient::new(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error": "Server error"}"#);
    let app_state = AppState::with_client(targets, mock_client.clone());
    let app = build_router(app_state);

    // Build request
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({
                "model": "error-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .unwrap(),
        ))
        .unwrap();

    // Make request using oneshot
    let response = app.oneshot(request).await.unwrap();

    // Should still get the error response
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // Pricing should still be in extensions even for error responses
    let pricing = response.extensions().get::<TokenPricing>();
    assert!(
        pricing.is_some(),
        "Pricing should be present in extensions even for error responses"
    );

    let pricing = pricing.unwrap();
    assert_eq!(pricing.input_price_per_token, Some(0.00001));
    assert_eq!(pricing.output_price_per_token, Some(0.00002));
}

#[tokio::test]
async fn test_pricing_in_extensions_with_different_models() {
    // Create multiple targets with different pricing
    let targets_map = Arc::new(DashMap::new());
    targets_map.insert(
        "expensive-model".to_string(),
        Target::builder()
            .url("https://api.expensive.com".parse().unwrap())
            .pricing(TokenPricing {
                input_price_per_token: Some(0.0001),
                output_price_per_token: Some(0.0002),
            })
            .build(),
    );
    targets_map.insert(
        "cheap-model".to_string(),
        Target::builder()
            .url("https://api.cheap.com".parse().unwrap())
            .pricing(TokenPricing {
                input_price_per_token: Some(0.000001),
                output_price_per_token: Some(0.000002),
            })
            .build(),
    );

    let targets = Targets {
        targets: targets_map,
        key_rate_limiters: Arc::new(DashMap::new()),
    };

    let mock_client = MockHttpClient::new(StatusCode::OK, r#"{"success": true}"#);
    let app_state = AppState::with_client(targets, mock_client.clone());

    // Test expensive model
    let app = build_router(app_state.clone());
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({
                "model": "expensive-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let pricing = response.extensions().get::<TokenPricing>().unwrap();
    assert_eq!(pricing.input_price_per_token, Some(0.0001));
    assert_eq!(pricing.output_price_per_token, Some(0.0002));

    // Test cheap model
    let app = build_router(app_state);
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&json!({
                "model": "cheap-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let pricing = response.extensions().get::<TokenPricing>().unwrap();
    assert_eq!(pricing.input_price_per_token, Some(0.000001));
    assert_eq!(pricing.output_price_per_token, Some(0.000002));
}
