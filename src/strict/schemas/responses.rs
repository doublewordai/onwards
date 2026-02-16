//! Open Responses API schemas
//!
//! These schemas match the Open Responses specification.
//! See: https://www.openresponses.org/specification

use serde::{Deserialize, Serialize};

/// Request body for POST /v1/responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    /// The model to use for completion
    pub model: String,

    /// The input to generate a response for - can be a string or array of items
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Input>,

    /// Instructions for the model (system prompt)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    /// Reference to a previous response for context continuation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,

    /// Whether to store this response for future reference
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,

    /// Metadata key-value pairs
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,

    /// Sampling temperature (0-2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Nucleus sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Maximum output tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,

    /// Stop sequences
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequence>,

    /// Whether to stream the response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Tools available to the model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,

    /// How to choose which tool to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    /// Parallel tool calls setting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,

    /// Context truncation strategy
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationStrategy>,

    /// User identifier for abuse tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Reasoning configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,

    /// Text generation configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextConfig>,

    /// Additional fields not explicitly modeled
    #[serde(flatten)]
    pub extra: Option<serde_json::Value>,
}

/// Input to the model - either a string or array of items
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Items(Vec<Item>),
}

/// An item in the input/output
#[derive(Debug, Clone)]
pub enum Item {
    /// A message item
    Message(MessageItem),

    /// A function call item
    FunctionCall(FunctionCallItem),

    /// A function call output item
    FunctionCallOutput(FunctionCallOutputItem),

    /// A reasoning item
    Reasoning(ReasoningItem),

    /// Unknown item type (for forward compatibility)
    /// Preserves the raw JSON to avoid data loss
    Unknown(serde_json::Value),
}

// Custom deserialization for Item to preserve unknown types
impl<'de> serde::Deserialize<'de> for Item {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // First deserialize to a generic Value
        let value = serde_json::Value::deserialize(deserializer)?;

        // Check the "type" field
        let item_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| serde::de::Error::missing_field("type"))?;

        // Match against known types
        match item_type {
            "message" => {
                let item: MessageItem = serde_json::from_value(value)
                    .map_err(serde::de::Error::custom)?;
                Ok(Item::Message(item))
            }
            "function_call" => {
                let item: FunctionCallItem = serde_json::from_value(value)
                    .map_err(serde::de::Error::custom)?;
                Ok(Item::FunctionCall(item))
            }
            "function_call_output" => {
                let item: FunctionCallOutputItem = serde_json::from_value(value)
                    .map_err(serde::de::Error::custom)?;
                Ok(Item::FunctionCallOutput(item))
            }
            "reasoning" => {
                let item: ReasoningItem = serde_json::from_value(value)
                    .map_err(serde::de::Error::custom)?;
                Ok(Item::Reasoning(item))
            }
            // Unknown type - preserve the entire raw value
            _ => Ok(Item::Unknown(value)),
        }
    }
}

// Custom serialization for Item to handle unknown types
impl serde::Serialize for Item {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Item::Message(msg) => {
                // Serialize with type tag
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::Value::String("message".to_string()));
                let value = serde_json::to_value(msg).map_err(serde::ser::Error::custom)?;
                if let serde_json::Value::Object(obj) = value {
                    map.extend(obj);
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            Item::FunctionCall(fc) => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::Value::String("function_call".to_string()));
                let value = serde_json::to_value(fc).map_err(serde::ser::Error::custom)?;
                if let serde_json::Value::Object(obj) = value {
                    map.extend(obj);
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            Item::FunctionCallOutput(fco) => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::Value::String("function_call_output".to_string()));
                let value = serde_json::to_value(fco).map_err(serde::ser::Error::custom)?;
                if let serde_json::Value::Object(obj) = value {
                    map.extend(obj);
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            Item::Reasoning(r) => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::Value::String("reasoning".to_string()));
                let value = serde_json::to_value(r).map_err(serde::ser::Error::custom)?;
                if let serde_json::Value::Object(obj) = value {
                    map.extend(obj);
                }
                serde_json::Value::Object(map).serialize(serializer)
            }
            // Unknown type - serialize the raw value as-is
            Item::Unknown(value) => value.serialize(serializer),
        }
    }
}

/// A message item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageItem {
    /// Unique identifier (required in responses, optional in requests)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The role of the message author
    pub role: String,

    /// The message content
    pub content: MessageContent,

    /// Current status of this item (required in responses, optional in requests)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// Message content - array of content parts
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

/// A content part in a message
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    /// Text content
    #[serde(rename = "input_text")]
    InputText { text: String },

    /// Output text content
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        #[serde(default)]
        annotations: Vec<Annotation>,
        #[serde(default)]
        logprobs: Vec<serde_json::Value>,
    },

    /// Input image content
    #[serde(rename = "input_image")]
    InputImage {
        #[serde(skip_serializing_if = "Option::is_none")]
        image_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },

    /// Input file content
    #[serde(rename = "input_file")]
    InputFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        file_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },

    /// Refusal content
    #[serde(rename = "refusal")]
    Refusal { refusal: String },
}

/// Annotation on output text
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    #[serde(rename = "type")]
    pub annotation_type: String,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

/// Function call item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallItem {
    /// Unique identifier (required in responses, optional in requests)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The ID to correlate with output
    pub call_id: String,

    /// Function name
    pub name: String,

    /// Function arguments as JSON string
    pub arguments: String,

    /// Current status of this item (required in responses, optional in requests)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// Function call output item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallOutputItem {
    /// Unique identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The call_id this output corresponds to
    pub call_id: String,

    /// The function output as a string
    pub output: String,
}

/// Reasoning item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningItem {
    /// Unique identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Plain text reasoning content (if not encrypted)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ReasoningContent>>,

    /// Encrypted reasoning content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,

    /// Summary of the reasoning
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<Vec<SummaryContent>>,

    /// Current status of this item
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
}

/// Reasoning content part
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ReasoningContent {
    #[serde(rename = "reasoning_text")]
    Text { text: String },
}

/// Summary content part
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SummaryContent {
    #[serde(rename = "summary_text")]
    Text { text: String },
}

/// Item status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// Model is currently emitting tokens
    InProgress,
    /// Token budget exhausted (terminal)
    Incomplete,
    /// Fully sampled (terminal)
    Completed,
}

/// Stop sequence
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopSequence {
    Single(String),
    Multiple(Vec<String>),
}

/// Tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Tool {
    #[serde(rename = "function")]
    Function {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parameters: Option<serde_json::Value>,
        /// Must always be present (boolean or null) per the spec
        strict: Option<bool>,
    },

    /// Code interpreter tool
    #[serde(rename = "code_interpreter")]
    CodeInterpreter {
        #[serde(skip_serializing_if = "Option::is_none")]
        container: Option<serde_json::Value>,
    },

    /// File search tool
    #[serde(rename = "file_search")]
    FileSearch {
        #[serde(skip_serializing_if = "Option::is_none")]
        vector_store_ids: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_num_results: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filters: Option<serde_json::Value>,
    },

    /// Web search tool
    #[serde(rename = "web_search_preview")]
    WebSearch {
        #[serde(skip_serializing_if = "Option::is_none")]
        user_location: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        search_context_size: Option<String>,
    },

    /// MCP tool
    #[serde(rename = "mcp")]
    Mcp {
        server_label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        server_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        require_approval: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_tools: Option<Vec<String>>,
    },

    /// Computer use tool
    #[serde(rename = "computer_use_preview")]
    ComputerUse {
        environment: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_width: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_height: Option<u32>,
    },
}

/// Tool choice specification
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(String), // "none", "auto", "required"
    Specific {
        #[serde(rename = "type")]
        tool_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// Truncation strategy
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationStrategy {
    Auto,
    Disabled,
}

/// Reasoning configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Text generation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<TextFormat>,
}

/// Text format configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TextFormat {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_object")]
    JsonObject,
    #[serde(rename = "json_schema")]
    JsonSchema {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        schema: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strict: Option<bool>,
    },
}

/// Response from POST /v1/responses
///
/// All fields are required by the Open Responses specification.
/// Nullable fields must serialize as `null`, not be omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesResponse {
    /// Unique identifier for this response
    pub id: String,

    /// Object type, always "response"
    pub object: String,

    /// Unix timestamp of creation
    pub created_at: u64,

    /// Unix timestamp of completion (null if not yet completed)
    pub completed_at: Option<u64>,

    /// Response status
    pub status: ResponseStatus,

    /// Details about why response is incomplete (null if complete)
    pub incomplete_details: Option<IncompleteDetails>,

    /// The model used
    pub model: String,

    /// Reference to previous response (null if none)
    pub previous_response_id: Option<String>,

    /// System instructions (null if none)
    pub instructions: Option<String>,

    /// Output items
    pub output: Vec<Item>,

    /// Error information (null if no error)
    pub error: Option<ResponseError>,

    /// Tools available during generation
    pub tools: Vec<Tool>,

    /// How the model chose which tool to use
    pub tool_choice: serde_json::Value,

    /// Context truncation strategy
    pub truncation: TruncationStrategy,

    /// Whether parallel tool calls were allowed
    pub parallel_tool_calls: bool,

    /// Text generation configuration
    pub text: TextConfig,

    /// Nucleus sampling parameter
    pub top_p: f32,

    /// Presence penalty
    pub presence_penalty: f32,

    /// Frequency penalty
    pub frequency_penalty: f32,

    /// Number of top logprobs returned
    pub top_logprobs: u32,

    /// Sampling temperature
    pub temperature: f32,

    /// Reasoning configuration (null if not used)
    pub reasoning: Option<ReasoningConfig>,

    /// Token usage (null if not available)
    pub usage: Option<ResponseUsage>,

    /// Maximum output tokens (null if not set)
    pub max_output_tokens: Option<u32>,

    /// Maximum tool calls (null if not set)
    pub max_tool_calls: Option<u32>,

    /// Whether this response was stored
    pub store: bool,

    /// Whether this request ran in background
    pub background: bool,

    /// Service tier used
    pub service_tier: String,

    /// Metadata
    pub metadata: Option<serde_json::Value>,

    /// Safety identifier (null if not set)
    pub safety_identifier: Option<String>,

    /// Prompt cache key (null if not set)
    pub prompt_cache_key: Option<String>,
}

/// Response status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    /// Response is being generated
    InProgress,
    /// Response completed successfully
    Completed,
    /// Response is incomplete (e.g., token limit)
    Incomplete,
    /// Response failed
    Failed,
    /// Requires tool action from client
    RequiresAction,
    /// Response was cancelled
    Cancelled,
}

/// Response error
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    #[serde(rename = "type")]
    pub error_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

/// Details about why response is incomplete
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncompleteDetails {
    pub reason: String,
}

/// Token usage for responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    pub input_tokens_details: InputTokensDetails,
    pub output_tokens_details: OutputTokensDetails,
}

/// Details about input tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputTokensDetails {
    pub cached_tokens: u32,
}

/// Details about output tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputTokensDetails {
    pub reasoning_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_simple_request() {
        let json = r#"{
            "model": "gpt-4o",
            "input": "Hello, how are you?"
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.model, "gpt-4o");
        assert!(matches!(request.input, Some(Input::Text(_))));
    }

    #[test]
    fn test_deserialize_request_with_items() {
        let json = r#"{
            "model": "gpt-4o",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": "What's the weather?"
                }
            ]
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(request.input, Some(Input::Items(_))));
    }

    #[test]
    fn test_deserialize_with_previous_response_id() {
        let json = r#"{
            "model": "gpt-4o",
            "previous_response_id": "resp_abc123",
            "input": "What about tomorrow?"
        }"#;

        let request: ResponsesRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            request.previous_response_id,
            Some("resp_abc123".to_string())
        );
    }

    #[test]
    fn test_deserialize_function_call_output_item() {
        let json = r#"{
            "type": "function_call_output",
            "call_id": "call_123",
            "output": "{\"temperature\": 72}"
        }"#;

        let item: Item = serde_json::from_str(json).unwrap();
        assert!(matches!(item, Item::FunctionCallOutput(_)));
    }

    #[test]
    fn test_serialize_response() {
        let response = ResponsesResponse {
            id: "resp_123".to_string(),
            object: "response".to_string(),
            created_at: 1234567890,
            completed_at: Some(1234567891),
            model: "gpt-4o".to_string(),
            status: ResponseStatus::Completed,
            incomplete_details: None,
            previous_response_id: None,
            instructions: None,
            output: vec![Item::Message(MessageItem {
                id: Some("item_0".to_string()),
                role: "assistant".to_string(),
                content: MessageContent::Text("Hello!".to_string()),
                status: Some(ItemStatus::Completed),
            })],
            error: None,
            tools: vec![],
            tool_choice: serde_json::Value::String("auto".to_string()),
            truncation: TruncationStrategy::Disabled,
            parallel_tool_calls: true,
            text: TextConfig {
                format: Some(TextFormat::Text),
            },
            top_p: 1.0,
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            top_logprobs: 0,
            temperature: 1.0,
            reasoning: None,
            usage: Some(ResponseUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                input_tokens_details: InputTokensDetails { cached_tokens: 0 },
                output_tokens_details: OutputTokensDetails {
                    reasoning_tokens: 0,
                },
            }),
            max_output_tokens: None,
            max_tool_calls: None,
            store: false,
            background: false,
            service_tier: "default".to_string(),
            metadata: None,
            safety_identifier: None,
            prompt_cache_key: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("resp_123"));
        assert!(json.contains("completed"));
        // Verify nullable fields are present as null
        assert!(json.contains("\"previous_response_id\":null"));
        assert!(json.contains("\"reasoning\":null"));
    }

    #[test]
    fn test_item_status_serialization() {
        assert_eq!(
            serde_json::to_string(&ItemStatus::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::to_string(&ItemStatus::Completed).unwrap(),
            "\"completed\""
        );
    }
}
