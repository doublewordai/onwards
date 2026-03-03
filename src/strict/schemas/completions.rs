//! Legacy Completions API schemas
//!
//! These schemas match the OpenAI Completions API specification (legacy text completions).
//! See: https://platform.openai.com/docs/api-reference/completions
//!
//! In strict mode, completions requests are validated against the typed schema,
//! forwarded to the upstream `/v1/completions` endpoint, and the response is
//! sanitized (unknown fields stripped, model field rewritten).

use serde::{Deserialize, Serialize};

use super::chat_completions::{StopSequence, Usage};

/// Prompt input — string, array of strings, or token arrays (rejected at handler level)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompletionPrompt {
    Single(String),
    Multiple(Vec<String>),
    /// Token-based prompts (number[] or number[][]); not supported in adapter mode
    Tokens(Vec<serde_json::Value>),
}

/// Request body for POST /v1/completions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// The model to use for completion
    pub model: String,

    /// The prompt to generate completions for
    pub prompt: CompletionPrompt,

    /// Text to append after the completion (fill-in-the-middle); ignored in adapter mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,

    /// Maximum tokens to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Sampling temperature (0–2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Nucleus sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Number of completions to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,

    /// Whether to stream the response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Include log probabilities (0–5); not supported in adapter mode, ignored
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<u32>,

    /// Echo the prompt in the response; not supported in adapter mode, ignored
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo: Option<bool>,

    /// Stop sequences
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequence>,

    /// Presence penalty (−2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,

    /// Frequency penalty (−2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,

    /// Generate best_of completions server-side; not supported in adapter mode, ignored
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_of: Option<u32>,

    /// Logit bias for tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<serde_json::Value>,

    /// User identifier for abuse tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Random seed for deterministic sampling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
}

/// Response from POST /v1/completions (non-streaming)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
}

/// A single completion choice
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChoice {
    pub text: String,
    pub index: u32,
    pub logprobs: Option<serde_json::Value>,
    pub finish_reason: Option<String>,
}

/// Streaming chunk from POST /v1/completions with stream=true
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChunkChoice>,
}

/// A single choice within a streaming completion chunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChunkChoice {
    pub text: String,
    pub index: u32,
    pub logprobs: Option<serde_json::Value>,
    pub finish_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_string_prompt() {
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": "Say hello"}"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.prompt, CompletionPrompt::Single(_)));
    }

    #[test]
    fn test_deserialize_array_of_strings_prompt() {
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": ["Hello", "World"]}"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.prompt, CompletionPrompt::Multiple(_)));
    }

    #[test]
    fn test_deserialize_token_array_prompt() {
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": [1, 2, 3]}"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.prompt, CompletionPrompt::Tokens(_)));
    }

    #[test]
    fn test_deserialize_with_all_fields() {
        let json = r#"{
            "model": "gpt-3.5-turbo-instruct",
            "prompt": "Complete this",
            "suffix": "end",
            "max_tokens": 100,
            "temperature": 0.7,
            "top_p": 0.9,
            "n": 1,
            "stream": false,
            "logprobs": 3,
            "echo": true,
            "stop": "\n",
            "presence_penalty": 0.1,
            "frequency_penalty": 0.2,
            "best_of": 3,
            "user": "user-123",
            "seed": 42
        }"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "gpt-3.5-turbo-instruct");
        assert_eq!(req.max_tokens, Some(100));
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.logprobs, Some(3));
        assert_eq!(req.echo, Some(true));
        assert_eq!(req.best_of, Some(3));
        assert_eq!(req.seed, Some(42));
    }
}
