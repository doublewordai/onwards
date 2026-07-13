//! Canonical OpenAI reasoning controls and provider-specific request translation.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

const ALLOWED_EFFORTS: &str = "none, minimal, low, medium, high, xhigh, max";
const MAX_TARGET_DEPTH: usize = 8;
const MAX_VALUE_BYTES: usize = 8 * 1024;
const MAX_VALUE_DEPTH: usize = 8;

/// Canonical reasoning effort values accepted by OpenAI-compatible endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = ReasoningError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|effort| effort.as_str() == value)
            .ok_or_else(|| ReasoningError::invalid_effort(value, "reasoning_effort"))
    }
}

/// Maps canonical reasoning efforts to values at one provider request path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTranslation {
    pub target_path: String,
    pub values: BTreeMap<ReasoningEffort, Value>,
}

/// Provider translations for each supported OpenAI request surface.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTranslationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_completions: Option<ReasoningTranslation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses: Option<ReasoningTranslation>,
}

/// A parameter-specific error safe to return in an OpenAI error envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningError {
    message: String,
    param: Option<String>,
    code: &'static str,
}

impl ReasoningError {
    fn new(message: impl Into<String>, param: Option<&str>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            param: param.map(str::to_string),
            code,
        }
    }

    fn invalid_effort(value: &str, param: &str) -> Self {
        Self::new(
            format!("Invalid value '{value}' for '{param}'. Expected one of: {ALLOWED_EFFORTS}."),
            Some(param),
            "invalid_value",
        )
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn param(&self) -> Option<&str> {
        self.param.as_deref()
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ReasoningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReasoningError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiSurface {
    ChatCompletions,
    Responses,
    Completions,
    Other,
}

impl ApiSurface {
    fn from_path(path: &str) -> Self {
        let path = path.trim_end_matches('/');
        if path.ends_with("/chat/completions") {
            Self::ChatCompletions
        } else if path.ends_with("/responses") {
            Self::Responses
        } else if path.ends_with("/completions") {
            Self::Completions
        } else {
            Self::Other
        }
    }

    const fn canonical_path(self) -> Option<&'static str> {
        match self {
            Self::ChatCompletions => Some("/reasoning_effort"),
            Self::Responses => Some("/reasoning/effort"),
            Self::Completions | Self::Other => None,
        }
    }

    const fn parameter_name(self) -> Option<&'static str> {
        match self {
            Self::ChatCompletions => Some("reasoning_effort"),
            Self::Responses => Some("reasoning.effort"),
            Self::Completions | Self::Other => None,
        }
    }
}

impl ReasoningTranslationConfig {
    /// Validate all target paths and mapped values before accepting configuration.
    pub fn validate(&self) -> Result<(), ReasoningError> {
        if self.chat_completions.is_none() && self.responses.is_none() {
            return Err(invalid_config(
                "at least one API surface must be configured",
            ));
        }

        for translation in [self.chat_completions.as_ref(), self.responses.as_ref()]
            .into_iter()
            .flatten()
        {
            validate_translation(translation)?;
        }
        Ok(())
    }

    /// Apply the configured provider mapping to a canonical request body.
    pub fn apply(&self, path: &str, body: &mut Value) -> Result<(), ReasoningError> {
        self.validate()?;
        let surface = ApiSurface::from_path(path);
        let Some(effort) = validate_canonical_reasoning(path, body)? else {
            return Ok(());
        };
        let translation = self.translation_for(surface);
        let Some(translation) = translation else {
            return Ok(());
        };
        self.validate_effort(path, effort)?;
        let mapped_value = translation
            .values
            .get(&effort)
            .cloned()
            .expect("validated effort has a provider mapping");

        let canonical_path = surface
            .canonical_path()
            .expect("translated reasoning surfaces have canonical paths");
        if translation.target_path != canonical_path {
            remove_json_pointer(body, canonical_path);
        }
        set_json_pointer(body, &translation.target_path, mapped_value)
    }

    /// Ensure a provider can represent an effort before load balancing begins.
    pub fn validate_effort(
        &self,
        path: &str,
        effort: ReasoningEffort,
    ) -> Result<(), ReasoningError> {
        self.validate()?;
        let surface = ApiSurface::from_path(path);
        let Some(translation) = self.translation_for(surface) else {
            return Ok(());
        };
        if translation.values.contains_key(&effort) {
            return Ok(());
        }
        let supported = translation
            .values
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        Err(ReasoningError::new(
            format!(
                "Reasoning effort '{effort}' is not supported by this provider. Supported values: {supported}."
            ),
            surface.parameter_name(),
            "unsupported_value",
        ))
    }

    fn translation_for(&self, surface: ApiSurface) -> Option<&ReasoningTranslation> {
        match surface {
            ApiSurface::ChatCompletions => self.chat_completions.as_ref(),
            ApiSurface::Responses => self.responses.as_ref(),
            ApiSurface::Completions | ApiSurface::Other => None,
        }
    }
}

/// Validate and return the canonical effort supplied for the request surface.
pub fn validate_canonical_reasoning(
    path: &str,
    body: &Value,
) -> Result<Option<ReasoningEffort>, ReasoningError> {
    let surface = ApiSurface::from_path(path);
    if surface == ApiSurface::Completions {
        let unsupported = [
            ("/reasoning_effort", "reasoning_effort"),
            ("/reasoning", "reasoning"),
            ("/thinking", "thinking"),
            ("/chat_template_kwargs", "chat_template_kwargs"),
        ]
        .into_iter()
        .find(|(pointer, _)| body.pointer(pointer).is_some());
        if let Some((_, param)) = unsupported {
            return Err(ReasoningError::new(
                format!(
                    "Parameter '{param}' is not supported by /v1/completions. Use /v1/chat/completions or /v1/responses."
                ),
                Some(param),
                "unsupported_parameter",
            ));
        }
        return Ok(None);
    }

    let unsupported = match surface {
        ApiSurface::ChatCompletions => [
            (
                "/chat_template_kwargs/thinking",
                "chat_template_kwargs.thinking",
            ),
            (
                "/chat_template_kwargs/enable_thinking",
                "chat_template_kwargs.enable_thinking",
            ),
            ("/chat_template_kwargs", "chat_template_kwargs"),
            ("/thinking", "thinking"),
            ("/reasoning", "reasoning"),
        ]
        .into_iter()
        .find(|(pointer, _)| body.pointer(pointer).is_some()),
        ApiSurface::Responses => [
            (
                "/chat_template_kwargs/thinking",
                "chat_template_kwargs.thinking",
            ),
            (
                "/chat_template_kwargs/enable_thinking",
                "chat_template_kwargs.enable_thinking",
            ),
            ("/chat_template_kwargs", "chat_template_kwargs"),
            ("/thinking", "thinking"),
            ("/reasoning_effort", "reasoning_effort"),
        ]
        .into_iter()
        .find(|(pointer, _)| body.pointer(pointer).is_some()),
        ApiSurface::Completions | ApiSurface::Other => None,
    };
    if let Some((_, param)) = unsupported {
        let canonical = surface
            .parameter_name()
            .expect("reasoning API surfaces have canonical parameters");
        return Err(ReasoningError::new(
            format!("Unsupported parameter '{param}'; use '{canonical}'."),
            Some(param),
            "unsupported_parameter",
        ));
    }

    let Some(canonical_path) = surface.canonical_path() else {
        return Ok(None);
    };
    let Some(value) = body.pointer(canonical_path) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let param = surface
        .parameter_name()
        .expect("canonical reasoning surfaces have parameter names");
    let value = value.as_str().ok_or_else(|| {
        ReasoningError::new(
            format!("Invalid type for '{param}'. Expected a string."),
            Some(param),
            "invalid_type",
        )
    })?;
    value
        .parse::<ReasoningEffort>()
        .map(Some)
        .map_err(|_| ReasoningError::invalid_effort(value, param))
}

fn validate_translation(translation: &ReasoningTranslation) -> Result<(), ReasoningError> {
    let segments = pointer_segments(&translation.target_path)?;
    let Some(root) = segments.first().map(String::as_str) else {
        return Err(invalid_config("target_path must not be empty"));
    };
    let allowed_path = match root {
        "reasoning_effort" => segments.len() == 1,
        "reasoning" | "thinking" => true,
        "chat_template_kwargs" => {
            segments.len() == 2 && matches!(segments[1].as_str(), "thinking" | "enable_thinking")
        }
        _ => false,
    };
    if !allowed_path {
        return Err(invalid_config(
            "target_path must address a reasoning-related request field",
        ));
    }
    if translation.values.is_empty() {
        return Err(invalid_config(
            "values must contain at least one effort mapping",
        ));
    }
    for value in translation.values.values() {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| invalid_config("mapped values must be valid JSON"))?;
        if bytes.len() > MAX_VALUE_BYTES {
            return Err(invalid_config("mapped values must not exceed 8192 bytes"));
        }
        if json_depth(value) > MAX_VALUE_DEPTH {
            return Err(invalid_config("mapped values must not exceed 8 levels"));
        }
    }
    Ok(())
}

fn pointer_segments(path: &str) -> Result<Vec<String>, ReasoningError> {
    if !path.starts_with('/') || path.len() == 1 {
        return Err(invalid_config(
            "target_path must be an absolute JSON pointer",
        ));
    }
    let mut segments = Vec::new();
    for segment in path[1..].split('/') {
        if segment.is_empty()
            || !segment.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            return Err(invalid_config(
                "target_path segments may only contain letters, numbers, underscores, and hyphens",
            ));
        }
        segments.push(segment.to_string());
    }
    if segments.len() > MAX_TARGET_DEPTH {
        return Err(invalid_config(
            "target_path must not exceed 8 path segments",
        ));
    }
    Ok(segments)
}

fn set_json_pointer(body: &mut Value, path: &str, value: Value) -> Result<(), ReasoningError> {
    let segments = pointer_segments(path)?;
    let mut current = body;
    for segment in &segments[..segments.len() - 1] {
        let object = current
            .as_object_mut()
            .ok_or_else(|| invalid_config("target_path crosses a non-object request field"))?;
        current = object
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let object = current
        .as_object_mut()
        .ok_or_else(|| invalid_config("target_path parent must be a JSON object"))?;
    object.insert(
        segments
            .last()
            .expect("validated JSON pointers contain a segment")
            .clone(),
        value,
    );
    Ok(())
}

fn remove_json_pointer(body: &mut Value, path: &str) {
    let Ok(segments) = pointer_segments(path) else {
        return;
    };
    let mut current = body;
    for segment in &segments[..segments.len() - 1] {
        let Some(next) = current
            .as_object_mut()
            .and_then(|object| object.get_mut(segment))
        else {
            return;
        };
        current = next;
    }
    if let Some(object) = current.as_object_mut()
        && let Some(last) = segments.last()
    {
        object.remove(last);
    }
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

fn invalid_config(message: &str) -> ReasoningError {
    ReasoningError::new(
        format!("Invalid reasoning translation config: {message}."),
        Some("reasoning_translation"),
        "invalid_translation_config",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sglang_config() -> ReasoningTranslationConfig {
        serde_json::from_value(json!({
            "chat_completions": {
                "target_path": "/chat_template_kwargs/thinking",
                "values": {
                    "none": false,
                    "low": true,
                    "medium": true,
                    "high": true
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn translates_chat_reasoning_effort_to_sglang_boolean() {
        let config = sglang_config();
        let mut body = json!({
            "model": "kimi-k2.5",
            "messages": [{"role": "user", "content": "Hello"}],
            "reasoning_effort": "none"
        });

        config.apply("/chat/completions", &mut body).unwrap();

        assert_eq!(
            body.pointer("/chat_template_kwargs/thinking"),
            Some(&json!(false))
        );
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn omitted_reasoning_effort_does_not_change_body() {
        let config = sglang_config();
        let mut body = json!({"model": "kimi-k2.5", "messages": []});
        let original = body.clone();

        config.apply("/chat/completions", &mut body).unwrap();

        assert_eq!(body, original);
    }

    #[test]
    fn supports_object_shaped_provider_values() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "target_path": "/thinking",
                "values": {
                    "none": {"type": "disabled"},
                    "high": {"type": "enabled"}
                }
            }
        }))
        .unwrap();
        let mut body = json!({"model": "model", "messages": [], "reasoning_effort": "high"});

        config.apply("/v1/chat/completions", &mut body).unwrap();

        assert_eq!(body["thinking"], json!({"type": "enabled"}));
    }

    #[test]
    fn rejects_effort_missing_from_provider_map() {
        let config = sglang_config();
        let mut body = json!({"model": "model", "messages": [], "reasoning_effort": "xhigh"});

        let error = config.apply("/chat/completions", &mut body).unwrap_err();

        assert_eq!(error.param(), Some("reasoning_effort"));
        assert_eq!(error.code(), "unsupported_value");
        assert!(error.message().contains("xhigh"));
    }

    #[test]
    fn rejects_unknown_canonical_effort() {
        let body = json!({"model": "model", "messages": [], "reasoning_effort": "ultra"});

        let error = validate_canonical_reasoning("/chat/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("reasoning_effort"));
        assert_eq!(error.code(), "invalid_value");
    }

    #[test]
    fn rejects_reasoning_on_legacy_completions() {
        let body = json!({"model": "model", "prompt": "Hello", "reasoning_effort": "low"});

        let error = validate_canonical_reasoning("/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("reasoning_effort"));
        assert_eq!(error.code(), "unsupported_parameter");
    }

    #[test]
    fn rejects_provider_reasoning_controls_on_legacy_completions() {
        let body = json!({"model": "model", "prompt": "Hello", "thinking": false});

        let error = validate_canonical_reasoning("/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("thinking"));
        assert_eq!(error.code(), "unsupported_parameter");
    }

    #[test]
    fn rejects_provider_native_chat_thinking_controls() {
        let body = json!({
            "model": "model",
            "messages": [],
            "chat_template_kwargs": {"thinking": false}
        });

        let error = validate_canonical_reasoning("/chat/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("chat_template_kwargs.thinking"));
        assert_eq!(error.code(), "unsupported_parameter");
        assert!(error.message().contains("reasoning_effort"));
    }

    #[test]
    fn rejects_chat_reasoning_provider_extension() {
        let body = json!({
            "model": "model",
            "messages": [],
            "reasoning": {"effort": "low"}
        });

        let error = validate_canonical_reasoning("/chat/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("reasoning"));
        assert_eq!(error.code(), "unsupported_parameter");
    }

    #[test]
    fn rejects_translation_paths_outside_reasoning_fields() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "target_path": "/messages/0/content",
                "values": {"none": "replaced"}
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert_eq!(error.code(), "invalid_translation_config");
        assert!(error.message().contains("target_path"));
    }

    #[test]
    fn rejects_non_reasoning_chat_template_kwargs_paths() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "target_path": "/chat_template_kwargs/template",
                "values": {"none": "replacement"}
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert_eq!(error.code(), "invalid_translation_config");
        assert!(error.message().contains("target_path"));
    }

    #[test]
    fn rejects_excessively_deep_target_paths() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "target_path": "/thinking/a/b/c/d/e/f/g/h",
                "values": {"none": false}
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert_eq!(error.code(), "invalid_translation_config");
        assert!(error.message().contains("target_path"));
    }
}
