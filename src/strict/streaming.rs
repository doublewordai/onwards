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
    ContentPart, ItemStatus, MessageContent, MessageItem, ResponseStatus, ResponseUsage,
    ResponsesResponse,
};
use serde::{Deserialize, Serialize};
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
    /// Current output items being built
    items: Vec<StreamingItem>,
    /// Sequence number for events
    sequence_number: u32,
    /// Whether we've seen the first chunk
    started: bool,
    /// Accumulated usage (from final chunk if include_usage is set)
    usage: Option<ResponseUsage>,
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

impl StreamingState {
    /// Create a new streaming state for a response
    pub fn new(model: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            response_id: format!("resp_{:016x}", timestamp),
            model: model.to_string(),
            created_at: timestamp,
            items: Vec::new(),
            sequence_number: 0,
            started: false,
            usage: None,
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
                input_tokens_details: None,
                output_tokens_details: None,
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

    /// Process a single choice from a chunk
    fn process_choice(&mut self, choice: &ChunkChoice) -> Vec<StreamingEvent> {
        let mut events = Vec::new();
        let index = choice.index as usize;

        // Ensure we have an item for this index
        while self.items.len() <= index {
            let item = StreamingItem {
                id: format!("item_{}", self.items.len()),
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
                        self.items[index].tool_calls[tc_index].arguments.push_str(args);
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
                response: ResponseCreatedData {
                    id: self.response_id.clone(),
                    object: "response".to_string(),
                    created_at: self.created_at,
                    model: self.model.clone(),
                    status: ResponseStatus::InProgress,
                },
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_item_added_event(&mut self, index: usize) -> StreamingEvent {
        let item = &self.items[index];
        StreamingEvent {
            event_type: "response.output_item.added".to_string(),
            data: StreamingEventData::OutputItemAdded {
                output_index: index as u32,
                item: OutputItemData {
                    id: item.id.clone(),
                    item_type: "message".to_string(),
                    role: if item.role.is_empty() {
                        "assistant".to_string()
                    } else {
                        item.role.clone()
                    },
                    status: ItemStatus::InProgress,
                },
            },
            sequence_number: self.next_sequence(),
        }
    }

    fn create_content_part_added_event(&mut self, index: usize) -> StreamingEvent {
        StreamingEvent {
            event_type: "response.content_part.added".to_string(),
            data: StreamingEventData::ContentPartAdded {
                item_id: self.items[index].id.clone(),
                output_index: index as u32,
                content_index: 0,
                part: ContentPartData {
                    part_type: "output_text".to_string(),
                    text: String::new(),
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
                output_index: index as u32,
                content_index: 0,
                delta: delta.to_string(),
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
                output_index: index as u32,
                content_index: 0,
                part: ContentPartData {
                    part_type: "output_text".to_string(),
                    text: item.content_text.clone(),
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
                output_index: index as u32,
                item: OutputItemData {
                    id: item.id.clone(),
                    item_type: "message".to_string(),
                    role: if item.role.is_empty() {
                        "assistant".to_string()
                    } else {
                        item.role.clone()
                    },
                    status: ItemStatus::Completed,
                },
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

    /// Build the final response object
    fn build_final_response(&self) -> ResponsesResponse {
        use super::schemas::responses::Item;

        let output: Vec<Item> = self
            .items
            .iter()
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
                        annotations: None,
                    }]),
                    status: Some(item.status),
                })
            })
            .collect();

        ResponsesResponse {
            id: self.response_id.clone(),
            object: "response".to_string(),
            created_at: self.created_at,
            model: self.model.clone(),
            status: ResponseStatus::Completed,
            output,
            error: None,
            incomplete_details: None,
            usage: self.usage.clone(),
            metadata: None,
            extra: None,
        }
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
        response: ResponseCreatedData,
    },
    OutputItemAdded {
        output_index: u32,
        item: OutputItemData,
    },
    ContentPartAdded {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: ContentPartData,
    },
    OutputTextDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
    },
    ContentPartDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: ContentPartData,
    },
    OutputItemDone {
        output_index: u32,
        item: OutputItemData,
    },
    ResponseCompleted {
        response: ResponsesResponse,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseCreatedData {
    pub id: String,
    pub object: String,
    pub created_at: u64,
    pub model: String,
    pub status: ResponseStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputItemData {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    pub role: String,
    pub status: ItemStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPartData {
    #[serde(rename = "type")]
    pub part_type: String,
    pub text: String,
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
    use crate::strict::schemas::chat_completions::{ChunkChoice, ChunkDelta};

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
        let mut state = StreamingState::new("gpt-4");

        let chunk = create_test_chunk("chunk_1", None, Some("assistant"), None);
        let events = state.process_chunk(&chunk);

        // Should emit response.created and output_item.added
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "response.created");
        assert_eq!(events[1].event_type, "response.output_item.added");
    }

    #[test]
    fn test_streaming_state_content_events() {
        let mut state = StreamingState::new("gpt-4");

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
        let mut state = StreamingState::new("gpt-4");

        // First chunk
        let chunk1 = create_test_chunk("chunk_1", Some("Hi"), Some("assistant"), None);
        state.process_chunk(&chunk1);

        // Final chunk with finish_reason
        let chunk2 = create_test_chunk("chunk_2", None, None, Some("stop"));
        let events = state.process_chunk(&chunk2);

        // Should emit content_part.done and output_item.done
        assert!(events
            .iter()
            .any(|e| e.event_type == "response.content_part.done"));
        assert!(events
            .iter()
            .any(|e| e.event_type == "response.output_item.done"));
    }

    #[test]
    fn test_streaming_state_finalize() {
        let mut state = StreamingState::new("gpt-4");

        let chunk = create_test_chunk("chunk_1", Some("Hello"), Some("assistant"), Some("stop"));
        state.process_chunk(&chunk);

        let events = state.finalize();

        // Should emit response.completed
        assert!(events
            .iter()
            .any(|e| e.event_type == "response.completed"));
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
        let mut state = StreamingState::new("gpt-4");

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
}
