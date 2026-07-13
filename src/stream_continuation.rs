use axum::http::Method;
use bytes::Bytes;
use serde_json::Value;
use std::fmt;

use crate::target::StreamContinuationConfig;

/// State retained while forwarding an eligible completion stream.
pub struct CompletionContinuation {
    request: Value,
    prompt: String,
    generated_text: String,
    max_buffered_bytes: usize,
    continuable: bool,
    terminal: bool,
    id: Option<Value>,
    model: Option<Value>,
    created: Option<Value>,
}

/// The forwarded SSE event and whether the stream reached a terminal state.
pub struct EventObservation {
    pub event: Bytes,
    pub terminal: bool,
}

/// Errors raised while building a continuation request or rewriting an event.
#[derive(Debug)]
pub enum ContinuationError {
    NotContinuable,
    Serialize(serde_json::Error),
}

impl fmt::Display for ContinuationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotContinuable => write!(f, "completion stream cannot be continued"),
            Self::Serialize(_) => write!(f, "failed to serialize completion continuation data"),
        }
    }
}

impl std::error::Error for ContinuationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotContinuable => None,
            Self::Serialize(error) => Some(error),
        }
    }
}

impl CompletionContinuation {
    /// Creates protocol state when a request can be safely resumed from text output.
    pub fn from_request(
        path: &str,
        method: &Method,
        body: &[u8],
        config: &StreamContinuationConfig,
    ) -> Option<Self> {
        if path != "/v1/completions" || method != Method::POST || !config.enabled_for_path(path) {
            return None;
        }

        let request: Value = serde_json::from_slice(body).ok()?;
        let prompt = request.get("prompt")?.as_str()?.to_owned();
        let supported_n = match request.get("n") {
            None => true,
            Some(Value::Number(n)) => n.as_u64() == Some(1),
            Some(_) => false,
        };
        if request.get("stream") != Some(&Value::Bool(true))
            || !supported_n
            || !matches!(request.get("echo"), None | Some(Value::Bool(false)))
        {
            return None;
        }

        Some(Self {
            request,
            prompt,
            generated_text: String::new(),
            max_buffered_bytes: config.max_buffered_bytes,
            continuable: true,
            terminal: false,
            id: None,
            model: None,
            created: None,
        })
    }

    /// Records a complete SSE event and optionally normalizes its stream identity.
    pub fn observe_event(
        &mut self,
        event: &[u8],
        rewrite_identity: bool,
    ) -> Result<EventObservation, ContinuationError> {
        let Some(data) = event_data(event) else {
            return Ok(EventObservation {
                event: Bytes::copy_from_slice(event),
                terminal: self.terminal,
            });
        };

        if data == "[DONE]" {
            self.terminal = true;
            return Ok(EventObservation {
                event: Bytes::copy_from_slice(event),
                terminal: true,
            });
        }

        let Ok(mut completion) = serde_json::from_str::<Value>(&data) else {
            return Ok(EventObservation {
                event: Bytes::copy_from_slice(event),
                terminal: self.terminal,
            });
        };
        let (text, terminal) = match completion_event(&completion) {
            CompletionEvent::Unrecognized => {
                if let Some(choices) = completion.get("choices").and_then(Value::as_array) {
                    self.continuable = false;
                    self.terminal |= choice_finish_reason(choices);
                }
                return Ok(EventObservation {
                    event: Bytes::copy_from_slice(event),
                    terminal: self.terminal,
                });
            }
            CompletionEvent::Ambiguous { terminal } => {
                self.continuable = false;
                self.terminal |= terminal;
                return Ok(EventObservation {
                    event: Bytes::copy_from_slice(event),
                    terminal: self.terminal,
                });
            }
            CompletionEvent::Recognized { text, terminal } => (text, terminal),
        };

        if !rewrite_identity {
            self.capture_identity(&completion);
        }

        if let Some(text) = text {
            self.append_text(text);
        }
        if terminal {
            self.terminal = true;
        }

        let event = if rewrite_identity {
            self.rewrite_identity(&mut completion);
            Bytes::from(serialize_sse(&completion)?)
        } else {
            Bytes::copy_from_slice(event)
        };
        Ok(EventObservation {
            event,
            terminal: self.terminal,
        })
    }

    /// Returns whether an interrupted response may issue another completion request.
    pub fn is_continuable(&self) -> bool {
        self.continuable && !self.terminal
    }

    /// Builds the next request with the emitted text appended to its prompt.
    pub fn request_body(&self, model_override: Option<&str>) -> Result<Bytes, ContinuationError> {
        if !self.is_continuable() {
            return Err(ContinuationError::NotContinuable);
        }

        let mut request = self.request.clone();
        let request = request
            .as_object_mut()
            .expect("eligible continuation requests are JSON objects");
        request.insert(
            "prompt".to_owned(),
            Value::String(format!("{}{}", self.prompt, self.generated_text)),
        );
        if let Some(model) = model_override {
            request.insert("model".to_owned(), Value::String(model.to_owned()));
        }

        serde_json::to_vec(&request)
            .map(Bytes::from)
            .map_err(ContinuationError::Serialize)
    }

    fn append_text(&mut self, text: &str) {
        if !self.continuable {
            return;
        }
        if self.generated_text.len().saturating_add(text.len()) > self.max_buffered_bytes {
            self.continuable = false;
            metrics::counter!("onwards_stream_continuation_buffer_exhausted_total").increment(1);
            return;
        }
        self.generated_text.push_str(text);
    }

    fn capture_identity(&mut self, completion: &Value) {
        if self.id.is_none() {
            self.id = completion.get("id").cloned();
        }
        if self.model.is_none() {
            self.model = completion.get("model").cloned();
        }
        if self.created.is_none() {
            self.created = completion.get("created").cloned();
        }
    }

    fn rewrite_identity(&self, completion: &mut Value) {
        let Some(completion) = completion.as_object_mut() else {
            return;
        };
        if let Some(id) = &self.id {
            completion.insert("id".to_owned(), id.clone());
        }
        if let Some(model) = &self.model {
            completion.insert("model".to_owned(), model.clone());
        }
        if let Some(created) = &self.created {
            completion.insert("created".to_owned(), created.clone());
        }
    }
}

enum CompletionEvent<'a> {
    Recognized {
        text: Option<&'a str>,
        terminal: bool,
    },
    Ambiguous {
        terminal: bool,
    },
    Unrecognized,
}

fn completion_event(completion: &Value) -> CompletionEvent<'_> {
    let Some(object) = completion.as_object() else {
        return CompletionEvent::Unrecognized;
    };
    let Some(choices) = object.get("choices").and_then(Value::as_array) else {
        return choice_less_completion_metadata(completion)
            .then_some(CompletionEvent::Recognized {
                text: None,
                terminal: false,
            })
            .unwrap_or(CompletionEvent::Unrecognized);
    };

    if choices.len() != 1 {
        return CompletionEvent::Ambiguous {
            terminal: choice_finish_reason(choices),
        };
    }

    let Some(choice) = choices.first().and_then(Value::as_object) else {
        return CompletionEvent::Unrecognized;
    };
    let text = choice.get("text").and_then(Value::as_str);
    let finish_reason = choice
        .get("finish_reason")
        .is_some_and(|reason| !reason.is_null());
    let explicitly_completion =
        object.get("object").and_then(Value::as_str) == Some("text_completion");
    if text.is_none() && !(explicitly_completion && choice.contains_key("finish_reason")) {
        return CompletionEvent::Unrecognized;
    }

    CompletionEvent::Recognized {
        text,
        terminal: finish_reason,
    }
}

fn choice_less_completion_metadata(completion: &Value) -> bool {
    let Some(completion) = completion.as_object() else {
        return false;
    };
    completion.get("object").and_then(Value::as_str) == Some("text_completion")
        && completion.contains_key("id")
        && completion.contains_key("model")
        && completion.contains_key("created")
}

fn choice_finish_reason(choices: &[Value]) -> bool {
    choices.iter().any(|choice| {
        choice
            .get("finish_reason")
            .is_some_and(|reason| !reason.is_null())
    })
}

fn event_data(event: &[u8]) -> Option<String> {
    let mut data = Vec::new();
    let mut found_data = false;

    for line in event.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(value) = line.strip_prefix(b"data:") else {
            continue;
        };
        if found_data {
            data.push(b'\n');
        }
        found_data = true;
        data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
    }

    found_data.then(|| String::from_utf8(data).ok()).flatten()
}

fn serialize_sse(completion: &Value) -> Result<Vec<u8>, ContinuationError> {
    let data = serde_json::to_vec(completion).map_err(ContinuationError::Serialize)?;
    let mut event = Vec::with_capacity(b"data: \n\n".len() + data.len());
    event.extend_from_slice(b"data: ");
    event.extend_from_slice(&data);
    event.extend_from_slice(b"\n\n");
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::StreamContinuationConfig;
    use axum::http::Method;
    use serde_json::Value;

    fn eligible_state(max_buffered_bytes: usize) -> CompletionContinuation {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes,
            idle_timeout_ms: None,
        };
        CompletionContinuation::from_request(
            "/v1/completions",
            &Method::POST,
            br#"{"model":"requested-m","prompt":"Say hello: ","stream":true}"#,
            &config,
        )
        .unwrap()
    }

    #[test]
    fn accepts_only_supported_completion_requests() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };
        let eligible = br#"{"prompt":"hello","stream":true}"#;

        assert!(
            CompletionContinuation::from_request(
                "/v1/completions",
                &Method::POST,
                eligible,
                &config,
            )
            .is_some()
        );
        for (path, method, body) in [
            ("/v1/chat/completions", Method::POST, eligible.as_slice()),
            ("/v1/completions", Method::GET, eligible.as_slice()),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":["hello"],"stream":true}"#.as_slice(),
            ),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":"hello","stream":false}"#.as_slice(),
            ),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":"hello","stream":true,"n":2}"#.as_slice(),
            ),
            (
                "/v1/completions",
                Method::POST,
                br#"{"prompt":"hello","stream":true,"echo":true}"#.as_slice(),
            ),
        ] {
            assert!(CompletionContinuation::from_request(path, &method, body, &config).is_none());
        }
    }

    #[test]
    fn interrupted_completion_builds_prefix_request() {
        let mut state = eligible_state(1024);
        state
            .observe_event(
                br#"data: {"id":"cmpl-a","created":1,"model":"m","choices":[{"text":"hello","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        let body: Value =
            serde_json::from_slice(&state.request_body(Some("upstream-m")).unwrap()).unwrap();
        assert_eq!(body["prompt"], "Say hello: hello");
        assert_eq!(body["model"], "upstream-m");
    }

    #[test]
    fn terminal_events_prevent_continuation() {
        let mut state = eligible_state(1024);
        assert!(
            state
                .observe_event(b"data: [DONE]\n\n", false)
                .unwrap()
                .terminal
        );
        assert!(!state.is_continuable());

        let mut state = eligible_state(1024);
        assert!(
            state
                .observe_event(
                    br#"data: {"choices":[{"text":"","finish_reason":"stop"}]}

"#,
                    false,
                )
                .unwrap()
                .terminal
        );
        assert!(!state.is_continuable());
    }

    #[test]
    fn multiline_continuation_event_reuses_original_identity() {
        let mut state = eligible_state(1024);
        state
            .observe_event(
                br#"data: {"id":"cmpl-first","created":1,"model":"first","choices":[{"text":"one","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();

        let observation = state
            .observe_event(
                b"data: {\"id\":\"cmpl-next\",\"created\":2,\ndata: \"model\":\"next\",\"choices\":[{\"text\":\"two\",\"finish_reason\":null}]}\n\n",

                true,
            )
            .unwrap();

        let rewritten = String::from_utf8(observation.event.to_vec()).unwrap();
        assert_eq!(
            rewritten,
            "data: {\"choices\":[{\"finish_reason\":null,\"text\":\"two\"}],\"created\":1,\"id\":\"cmpl-first\",\"model\":\"first\"}\n\n"
        );
    }

    #[test]
    fn preserves_comments_and_malformed_events() {
        let mut state = eligible_state(1024);
        for event in [
            b": keepalive\n\n".as_slice(),
            b"data: {not-json}\n\n".as_slice(),
            b"event: ping\ndata: unknown\n\n".as_slice(),
        ] {
            let observation = state.observe_event(event, true).unwrap();
            assert_eq!(observation.event.as_ref(), event);
            assert!(!observation.terminal);
        }
    }

    #[test]
    fn multiple_choices_disable_continuation_without_appending_a_prefix() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"choices":[{"text":"one","finish_reason":null},{"text":"two","finish_reason":"stop"}]}

"#;
        let observation = state.observe_event(event, true).unwrap();

        assert_eq!(observation.event.as_ref(), event);
        assert!(observation.terminal);
        assert!(!state.is_continuable());
        assert!(state.generated_text.is_empty());
    }

    #[test]
    fn buffer_exhaustion_disables_continuation_without_retaining_new_text() {
        let mut state = eligible_state(5);
        state
            .observe_event(
                br#"data: {"choices":[{"text":"hello","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        state
            .observe_event(
                br#"data: {"choices":[{"text":"!","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();

        assert!(!state.is_continuable());
        assert!(state.request_body(None).is_err());
    }

    #[test]
    fn event_observation_exposes_original_event_when_no_identity_rewrite_is_requested() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"choices":[{"text":"hello","finish_reason":null}]}

"#;
        let observation = state.observe_event(event, false).unwrap();
        assert_eq!(observation.event.as_ref(), event);
        assert!(!observation.terminal);
    }

    #[test]
    fn unrecognized_choice_events_pass_through_and_disable_continuation() {
        for (event, terminal) in [
            (
                br#"data: {"id":"chat-next","choices":[{"delta":{"content":"two"},"finish_reason":"stop"}]}

"#
                .as_slice(),
                true,
            ),
            (
                br#"data: {"id":"empty-choice","choices":[{}]}

"#
                .as_slice(),
                false,
            ),
        ] {
            let mut state = eligible_state(1024);
            state
                .observe_event(
                    br#"data: {"id":"cmpl-first","created":1,"model":"first","choices":[{"text":"one","finish_reason":null}]}

"#,
                    false,
                )
                .unwrap();
            let observation = state.observe_event(event, true).unwrap();
            assert_eq!(observation.event.as_ref(), event);
            assert_eq!(observation.terminal, terminal);
            assert!(!state.is_continuable());
        }
    }

    #[test]
    fn unrecognized_json_without_choices_passes_through_without_disabling_continuation() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"id":"other","type":"notification","payload":{"value":1}}

"#;

        let observation = state.observe_event(event, true).unwrap();
        assert_eq!(observation.event.as_ref(), event);
        assert!(!observation.terminal);
        assert!(state.is_continuable());
    }

    #[test]
    fn continuation_events_do_not_fill_missing_initial_identity() {
        let mut state = eligible_state(1024);
        state
            .observe_event(
                br#"data: {"id":"cmpl-first","choices":[{"text":"one","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();

        for (model, created) in [("provider-a", 2), ("provider-b", 3)] {
            let event = format!(
                "data: {{\"id\":\"cmpl-next\",\"created\":{created},\"model\":\"{model}\",\"choices\":[{{\"text\":\"two\",\"finish_reason\":null}}]}}\n\n"
            );
            let observation = state.observe_event(event.as_bytes(), true).unwrap();
            let rewritten: Value =
                serde_json::from_slice(&observation.event[6..observation.event.len() - 2]).unwrap();
            assert_eq!(rewritten["id"], "cmpl-first");
            assert_eq!(rewritten["model"], model);
            assert_eq!(rewritten["created"], created);
        }
    }

    #[test]
    fn unambiguous_choice_less_completion_metadata_captures_initial_identity() {
        let mut state = eligible_state(1024);
        state
            .observe_event(
                br#"data: {"id":"cmpl-first","object":"text_completion","created":1,"model":"first"}

"#,
                false,
            )
            .unwrap();

        let observation = state
            .observe_event(
                br#"data: {"id":"cmpl-next","created":2,"model":"next","choices":[{"text":"two","finish_reason":null}]}

"#,
                true,
            )
            .unwrap();
        let rewritten: Value =
            serde_json::from_slice(&observation.event[6..observation.event.len() - 2]).unwrap();
        assert_eq!(rewritten["id"], "cmpl-first");
        assert_eq!(rewritten["model"], "first");
        assert_eq!(rewritten["created"], 1);
    }

    #[test]
    fn multibyte_text_at_the_buffer_boundary_is_retained_but_overflow_is_not() {
        let event = "data: {\"choices\":[{\"text\":\"éé\",\"finish_reason\":null}]}\n\n";

        let mut exact_boundary = eligible_state(4);
        exact_boundary
            .observe_event(event.as_bytes(), false)
            .unwrap();
        let body: Value =
            serde_json::from_slice(&exact_boundary.request_body(None).unwrap()).unwrap();
        assert_eq!(body["prompt"], "Say hello: éé");

        let mut overflow = eligible_state(3);
        overflow.observe_event(event.as_bytes(), false).unwrap();
        assert!(!overflow.is_continuable());
        assert!(overflow.generated_text.is_empty());
    }
}
