//! Streaming event synthesis for Open Responses
//!
//! This module transforms `chat.completion.chunk` SSE events from upstream providers
//! into Open Responses semantic streaming events.
//!
//! ## Event Sequence
//!
//! For each output item, the following events are generated in order:
//!
//! 1. `response.output_item.added` - Item created
//! 2. `response.content_part.added` - Content part started
//! 3. `response.output_text.delta` - Text deltas (repeating)
//! 4. `response.content_part.done` - Content part finished
//! 5. `response.output_item.done` - Item finished
//!
//! At the end of the response:
//! 6. `response.completed` - Full response complete

use super::schemas::chat_completions::{ChatCompletionChunk, ChunkChoice};
use super::schemas::responses::{
    ContentPart, Item, ItemStatus, MessageContent, MessageItem, ResponseStatus, ResponseUsage,
    ResponsesRequest, ResponsesResponse, TextConfig, TextFormat, TruncationStrategy,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// State machine for tracking streaming response state
#[derive(Debug, Clone)]
pub struct StreamingState {
    /// Unique response ID
    response_id: String,
    /// Model being used
    model: String,
    /// Creation timestamp
    created_at: u64,
    /// Current iteration's output items being built
    items: Vec<StreamingItem>,
    /// Items from previous tool-loop iterations (already emitted to client)
    completed_items: Vec<StreamingItem>,
    /// Offset added to item indices so IDs and output_index values are unique across iterations
    item_index_offset: usize,
    /// Sequence number for events
    sequence_number: u32,
    /// Whether we've seen the first chunk
    started: bool,
    /// Accumulated usage (from final chunk if include_usage is set)
    usage: Option<ResponseUsage>,
    /// Original request parameters (for echoing back in response.completed)
    request: ResponsesRequest,
}

/// A streaming item being built incrementally
#[derive(Debug, Clone)]
struct StreamingItem {
    id: String,
    role: String,
    content_text: String,
    tool_calls: Vec<StreamingToolCall>,
    status: ItemStatus,
    content_part_started: bool,
}

/// A streaming tool call being built
#[derive(Debug, Clone)]
struct StreamingToolCall {
    id: String,
    name: String,
    arguments: String,
}

static RESPONSE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl StreamingState {
    /// Create a new streaming state for a response
    pub fn new(request: &ResponsesRequest) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            response_id: format!(
                "resp_{:016x}",
                RESPONSE_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            model: request.model.clone(),
            created_at: timestamp,
            items: Vec::new(),
            completed_items: Vec::new(),
            item_index_offset: 0,
            sequence_number: 0,
            started: false,
            usage: None,
            request: request.clone(),
        }
    }

    /// Process a chat completion chunk and return semantic events
    pub fn process_chunk(&mut self, chunk: &ChatCompletionChunk) -> Vec<StreamingEvent> {
        let mut events = Vec::new();

        // Emit response.created on first chunk
        if !self.started {
            self.started = true;
            events.push(self.create_response_created_event());
        }

        // Process each choice in the chunk
        for choice in &chunk.choices {
            let choice_events = self.process_choice(choice);
            events.extend(choice_events);
        }

        // Store usage if present (typically in final chunk)
        if let Some(ref usage) = chunk.usage {
            self.usage = Some(ResponseUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                input_tokens_details: super::schemas::responses::InputTokensDetails {
                    cached_tokens: 0,
                },
                output_tokens_details: super::schemas::responses::OutputTokensDetails {
                    reasoning_tokens: 0,
                },
            });
        }

        // Check for finish_reason to emit done events
        for choice in &chunk.choices {
            if choice.finish_reason.is_some() {
                let done_events = self.finalize_item(choice.index as usize);
                events.extend(done_events);
            }
        }

        events
    }

    /// Finalize the response and emit completion event
    pub fn finalize(&mut self) -> Vec<StreamingEvent> {
        let mut events = Vec::new();

        // Finalize any remaining items
        for i in 0..self.items.len() {
            if self.items[i].status == ItemStatus::InProgress {
                let done_events = self.finalize_item(i);
                events.extend(done_events);
            }
        }

        // Emit response.completed
        events.push(self.create_response_completed_event());

        events
    }

    /// Extract tool calls accumulated during the current iteration.
    ///
    /// Returns `(call_id, function_name, arguments)` tuples.
    pub fn extract_tool_calls(&self) -> Vec<(String, String, String)> {
        self.items
            .iter()
            .flat_map(|item| &item.tool_calls)
            .filter(|tc| !tc.id.is_empty())
            .map(|tc| (tc.id.clone(), tc.name.clone(), tc.arguments.clone()))
            .collect()
    }

    /// Prepare the state machine for the next tool-loop iteration.
    ///
    /// Moves the current iteration's items into `completed_items` so that
    /// `build_final_response` can include all items across iterations, and
    /// advances the index offset so new items get unique IDs and output indices.
    pub fn prepare_next_iteration(&mut self) {
        self.item_index_offset += self.items.len();
        self.completed_items.append(&mut self.items);
    }

    /// Process a single choice from a chunk
    fn process_choice(&mut self, choice: &ChunkChoice) -> Vec<StreamingEvent> {
        let mut events = Vec::new();
        let index = choice.index as usize;

        // Ensure we have an item for this index
        while self.items.len() <= index {
            let item = StreamingItem {
                id: format!("item_{}", self.item_index_offset + self.items.len()),
                role: String::new(),
                content_text: String::new(),
                tool_calls: Vec::new(),
                status: ItemStatus::InProgress,
                content_part_started: false,
            };
            self.items.push(item);

            // Emit output_item.added for new items
            events.push(self.create_item_added_event(self.items.len() - 1));
        }

        // Process delta and collect what events to emit
        let delta = &choice.delta;
        let mut emit_content_part_added = false;
        let mut emit_text_delta: Option<String> = None;

        // Handle role (usually in first chunk)
        if let Some(ref role) = delta.role {
            self.items[index].role = role.clone();
        }

        // Handle content delta
        if let Some(ref content) = delta.content {
            if !self.items[index].content_part_started && !content.is_empty() {
                self.items[index].content_part_started = true;
                emit_content_part_added = true;
            }

            if !content.is_empty() {
                self.items[index].content_text.push_str(content);
                emit_text_delta = Some(content.clone());
            }
        }

        // Handle tool calls
        if let Some(ref tool_calls) = delta.tool_calls {
            for tc in tool_calls {
                let tc_index = tc.index as usize;

                // Ensure we have a tool call entry
                while self.items[index].tool_calls.len() <= tc_index {
                    self.items[index].tool_calls.push(StreamingToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                }

                // Update tool call fields
                if let Some(ref id) = tc.id {
                    self.items[index].tool_calls[tc_index].id = id.clone();
                }
                if let Some(ref func) = tc.function {
                    if let Some(ref name) = func.name {
                        self.items[index].tool_calls[tc_index].name = name.clone();
                    }
                    if let Some(ref args) = func.arguments {
                        self.items[index].tool_calls[tc_index]
                            .arguments
                            .push_str(args);
                    }
                }
            }
        }

        // Now emit collected events (no longer holding mutable borrow of items)
        if emit_content_part_added {
            events.push(self.create_content_part_added_event(index));
        }
        if let Some(ref delta_text) = emit_text_delta {
            events.push(self.create_text_delta_event(index, delta_text));
        }

        events
    }

    /// Finalize an item and emit done events
    fn finalize_item(&mut self, index: usize) -> Vec<StreamingEvent> {
        let mut events = Vec::new();

        if index < self.items.len() {
            let item = &mut self.items[index];

            if item.status == ItemStatus::InProgress {
                item.status = ItemStatus::Completed;

                // Emit content_part.done if we had content
                if item.content_part_started {
                    events.push(self.create_content_part_done_event(index));
                }

                // Emit output_item.done
                events.push(self.create_item_done_event(index));
            }
        }

        events
    }

    fn next_sequence(&mut self) -> u32 {
        let seq = self.sequence_number;
        self.sequence_number += 1;
        seq
    }

    fn create_response_created_event(&mut self) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.created".to_string(),
            data: StreamingEventData::ResponseCreated {
                response: self.build_response_snapshot(ResponseStatus::InProgress, vec![], None),
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_item_added_event(&mut self, index: usize) -> StreamingEvent {
        let item = &self.items[index];
        StreamingEvent {
            event_type: "response.output_item.added".to_string(),
            data: StreamingEventData::OutputItemAdded {
                output_index: (self.item_index_offset + index) as u32,
                item: Item::Message(MessageItem {
                    id: Some(item.id.clone()),
                    role: if item.role.is_empty() {
                        "assistant".to_string()
                    } else {
                        item.role.clone()
                    },
                    content: MessageContent::Parts(vec![]),
                    status: Some(ItemStatus::InProgress),
                }),
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_content_part_added_event(&mut self, index: usize) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.content_part.added".to_string(),
            data: StreamingEventData::ContentPartAdded {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                content_index: 0,
                part: ContentPart::OutputText {
                    text: String::new(),
                    annotations: vec![],
                    logprobs: vec![],
                },
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_text_delta_event(&mut self, index: usize, delta: &str) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.output_text.delta".to_string(),
            data: StreamingEventData::OutputTextDelta {
                item_id: self.items[index].id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                content_index: 0,
                delta: delta.to_string(),
                logprobs: vec![],
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_content_part_done_event(&mut self, index: usize) -> StreamingEvent {
        let item = &self.items[index];
        StreamingEvent {
            event_type: "response.content_part.done".to_string(),
            data: StreamingEventData::ContentPartDone {
                item_id: item.id.clone(),
                output_index: (self.item_index_offset + index) as u32,
                content_index: 0,
                part: ContentPart::OutputText {
                    text: item.content_text.clone(),
                    annotations: vec![],
                    logprobs: vec![],
                },
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_item_done_event(&mut self, index: usize) -> StreamingEvent {
        let item = &self.items[index];
        StreamingEvent {
            event_type: "response.output_item.done".to_string(),
            data: StreamingEventData::OutputItemDone {
                output_index: (self.item_index_offset + index) as u32,
                item: Item::Message(MessageItem {
                    id: Some(item.id.clone()),
                    role: if item.role.is_empty() {
                        "assistant".to_string()
                    } else {
                        item.role.clone()
                    },
                    content: MessageContent::Parts(vec![ContentPart::OutputText {
                        text: item.content_text.clone(),
                        annotations: vec![],
                        logprobs: vec![],
                    }]),
                    status: Some(ItemStatus::Completed),
                }),
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_response_completed_event(&mut self) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.completed".to_string(),
            data: StreamingEventData::ResponseCompleted {
                response: self.build_final_response(),
            },
            sequence_number: self.next_sequence(),
        }
    }

    /// Build a response snapshot with the given status, output, and usage
    fn build_response_snapshot(
        &self,
        status: ResponseStatus,
        output: Vec<Item>,
        usage: Option<ResponseUsage>,
    ) -> ResponsesResponse {
        let req = &self.request;

        let tool_choice = req
            .tool_choice
            .as_ref()
            .and_then(|tc| serde_json::to_value(tc).ok())
            .unwrap_or(serde_json::Value::String("auto".to_string()));

        let completed_at = if status == ResponseStatus::Completed {
            Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            )
        } else {
            None
        };

        ResponsesResponse {
            id: self.response_id.clone(),
            object: "response".to_string(),
            created_at: self.created_at,
            completed_at,
            status,
            incomplete_details: None,
            model: self.model.clone(),
            previous_response_id: req.previous_response_id.clone(),
            instructions: req.instructions.clone(),
            output,
            error: None,
            tools: req.tools.clone().unwrap_or_default(),
            tool_choice,
            truncation: req
                .truncation
                .clone()
                .unwrap_or(TruncationStrategy::Disabled),
            parallel_tool_calls: req.parallel_tool_calls.unwrap_or(true),
            text: req.text.clone().unwrap_or(TextConfig {
                format: Some(TextFormat::Text),
            }),
            top_p: req.top_p.unwrap_or(1.0),
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            top_logprobs: 0,
            temperature: req.temperature.unwrap_or(1.0),
            reasoning: req.reasoning.clone(),
            usage,
            max_output_tokens: req.max_output_tokens,
            max_tool_calls: None,
            store: req.store.unwrap_or(false),
            background: false,
            service_tier: "default".to_string(),
            metadata: req.metadata.clone(),
            safety_identifier: None,
            prompt_cache_key: None,
        }
    }

    /// Build the final response object
    fn build_final_response(&self) -> ResponsesResponse {
        let output: Vec<Item> = self
            .completed_items
            .iter()
            .chain(self.items.iter())
            .map(|item| {
                Item::Message(MessageItem {
                    id: Some(item.id.clone()),
                    role: if item.role.is_empty() {
                        "assistant".to_string()
                    } else {
                        item.role.clone()
                    },
                    content: MessageContent::Parts(vec![ContentPart::OutputText {
                        text: item.content_text.clone(),
                        annotations: vec![],
                        logprobs: vec![],
                    }]),
                    status: Some(item.status),
                })
            })
            .collect();

        self.build_response_snapshot(ResponseStatus::Completed, output, self.usage.clone())
    }
}

/// A semantic streaming event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingEvent {
    /// Event type (e.g., "response.output_text.delta")
    #[serde(rename = "type")]
    pub event_type: String,
    /// Event data
    #[serde(flatten)]
    pub data: StreamingEventData,
    /// Sequence number for ordering
    pub sequence_number: u32,
}

impl StreamingEvent {
    /// Format as SSE event
    pub fn to_sse(&self) -> String {
        let json = serde_json::to_string(&self).unwrap_or_default();
        format!("event: {}\ndata: {}\n\n", self.event_type, json)
    }
}

/// Event data variants
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamingEventData {
    ResponseCreated {
        response: ResponsesResponse,
    },
    OutputItemAdded {
        output_index: u32,
        item: Item,
    },
    ContentPartAdded {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: ContentPart,
    },
    OutputTextDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
        logprobs: Vec<serde_json::Value>,
    },
    ContentPartDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: ContentPart,
    },
    OutputItemDone {
        output_index: u32,
        item: Item,
    },
    ResponseCompleted {
        response: ResponsesResponse,
    },
}

/// Parse a chat.completion.chunk from SSE data line
pub fn parse_chat_chunk(data: &str) -> Option<ChatCompletionChunk> {
    if data.trim() == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strict::schemas::chat_completions::{
        ChunkChoice, ChunkDelta, ChunkFunctionCall, ChunkToolCall,
    };

    fn test_request(model: &str) -> ResponsesRequest {
        ResponsesRequest {
            model: model.to_string(),
            input: None,
            instructions: None,
            previous_response_id: None,
            store: None,
            metadata: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
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
        }
    }

    fn create_test_chunk(
        id: &str,
        content: Option<&str>,
        role: Option<&str>,
        finish_reason: Option<&str>,
    ) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: role.map(String::from),
                    content: content.map(String::from),
                    tool_calls: None,
                },
                finish_reason: finish_reason.map(String::from),
                logprobs: None,
            }],
            usage: None,
            system_fingerprint: None,
            service_tier: None,
        }
    }

    #[test]
    fn test_streaming_state_initial_events() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        let chunk = create_test_chunk("chunk_1", None, Some("assistant"), None);
        let events = state.process_chunk(&chunk);

        // Should emit response.created and output_item.added
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "response.created");
        assert_eq!(events[1].event_type, "response.output_item.added");
    }

    #[test]
    fn test_streaming_state_content_events() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First chunk with role
        let chunk1 = create_test_chunk("chunk_1", None, Some("assistant"), None);
        state.process_chunk(&chunk1);

        // Content chunk
        let chunk2 = create_test_chunk("chunk_2", Some("Hello"), None, None);
        let events = state.process_chunk(&chunk2);

        // Should emit content_part.added and output_text.delta
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "response.content_part.added");
        assert_eq!(events[1].event_type, "response.output_text.delta");
    }

    #[test]
    fn test_streaming_state_completion_events() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First chunk
        let chunk1 = create_test_chunk("chunk_1", Some("Hi"), Some("assistant"), None);
        state.process_chunk(&chunk1);

        // Final chunk with finish_reason
        let chunk2 = create_test_chunk("chunk_2", None, None, Some("stop"));
        let events = state.process_chunk(&chunk2);

        // Should emit content_part.done and output_item.done
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "response.content_part.done")
        );
        assert!(
            events
                .iter()
                .any(|e| e.event_type == "response.output_item.done")
        );
    }

    #[test]
    fn test_streaming_state_finalize() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        let chunk = create_test_chunk("chunk_1", Some("Hello"), Some("assistant"), Some("stop"));
        state.process_chunk(&chunk);

        let events = state.finalize();

        // Should emit response.completed
        assert!(events.iter().any(|e| e.event_type == "response.completed"));
    }

    #[test]
    fn test_event_sse_format() {
        let event = StreamingEvent {
            event_type: "response.output_text.delta".to_string(),
            data: StreamingEventData::OutputTextDelta {
                item_id: "item_0".to_string(),
                output_index: 0,
                content_index: 0,
                delta: "Hello".to_string(),
                logprobs: vec![],
            },
            sequence_number: 1,
        };

        let sse = event.to_sse();
        assert!(sse.starts_with("event: response.output_text.delta\n"));
        assert!(sse.contains("data:"));
    }

    #[test]
    fn test_parse_chat_chunk() {
        let json = r#"{"id":"chatcmpl-123","object":"chat.completion.chunk","created":1234567890,"model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;

        let chunk = parse_chat_chunk(json);
        assert!(chunk.is_some());

        let chunk = chunk.unwrap();
        assert_eq!(chunk.id, "chatcmpl-123");
    }

    #[test]
    fn test_parse_done_marker() {
        let chunk = parse_chat_chunk("[DONE]");
        assert!(chunk.is_none());
    }

    #[test]
    fn test_sequence_numbers_increase() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        let chunk1 = create_test_chunk("chunk_1", Some("Hello"), Some("assistant"), None);
        let events1 = state.process_chunk(&chunk1);

        let chunk2 = create_test_chunk("chunk_2", Some(" world"), None, None);
        let events2 = state.process_chunk(&chunk2);

        // Sequence numbers should be monotonically increasing
        let all_events: Vec<_> = events1.into_iter().chain(events2).collect();
        for i in 1..all_events.len() {
            assert!(all_events[i].sequence_number > all_events[i - 1].sequence_number);
        }
    }

    /// Helper: create a chunk with tool call deltas
    fn create_tool_call_chunk(
        id: &str,
        role: Option<&str>,
        tool_call_id: Option<&str>,
        tool_name: Option<&str>,
        tool_args: Option<&str>,
        finish_reason: Option<&str>,
    ) -> ChatCompletionChunk {
        let tool_calls = if tool_call_id.is_some() || tool_name.is_some() || tool_args.is_some() {
            Some(vec![ChunkToolCall {
                index: 0,
                id: tool_call_id.map(String::from),
                call_type: tool_call_id.map(|_| "function".to_string()),
                function: if tool_name.is_some() || tool_args.is_some() {
                    Some(ChunkFunctionCall {
                        name: tool_name.map(String::from),
                        arguments: tool_args.map(String::from),
                    })
                } else {
                    None
                },
            }])
        } else {
            None
        };

        ChatCompletionChunk {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created: 1234567890,
            model: "gpt-4".to_string(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: role.map(String::from),
                    content: None,
                    tool_calls,
                },
                finish_reason: finish_reason.map(String::from),
                logprobs: None,
            }],
            usage: None,
            system_fingerprint: None,
            service_tier: None,
        }
    }

    #[test]
    fn test_extract_tool_calls() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // Stream a tool call: role, then tool call start, then arguments, then finish
        state.process_chunk(&create_tool_call_chunk(
            "c1",
            Some("assistant"),
            None,
            None,
            None,
            None,
        ));
        state.process_chunk(&create_tool_call_chunk(
            "c2",
            None,
            Some("call_abc"),
            Some("get_weather"),
            Some(r#"{"loc"#),
            None,
        ));
        state.process_chunk(&create_tool_call_chunk(
            "c3",
            None,
            None,
            None,
            Some(r#"ation":"Paris"}"#),
            None,
        ));
        state.process_chunk(&create_tool_call_chunk(
            "c4",
            None,
            None,
            None,
            None,
            Some("tool_calls"),
        ));

        let tool_calls = state.extract_tool_calls();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].0, "call_abc");
        assert_eq!(tool_calls[0].1, "get_weather");
        assert_eq!(tool_calls[0].2, r#"{"location":"Paris"}"#);
    }

    #[test]
    fn test_extract_tool_calls_empty_when_no_tools() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // Plain text stream with no tool calls
        state.process_chunk(&create_test_chunk(
            "c1",
            Some("Hi"),
            Some("assistant"),
            None,
        ));
        state.process_chunk(&create_test_chunk("c2", None, None, Some("stop")));

        let tool_calls = state.extract_tool_calls();
        assert!(tool_calls.is_empty());
    }

    #[test]
    fn test_prepare_next_iteration() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First iteration: one item
        state.process_chunk(&create_test_chunk(
            "c1",
            Some("Hi"),
            Some("assistant"),
            Some("stop"),
        ));

        assert_eq!(state.items.len(), 1);
        assert_eq!(state.completed_items.len(), 0);
        assert_eq!(state.item_index_offset, 0);

        state.prepare_next_iteration();

        assert_eq!(state.items.len(), 0);
        assert_eq!(state.completed_items.len(), 1);
        assert_eq!(state.item_index_offset, 1);
    }

    #[test]
    fn test_multi_iteration_item_ids_are_unique() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First iteration
        state.process_chunk(&create_test_chunk(
            "c1",
            Some("call result"),
            Some("assistant"),
            Some("tool_calls"),
        ));

        // Collect first iteration item IDs
        let first_item_ids: Vec<String> = state.items.iter().map(|i| i.id.clone()).collect();

        state.prepare_next_iteration();

        // Second iteration — choice.index is 0 again
        state.process_chunk(&create_test_chunk(
            "c2",
            Some("Final answer"),
            Some("assistant"),
            Some("stop"),
        ));

        let second_item_ids: Vec<String> = state.items.iter().map(|i| i.id.clone()).collect();

        // IDs must not overlap
        for id in &first_item_ids {
            assert!(
                !second_item_ids.contains(id),
                "ID collision across iterations: {id}"
            );
        }

        // Specifically: first iteration gets item_0, second gets item_1
        assert_eq!(first_item_ids, vec!["item_0"]);
        assert_eq!(second_item_ids, vec!["item_1"]);
    }

    #[test]
    fn test_multi_iteration_output_indices() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First iteration
        let events1 = state.process_chunk(&create_test_chunk(
            "c1",
            Some("Hi"),
            Some("assistant"),
            None,
        ));

        // Find the output_item.added event and check its index
        let added_event_1 = events1
            .iter()
            .find(|e| e.event_type == "response.output_item.added")
            .unwrap();
        if let StreamingEventData::OutputItemAdded { output_index, .. } = &added_event_1.data {
            assert_eq!(*output_index, 0);
        } else {
            panic!("Expected OutputItemAdded");
        }

        state.process_chunk(&create_test_chunk("c2", None, None, Some("stop")));
        state.prepare_next_iteration();

        // Second iteration — should get output_index 1
        let events2 = state.process_chunk(&create_test_chunk(
            "c3",
            Some("World"),
            Some("assistant"),
            None,
        ));

        let added_event_2 = events2
            .iter()
            .find(|e| e.event_type == "response.output_item.added")
            .unwrap();
        if let StreamingEventData::OutputItemAdded { output_index, .. } = &added_event_2.data {
            assert_eq!(*output_index, 1);
        } else {
            panic!("Expected OutputItemAdded");
        }
    }

    #[test]
    fn test_multi_iteration_final_response_includes_all_items() {
        let mut state = StreamingState::new(&test_request("gpt-4"));

        // First iteration
        state.process_chunk(&create_test_chunk(
            "c1",
            Some("thinking..."),
            Some("assistant"),
            Some("tool_calls"),
        ));
        state.prepare_next_iteration();

        // Second iteration
        state.process_chunk(&create_test_chunk(
            "c2",
            Some("The answer is 42."),
            Some("assistant"),
            Some("stop"),
        ));

        let final_events = state.finalize();
        let completed = final_events
            .iter()
            .find(|e| e.event_type == "response.completed")
            .expect("Should have response.completed event");

        if let StreamingEventData::ResponseCompleted { response } = &completed.data {
            // Should have items from both iterations
            assert_eq!(
                response.output.len(),
                2,
                "Final response should contain items from both iterations"
            );
        } else {
            panic!("Expected ResponseCompleted");
        }
    }
}
