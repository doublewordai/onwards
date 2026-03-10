use crate::target::Target;
use sha2::{Digest, Sha256};

pub fn compute_session_fingerprint(
    session_id: Option<&str>,
    authorization: Option<&str>,
) -> Option<String> {
    let identity = match (session_id, authorization) {
        (Some(session_id), Some(authorization)) => format!("{authorization}|{session_id}"),
        (Some(session_id), None) => session_id.to_string(),
        (None, Some(authorization)) => authorization.to_string(),
        (None, None) => return None,
    };

    let mut hasher = Sha256::new();
    hasher.update(identity.as_bytes());
    Some(hex::encode(hasher.finalize()))
}

pub fn compute_provider_id(target: &Target) -> String {
    let mut hasher = Sha256::new();
    hasher.update(target.url.as_str().as_bytes());
    hasher.update(b"|");
    hasher.update(target.onwards_key.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(target.onwards_model.as_deref().unwrap_or("").as_bytes());
    let digest = hex::encode(hasher.finalize());
    digest[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Target;

    #[test]
    fn fingerprint_combines_auth_boundary() {
        let a = compute_session_fingerprint(Some("session"), Some("auth-a")).unwrap();
        let b = compute_session_fingerprint(Some("session"), Some("auth-b")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn provider_id_uses_delimiters() {
        let left = Target::builder()
            .url("https://api.example.com/".parse().unwrap())
            .onwards_key("12".to_string())
            .onwards_model("3".to_string())
            .build();
        let right = Target::builder()
            .url("https://api.example.com1/".parse().unwrap())
            .onwards_key("2".to_string())
            .onwards_model("3".to_string())
            .build();
        assert_ne!(compute_provider_id(&left), compute_provider_id(&right));
    }
}
