//! Open Responses Adapter
//!
//! This adapter implements full Open Responses semantics over any Chat Completions backend.
//! It handles:
//! - Items ↔ Messages conversion
//! - State management via ResponseStore trait
//! - Tool loop orchestration via ToolExecutor trait
//! - Streaming event synthesis

use super::schemas::chat_completions::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, Choice, FunctionCall,
    MessageContent, Tool as ChatTool, ToolCall, ToolChoice as ChatToolChoice,
};
use super::schemas::responses::{
    ContentPart, FunctionCallItem, Input, InputTokensDetails, Item, ItemStatus,
    MessageContent as ResponseMessageContent, MessageItem, OutputTokensDetails, ResponseStatus,
    ResponseUsage, ResponsesRequest, ResponsesResponse, TextConfig, TextFormat,
    Tool as ResponseTool, ToolChoice as ResponseToolChoice, TruncationStrategy,
};
use crate::traits::{ResponseStore, StoreError, ToolError, ToolExecutor};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{debug, info, warn};

/// The Open Responses adapter that bridges the Responses API to Chat Completions
pub struct OpenResponsesAdapter {
    store: Arc<dyn ResponseStore>,
    executor: Arc<dyn ToolExecutor>,
    max_tool_iterations: u32,
}

impl OpenResponsesAdapter {
    /// Create a new adapter with the given store and executor
    pub fn new(store: Arc<dyn ResponseStore>, executor: Arc<dyn ToolExecutor>) -> Self {
        Self {
            store,
            executor,
            max_tool_iterations: 10, // Default limit to prevent infinite loops
        }
    }

    /// Set the maximum number of tool iterations
    pub fn with_max_tool_iterations(mut self, max: u32) -> Self {
        self.max_tool_iterations = max;
        self
    }

    /// Convert a Responses request to a Chat Completions request
    pub async fn to_chat_request(
        &self,
        request: &ResponsesRequest,
    ) -> Result<ChatCompletionRequest, AdapterError> {
        // Get context from previous response if specified
        let mut messages = if let Some(ref prev_id) = request.previous_response_id {
            match self.store.get_context(prev_id).await {
                Ok(Some(context)) => {
                    // Parse the stored context as items and convert
                    let items: Vec<Item> = serde_json::from_value(context)
                        .map_err(|e| AdapterError::ContextError(e.to_string()))?;
                    items_to_messages(&items)?
                }
                Ok(None) => {
                    return Err(AdapterError::PreviousResponseNotFound(prev_id.clone()));
                }
                Err(e) => {
                    return Err(AdapterError::StoreError(e));
                }
            }
        } else {
            Vec::new()
        };

        // Add system message from instructions
        if let Some(ref instructions) = request.instructions {
            messages.insert(
                0,
                ChatMessage {
                    role: "system".to_string(),
                    content: Some(MessageContent::Text(instructions.clone())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    extra: None,
                },
            );
        }

        // Convert input to messages
        if let Some(ref input) = request.input {
            let input_messages = input_to_messages(input)?;
            messages.extend(input_messages);
        }

        // Convert tools
        let tools = request.tools.as_ref().map(|t| convert_tools(t));

        // Convert tool choice
        let tool_choice = request.tool_choice.as_ref().map(convert_tool_choice);

        Ok(ChatCompletionRequest {
            model: request.model.clone(),
            messages,
            temperature: request.temperature,
            top_p: request.top_p,
            n: None,
            stream: request.stream,
            stream_options: if request.stream == Some(true) {
                Some(super::schemas::chat_completions::StreamOptions {
                    include_usage: Some(true),
                })
            } else {
                None
            },
            stop: request.stop.clone().map(|s| match s {
                super::schemas::responses::StopSequence::Single(s) => {
                    super::schemas::chat_completions::StopSequence::Single(s)
                }
                super::schemas::responses::StopSequence::Multiple(v) => {
                    super::schemas::chat_completions::StopSequence::Multiple(v)
                }
            }),
            max_tokens: request.max_output_tokens,
            max_completion_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: request.user.clone(),
            seed: None,
            tools,
            tool_choice,
            parallel_tool_calls: request.parallel_tool_calls,
            response_format: None, // TODO: Convert text.format
            service_tier: None,
            extra: None,
        })
    }

    /// Convert a Chat Completions response to a Responses response
    ///
    /// Echoes back request parameters as required by the Open Responses spec.
    pub fn to_responses_response(
        &self,
        chat_response: &ChatCompletionResponse,
        request: &ResponsesRequest,
    ) -> ResponsesResponse {
        let output = chat_response
            .choices
            .iter()
            .flat_map(|choice| message_to_items(&choice.message, choice.finish_reason.as_deref()))
            .collect();

        let status = determine_response_status(&chat_response.choices);

        let completed_at = if status == ResponseStatus::Completed {
            Some(chat_response.created)
        } else {
            None
        };

        let tool_choice = request
            .tool_choice
            .as_ref()
            .and_then(|tc| serde_json::to_value(tc).ok())
            .unwrap_or(serde_json::Value::String("auto".to_string()));

        ResponsesResponse {
            id: format!("resp_{}", &chat_response.id),
            object: "response".to_string(),
            created_at: chat_response.created,
            completed_at,
            status,
            incomplete_details: None,
            model: request.model.clone(),
            previous_response_id: request.previous_response_id.clone(),
            instructions: request.instructions.clone(),
            output,
            error: None,
            tools: request.tools.clone().unwrap_or_default(),
            tool_choice,
            truncation: request
                .truncation
                .clone()
                .unwrap_or(TruncationStrategy::Disabled),
            parallel_tool_calls: request.parallel_tool_calls.unwrap_or(true),
            text: request.text.clone().unwrap_or(TextConfig {
                format: Some(TextFormat::Text),
            }),
            top_p: request.top_p.unwrap_or(1.0),
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            top_logprobs: 0,
            temperature: request.temperature.unwrap_or(1.0),
            reasoning: request.reasoning.clone(),
            usage: chat_response.usage.as_ref().map(|u| ResponseUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                input_tokens_details: InputTokensDetails { cached_tokens: 0 },
                output_tokens_details: OutputTokensDetails {
                    reasoning_tokens: 0,
                },
            }),
            max_output_tokens: request.max_output_tokens,
            max_tool_calls: None,
            store: request.store.unwrap_or(false),
            background: false,
            service_tier: chat_response
                .service_tier
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            metadata: request.metadata.clone(),
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }

    /// Store a response and return the stored response with ID
    pub async fn store_response(&self, response: &ResponsesResponse) -> Result<String, StoreError> {
        let value = serde_json::to_value(response)
            .map_err(|e| StoreError::SerializationError(e.to_string()))?;
        self.store.store(&value).await
    }

    /// Check if a response requires tool execution
    pub fn requires_tool_action(&self, response: &ChatCompletionResponse) -> bool {
        response
            .choices
            .first()
            .map(|c| c.finish_reason.as_deref() == Some("tool_calls"))
            .unwrap_or(false)
    }

    /// Extract tool calls from a response that require execution
    pub fn extract_tool_calls(&self, response: &ChatCompletionResponse) -> Vec<PendingToolCall> {
        response
            .choices
            .iter()
            .flat_map(|choice| {
                choice
                    .message
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls.iter().map(|tc| PendingToolCall {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                        })
                    })
                    .into_iter()
                    .flatten()
            })
            .collect()
    }

    /// Execute a tool call using the configured executor
    pub async fn execute_tool(
        &self,
        tool_call: &PendingToolCall,
    ) -> Result<ToolCallResult, ToolError> {
        // Check if the executor can handle this tool
        if !self.executor.can_handle(&tool_call.name) {
            debug!(
                tool_name = %tool_call.name,
                tool_call_id = %tool_call.id,
                "Tool not handled by executor, returning to client"
            );
            return Ok(ToolCallResult::Unhandled(tool_call.clone()));
        }

        // Parse arguments as JSON
        let args: serde_json::Value = serde_json::from_str(&tool_call.arguments)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;

        debug!(
            tool_name = %tool_call.name,
            tool_call_id = %tool_call.id,
            arguments = %tool_call.arguments,
            "Executing tool"
        );

        let start = std::time::Instant::now();

        // Execute the tool
        let result = self
            .executor
            .execute(&tool_call.name, &tool_call.id, &args)
            .await?;

        let duration = start.elapsed();
        let output = serde_json::to_string(&result).unwrap_or_else(|_| result.to_string());

        info!(
            tool_name = %tool_call.name,
            tool_call_id = %tool_call.id,
            duration_ms = duration.as_millis() as u64,
            output_len = output.len(),
            "Tool executed successfully"
        );

        Ok(ToolCallResult::Executed {
            call_id: tool_call.id.clone(),
            output,
        })
    }

    /// Execute all tool calls and return results
    pub async fn execute_tool_calls(&self, tool_calls: &[PendingToolCall]) -> Vec<ToolCallResult> {
        let mut results = Vec::new();
        for tc in tool_calls {
            match self.execute_tool(tc).await {
                Ok(result) => results.push(result),
                Err(e) => {
                    warn!(
                        tool_name = %tc.name,
                        tool_call_id = %tc.id,
                        error = %e,
                        "Tool execution failed"
                    );
                    results.push(ToolCallResult::Error {
                        call_id: tc.id.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }

        let executed = results
            .iter()
            .filter(|r| matches!(r, ToolCallResult::Executed { .. }))
            .count();
        let unhandled = results
            .iter()
            .filter(|r| matches!(r, ToolCallResult::Unhandled(_)))
            .count();
        let errors = results
            .iter()
            .filter(|r| matches!(r, ToolCallResult::Error { .. }))
            .count();

        debug!(
            total = tool_calls.len(),
            executed, unhandled, errors, "Tool calls batch complete"
        );

        results
    }

    /// Add tool results to messages for the next iteration
    pub fn add_tool_results_to_messages(
        &self,
        messages: &mut Vec<ChatMessage>,
        assistant_message: &ChatMessage,
        results: &[ToolCallResult],
    ) {
        // First add the assistant message with tool calls
        messages.push(assistant_message.clone());

        // Then add tool response messages
        for result in results {
            match result {
                ToolCallResult::Executed { call_id, output } => {
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: Some(MessageContent::Text(output.clone())),
                        name: None,
                        tool_calls: None,
                        tool_call_id: Some(call_id.clone()),
                        extra: None,
                    });
                }
                ToolCallResult::Error { call_id, error } => {
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: Some(MessageContent::Text(format!("Error: {}", error))),
                        name: None,
                        tool_calls: None,
                        tool_call_id: Some(call_id.clone()),
                        extra: None,
                    });
                }
                ToolCallResult::Unhandled(_) => {
                    // Unhandled tools are returned to the client, not added to messages
                }
            }
        }
    }

    /// Check if there are any unhandled tool calls that need client action
    pub fn has_unhandled_tools(&self, results: &[ToolCallResult]) -> bool {
        results
            .iter()
            .any(|r| matches!(r, ToolCallResult::Unhandled(_)))
    }

    /// Get the maximum number of tool iterations
    pub fn max_iterations(&self) -> u32 {
        self.max_tool_iterations
    }
}

/// A pending tool call extracted from the response
#[derive(Debug, Clone)]
pub struct PendingToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Result of executing a tool call
#[derive(Debug, Clone)]
pub enum ToolCallResult {
    /// Tool was executed successfully
    Executed { call_id: String, output: String },
    /// Tool execution failed
    Error { call_id: String, error: String },
    /// Tool was not handled (should be returned to client)
    Unhandled(PendingToolCall),
}

/// Errors that can occur during adaptation
#[derive(Debug, Clone)]
pub enum AdapterError {
    /// Previous response ID was specified but not found
    PreviousResponseNotFound(String),
    /// Error accessing the response store
    StoreError(StoreError),
    /// Error during context processing
    ContextError(String),
    /// Error during conversion
    ConversionError(String),
    /// Tool execution error
    ToolError(ToolError),
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::PreviousResponseNotFound(id) => {
                write!(f, "Previous response not found: {}", id)
            }
            AdapterError::StoreError(e) => write!(f, "Store error: {}", e),
            AdapterError::ContextError(msg) => write!(f, "Context error: {}", msg),
            AdapterError::ConversionError(msg) => write!(f, "Conversion error: {}", msg),
            AdapterError::ToolError(e) => write!(f, "Tool error: {}", e),
        }
    }
}

impl std::error::Error for AdapterError {}

/// Convert Responses API input to Chat Completions messages
fn input_to_messages(input: &Input) -> Result<Vec<ChatMessage>, AdapterError> {
    match input {
        Input::Text(text) => Ok(vec![ChatMessage {
            role: "user".to_string(),
            content: Some(MessageContent::Text(text.clone())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            extra: None,
        }]),
        Input::Items(items) => items_to_messages(items),
    }
}

/// Convert Responses API items to Chat Completions messages
fn items_to_messages(items: &[Item]) -> Result<Vec<ChatMessage>, AdapterError> {
    let mut messages = Vec::new();

    for item in items {
        match item {
            Item::Message(msg) => {
                messages.push(ChatMessage {
                    role: msg.role.clone(),
                    content: Some(convert_message_content(&msg.content)),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                    extra: None,
                });
            }
            Item::FunctionCall(call) => {
                // Function calls in items become assistant messages with tool_calls
                // We need to find the corresponding message or create one
                let tool_call = ToolCall {
                    id: call.call_id.clone(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                };

                // Check if the last message is an assistant message we can add to
                if let Some(last) = messages.last_mut() {
                    if last.role == "assistant" {
                        if let Some(ref mut calls) = last.tool_calls {
                            calls.push(tool_call);
                        } else {
                            last.tool_calls = Some(vec![tool_call]);
                        }
                        continue;
                    }
                }

                // Otherwise create a new assistant message
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: None,
                    name: None,
                    tool_calls: Some(vec![tool_call]),
                    tool_call_id: None,
                    extra: None,
                });
            }
            Item::FunctionCallOutput(output) => {
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(MessageContent::Text(output.output.clone())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: Some(output.call_id.clone()),
                    extra: None,
                });
            }
            Item::Reasoning(_) => {
                // Reasoning items don't map to Chat Completions messages
                // They're model-internal and can't be fed back
                debug!("Skipping reasoning item in conversion to messages");
            }
            Item::Unknown => {
                warn!("Unknown item type encountered during conversion");
            }
        }
    }

    Ok(messages)
}

/// Convert Responses message content to Chat Completions message content
fn convert_message_content(content: &ResponseMessageContent) -> MessageContent {
    match content {
        ResponseMessageContent::Text(text) => MessageContent::Text(text.clone()),
        ResponseMessageContent::Parts(parts) => {
            // Convert content parts
            let chat_parts: Vec<super::schemas::chat_completions::ContentPart> = parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::InputText { text } => {
                        Some(super::schemas::chat_completions::ContentPart::Text {
                            text: text.clone(),
                        })
                    }
                    ContentPart::OutputText { text, .. } => {
                        Some(super::schemas::chat_completions::ContentPart::Text {
                            text: text.clone(),
                        })
                    }
                    ContentPart::InputImage { image_url, detail } => {
                        image_url.as_ref().map(|url| {
                            super::schemas::chat_completions::ContentPart::ImageUrl {
                                image_url: super::schemas::chat_completions::ImageUrl {
                                    url: url.clone(),
                                    detail: detail.clone(),
                                },
                            }
                        })
                    }
                    ContentPart::InputFile { .. } => {
                        // Files can't be directly converted to Chat Completions
                        warn!("File input cannot be converted to Chat Completions format");
                        None
                    }
                    ContentPart::Refusal { refusal } => {
                        // Refusals become text for now
                        Some(super::schemas::chat_completions::ContentPart::Text {
                            text: refusal.clone(),
                        })
                    }
                })
                .collect();

            if chat_parts.is_empty() {
                MessageContent::Text(String::new())
            } else {
                MessageContent::Parts(chat_parts)
            }
        }
    }
}

/// Convert a Chat Completions message to Responses items
fn message_to_items(message: &ChatMessage, finish_reason: Option<&str>) -> Vec<Item> {
    let mut items = Vec::new();
    let status = match finish_reason {
        Some("stop") => Some(ItemStatus::Completed),
        Some("length") => Some(ItemStatus::Incomplete),
        _ => Some(ItemStatus::Completed),
    };

    // Add the message item
    if let Some(ref content) = message.content {
        let content_text = match content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(parts) => {
                // Concatenate text parts
                parts
                    .iter()
                    .filter_map(|p| match p {
                        super::schemas::chat_completions::ContentPart::Text { text } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            }
        };

        if !content_text.is_empty() {
            items.push(Item::Message(MessageItem {
                id: Some(generate_item_id()),
                role: message.role.clone(),
                content: ResponseMessageContent::Parts(vec![ContentPart::OutputText {
                    text: content_text,
                    annotations: vec![],
                    logprobs: vec![],
                }]),
                status,
            }));
        }
    }

    // Add function call items
    if let Some(ref tool_calls) = message.tool_calls {
        for call in tool_calls {
            items.push(Item::FunctionCall(FunctionCallItem {
                id: Some(generate_item_id()),
                call_id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
                status,
            }));
        }
    }

    items
}

/// Convert Responses tools to Chat Completions tools
fn convert_tools(tools: &[ResponseTool]) -> Vec<ChatTool> {
    tools
        .iter()
        .filter_map(|tool| match tool {
            ResponseTool::Function {
                name,
                description,
                parameters,
                strict,
            } => name.as_ref().map(|n| ChatTool {
                tool_type: "function".to_string(),
                function: super::schemas::chat_completions::FunctionDefinition {
                    name: n.clone(),
                    description: description.clone(),
                    parameters: parameters.clone(),
                    strict: *strict,
                },
            }),
            // Other tool types (code_interpreter, file_search, etc.) don't map to Chat Completions
            _ => {
                debug!("Skipping non-function tool type in conversion");
                None
            }
        })
        .collect()
}

/// Convert Responses tool choice to Chat Completions tool choice
fn convert_tool_choice(choice: &ResponseToolChoice) -> ChatToolChoice {
    match choice {
        ResponseToolChoice::Mode(mode) => ChatToolChoice::Mode(mode.clone()),
        ResponseToolChoice::Specific { tool_type, name } => {
            if let Some(n) = name {
                ChatToolChoice::Specific {
                    tool_type: tool_type.clone(),
                    function: super::schemas::chat_completions::ToolChoiceFunction {
                        name: n.clone(),
                    },
                }
            } else {
                ChatToolChoice::Mode("auto".to_string())
            }
        }
    }
}

/// Determine the response status from Chat Completions choices
fn determine_response_status(choices: &[Choice]) -> ResponseStatus {
    if choices.is_empty() {
        return ResponseStatus::Failed;
    }

    let first_choice = &choices[0];

    match first_choice.finish_reason.as_deref() {
        Some("stop") => ResponseStatus::Completed,
        Some("length") => ResponseStatus::Incomplete,
        Some("tool_calls") => ResponseStatus::RequiresAction,
        Some("content_filter") => ResponseStatus::Failed,
        _ => ResponseStatus::Completed,
    }
}

static ITEM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique item ID
fn generate_item_id() -> String {
    let count = ITEM_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("item_{:016x}", count)
}

#[cfg(test)]
mod tests {
    use super::super::schemas::responses::FunctionCallOutputItem;
    use super::*;
    use crate::traits::{NoOpResponseStore, NoOpToolExecutor};

    fn create_test_adapter() -> OpenResponsesAdapter {
        OpenResponsesAdapter::new(Arc::new(NoOpResponseStore), Arc::new(NoOpToolExecutor))
    }

    #[test]
    fn test_input_text_to_messages() {
        let input = Input::Text("Hello".to_string());
        let messages = input_to_messages(&input).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert!(matches!(
            messages[0].content,
            Some(MessageContent::Text(ref t)) if t == "Hello"
        ));
    }

    #[test]
    fn test_items_to_messages() {
        let items = vec![
            Item::Message(MessageItem {
                id: Some("msg_1".to_string()),
                role: "user".to_string(),
                content: ResponseMessageContent::Text("What's the weather?".to_string()),
                status: None,
            }),
            Item::FunctionCall(FunctionCallItem {
                id: Some("fc_1".to_string()),
                call_id: "call_123".to_string(),
                name: "get_weather".to_string(),
                arguments: r#"{"location": "Paris"}"#.to_string(),
                status: None,
            }),
            Item::FunctionCallOutput(FunctionCallOutputItem {
                id: Some("fco_1".to_string()),
                call_id: "call_123".to_string(),
                output: r#"{"temp": 72}"#.to_string(),
            }),
        ];

        let messages = items_to_messages(&items).unwrap();

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert!(messages[1].tool_calls.is_some());
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_call_id, Some("call_123".to_string()));
    }

    #[test]
    fn test_message_to_items() {
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: Some(MessageContent::Text("Hello!".to_string())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            extra: None,
        };

        let items = message_to_items(&message, Some("stop"));

        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Item::Message(_)));
        if let Item::Message(ref msg) = items[0] {
            assert_eq!(msg.status, Some(ItemStatus::Completed));
        }
    }

    #[test]
    fn test_message_with_tool_calls_to_items() {
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_123".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "get_weather".to_string(),
                    arguments: r#"{"location": "Paris"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
            extra: None,
        };

        let items = message_to_items(&message, Some("tool_calls"));

        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Item::FunctionCall(_)));
    }

    #[tokio::test]
    async fn test_adapter_simple_request() {
        let adapter = create_test_adapter();

        let request = ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: Some(Input::Text("Hello".to_string())),
            instructions: Some("Be helpful".to_string()),
            previous_response_id: None,
            store: None,
            metadata: None,
            temperature: Some(0.7),
            top_p: None,
            max_output_tokens: Some(100),
            stop: None,
            stream: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            truncation: None,
            user: None,
            reasoning: None,
            text: None,
            extra: None,
        };

        let chat_request = adapter.to_chat_request(&request).await.unwrap();

        assert_eq!(chat_request.model, "gpt-4o");
        assert_eq!(chat_request.messages.len(), 2); // system + user
        assert_eq!(chat_request.messages[0].role, "system");
        assert_eq!(chat_request.messages[1].role, "user");
        assert_eq!(chat_request.temperature, Some(0.7));
        assert_eq!(chat_request.max_tokens, Some(100));
    }

    #[test]
    fn test_determine_response_status() {
        let choices_stop = vec![Choice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text("Done".to_string())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
                extra: None,
            },
            finish_reason: Some("stop".to_string()),
            logprobs: None,
        }];

        assert_eq!(
            determine_response_status(&choices_stop),
            ResponseStatus::Completed
        );

        let choices_tool_calls = vec![Choice {
            index: 0,
            message: ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_calls: Some(vec![]),
                tool_call_id: None,
                extra: None,
            },
            finish_reason: Some("tool_calls".to_string()),
            logprobs: None,
        }];

        assert_eq!(
            determine_response_status(&choices_tool_calls),
            ResponseStatus::RequiresAction
        );
    }

    #[tokio::test]
    async fn test_stream_options_set_when_streaming() {
        let adapter = create_test_adapter();

        let request = ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: Some(Input::Text("Hello".to_string())),
            stream: Some(true),
            instructions: None,
            previous_response_id: None,
            store: None,
            metadata: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            stop: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            truncation: None,
            user: None,
            reasoning: None,
            text: None,
            extra: None,
        };

        let chat_request = adapter.to_chat_request(&request).await.unwrap();
        let opts = chat_request
            .stream_options
            .expect("stream_options should be set");
        assert_eq!(opts.include_usage, Some(true));
    }

    #[tokio::test]
    async fn test_stream_options_none_when_not_streaming() {
        let adapter = create_test_adapter();

        let request = ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: Some(Input::Text("Hello".to_string())),
            stream: None,
            instructions: None,
            previous_response_id: None,
            store: None,
            metadata: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            stop: None,
            tools: None,
            tool_choice: None,
            parallel_tool_calls: None,
            truncation: None,
            user: None,
            reasoning: None,
            text: None,
            extra: None,
        };

        let chat_request = adapter.to_chat_request(&request).await.unwrap();
        assert!(chat_request.stream_options.is_none());
    }

    #[test]
    fn test_generate_item_id_unique() {
        let ids: Vec<String> = (0..100).map(|_| generate_item_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "All generated IDs should be unique"
        );
    }
}
