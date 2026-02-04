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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Item {
    /// A message item
    #[serde(rename = "message")]
    Message(MessageItem),

    /// A function call item
    #[serde(rename = "function_call")]
    FunctionCall(FunctionCallItem),

    /// A function call output item
    #[serde(rename = "function_call_output")]
    FunctionCallOutput(FunctionCallOutputItem),

    /// A reasoning item
    #[serde(rename = "reasoning")]
    Reasoning(ReasoningItem),

    /// Unknown item type (for forward compatibility)
    #[serde(other)]
    Unknown,
}

/// A message item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageItem {
    /// Optional unique identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The role of the message author
    pub role: String,

    /// The message content
    pub content: MessageContent,

    /// Current status of this item
    #[serde(skip_serializing_if = "Option::is_none")]
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
        #[serde(skip_serializing_if = "Option::is_none")]
        annotations: Option<Vec<Annotation>>,
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
    /// Unique identifier for this call
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The ID to correlate with output
    pub call_id: String,

    /// Function name
    pub name: String,

    /// Function arguments as JSON string
    pub arguments: String,

    /// Current status of this item
    #[serde(skip_serializing_if = "Option::is_none")]
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
        #[serde(skip_serializing_if = "Option::is_none")]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesResponse {
    /// Unique identifier for this response
    pub id: String,

    /// Object type, always "response"
    pub object: String,

    /// Unix timestamp of creation
    pub created_at: u64,

    /// The model used
    pub model: String,

    /// Response status
    pub status: ResponseStatus,

    /// Output items
    pub output: Vec<Item>,

    /// Error information (if status is "failed")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,

    /// Incomplete status details
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_details: Option<IncompleteDetails>,

    /// Token usage
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ResponseUsage>,

    /// Metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,

    /// Additional fields
    #[serde(flatten)]
    pub extra: Option<serde_json::Value>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<InputTokensDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<OutputTokensDetails>,
}

/// Details about input tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
}

/// Details about output tokens
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputTokensDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
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
            model: "gpt-4o".to_string(),
            status: ResponseStatus::Completed,
            output: vec![Item::Message(MessageItem {
                id: Some("item_0".to_string()),
                role: "assistant".to_string(),
                content: MessageContent::Text("Hello!".to_string()),
                status: Some(ItemStatus::Completed),
            })],
            error: None,
            incomplete_details: None,
            usage: Some(ResponseUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                input_tokens_details: None,
                output_tokens_details: None,
            }),
            metadata: None,
            extra: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("resp_123"));
        assert!(json.contains("completed"));
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
