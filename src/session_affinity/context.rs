use axum::http::HeaderMap;

use crate::session_affinity::key::compute_session_fingerprint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    HeaderOnly,
    AuthorizationOnly,
    HeaderAndAuthorization,
    None,
}

impl SessionSource {
    fn from_identity_inputs(has_session_id: bool, has_authorization: bool) -> Self {
        match (has_session_id, has_authorization) {
            (true, true) => Self::HeaderAndAuthorization,
            (true, false) => Self::HeaderOnly,
            (false, true) => Self::AuthorizationOnly,
            (false, false) => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionContext {
    pub fingerprint: String,
    pub source: SessionSource,
}

pub fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("authorization")?.to_str().ok()?.trim();
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

pub fn extract_session_header(headers: &HeaderMap, header_name: &str) -> Option<String> {
    let value = headers.get(header_name)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub fn build_session_context(
    headers: &HeaderMap,
    session_header_name: &str,
) -> Option<SessionContext> {
    let session_id = extract_session_header(headers, session_header_name);
    let authorization = extract_bearer_token(headers);
    let fingerprint = compute_session_fingerprint(session_id.as_deref(), authorization.as_deref())?;

    Some(SessionContext {
        fingerprint,
        source: SessionSource::from_identity_inputs(session_id.is_some(), authorization.is_some()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn header_whitespace_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", HeaderValue::from_static("  abc  "));
        let session = extract_session_header(&headers, "x-session-id").unwrap();
        assert_eq!(session, "abc");
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("bEaReR token"));
        let token = extract_bearer_token(&headers).unwrap();
        assert_eq!(token, "token");
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Session-Id", HeaderValue::from_static("session-1"));
        let session = extract_session_header(&headers, "x-session-id").unwrap();
        assert_eq!(session, "session-1");
    }

    #[test]
    fn non_bearer_authorization_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Basic tenant-a"));
        assert!(extract_bearer_token(&headers).is_none());
        assert!(build_session_context(&headers, "x-session-id").is_none());
    }

    #[test]
    fn build_session_context_marks_header_only_source() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", HeaderValue::from_static("session-1"));

        let session = build_session_context(&headers, "x-session-id").unwrap();

        assert_eq!(session.source, SessionSource::HeaderOnly);
    }

    #[test]
    fn build_session_context_marks_authorization_only_source() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer tenant-a"));

        let session = build_session_context(&headers, "x-session-id").unwrap();

        assert_eq!(session.source, SessionSource::AuthorizationOnly);
    }

    #[test]
    fn build_session_context_marks_combined_source() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", HeaderValue::from_static("session-1"));
        headers.insert("authorization", HeaderValue::from_static("Bearer tenant-a"));

        let session = build_session_context(&headers, "x-session-id").unwrap();

        assert_eq!(session.source, SessionSource::HeaderAndAuthorization);
    }

    #[test]
    fn session_source_marks_missing_identity() {
        assert_eq!(
            SessionSource::from_identity_inputs(false, false),
            SessionSource::None
        );
    }
}
