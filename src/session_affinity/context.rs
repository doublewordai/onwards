use axum::http::HeaderMap;

use crate::session_affinity::key::compute_session_fingerprint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
    HeaderOnly,
    AuthorizationOnly,
    HeaderAndAuthorization,
    None,
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

    let source = match (session_id.is_some(), authorization.is_some()) {
        (true, true) => SessionSource::HeaderAndAuthorization,
        (true, false) => SessionSource::HeaderOnly,
        (false, true) => SessionSource::AuthorizationOnly,
        (false, false) => SessionSource::None,
    };

    Some(SessionContext {
        fingerprint,
        source,
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
}
