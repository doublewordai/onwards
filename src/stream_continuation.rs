use axum::{
    body::Body,
    http::{
        HeaderMap, Method,
        header::{CONTENT_ENCODING, CONTENT_TYPE},
    },
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::client::HttpClient;
use crate::handlers::{UpstreamRequestMetadata, build_upstream_request};
use crate::load_balancer::{ProviderPool, SelectionState};
use crate::sse::SseBufferedStream;
use crate::target::ConcurrencyGuard;
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
    identity_established: bool,
}

pub(crate) fn is_event_stream(headers: &HeaderMap) -> bool {
    let mut content_types = headers.get_all(CONTENT_TYPE).iter();
    let (Some(content_type), None) = (content_types.next(), content_types.next()) else {
        return false;
    };
    content_type
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/event-stream"))
}

pub(crate) fn has_identity_content_encoding(headers: &HeaderMap) -> bool {
    let mut encodings = headers.get_all(CONTENT_ENCODING).iter();
    match (encodings.next(), encodings.next()) {
        (None, None) => true,
        (Some(value), None) => value
            .to_str()
            .is_ok_and(|encoding| encoding.trim().eq_ignore_ascii_case("identity")),
        _ => false,
    }
}

/// The forwarded SSE event and whether the stream reached a terminal state.
pub struct EventObservation {
    pub event: Bytes,
    pub terminal: bool,
    pub done: bool,
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

        let fallback_model = request
            .get("model")
            .cloned()
            .unwrap_or_else(|| Value::String("unknown".to_string()));
        let fallback_created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Some(Self {
            request,
            prompt,
            generated_text: String::new(),
            max_buffered_bytes: config.max_buffered_bytes,
            continuable: true,
            terminal: false,
            id: Some(Value::String(format!("cmpl-{}", uuid::Uuid::new_v4()))),
            model: Some(fallback_model),
            created: Some(Value::from(fallback_created)),
            identity_established: false,
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
                done: false,
            });
        };

        if data == "[DONE]" {
            self.terminal = true;
            return Ok(EventObservation {
                event: Bytes::copy_from_slice(event),
                terminal: true,
                done: true,
            });
        }

        let Ok(mut completion) = serde_json::from_str::<Value>(&data) else {
            return Ok(EventObservation {
                event: Bytes::copy_from_slice(event),
                terminal: self.terminal,
                done: false,
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
                    done: false,
                });
            }
            CompletionEvent::Ambiguous { terminal } => {
                self.continuable = false;
                self.terminal |= terminal;
                return Ok(EventObservation {
                    event: Bytes::copy_from_slice(event),
                    terminal: self.terminal,
                    done: false,
                });
            }
            CompletionEvent::Recognized { text, terminal } => (text, terminal),
        };

        if !rewrite_identity && !self.identity_established {
            self.establish_identity(&completion);
        }

        if let Some(text) = text {
            self.append_text(text);
        }
        if terminal {
            self.terminal = true;
        }

        self.rewrite_identity(&mut completion);
        let event = Bytes::from(serialize_sse(&completion)?);
        Ok(EventObservation {
            event,
            terminal: self.terminal,
            done: false,
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

    fn establish_identity(&mut self, completion: &Value) {
        self.id = completion.get("id").cloned().or_else(|| self.id.take());
        self.model = completion
            .get("model")
            .cloned()
            .or_else(|| self.model.take());
        self.created = completion
            .get("created")
            .cloned()
            .or_else(|| self.created.take());
        self.identity_established = true;
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

enum StreamInterruption {
    Done,
    Eof,
    Body(std::io::Error),
    IdleTimeout,
}

impl StreamInterruption {
    fn into_error(self) -> Option<std::io::Error> {
        match self {
            Self::Done | Self::Eof => None,
            Self::Body(error) => Some(error),
            Self::IdleTimeout => Some(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upstream completion stream idle timeout",
            )),
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Eof => "eof",
            Self::Body(_) => "body_error",
            Self::IdleTimeout => "idle_timeout",
        }
    }
}

/// Combines an initial completion body with independently selected continuation bodies.
pub(crate) fn wrap_completion_stream<T>(
    initial_body: Body,
    initial_guard: ConcurrencyGuard,
    mut continuation: CompletionContinuation,
    config: StreamContinuationConfig,
    pool: ProviderPool,
    http_client: T,
    request_metadata: UpstreamRequestMetadata,
) -> Body
where
    T: HttpClient + Clone + Send + Sync + 'static,
{
    let fallback = pool.fallback().cloned().unwrap_or_default();
    let mut selection = SelectionState::new(config.max_attempts, fallback.with_replacement);
    let stream = async_stream::stream! {
        let mut current_body = initial_body;
        let mut current_guard = Some(initial_guard);
        let mut rewrite_identity = false;
        let mut continuation_attempt = 0_u32;
        let mut total_backoff_ms = 0_u64;

        loop {
            let mut events = SseBufferedStream::new(current_body.into_data_stream());
            let interruption = loop {
                let next = if let Some(timeout_ms) = config.idle_timeout_ms {
                    match tokio::time::timeout(Duration::from_millis(timeout_ms), events.next()).await {
                        Ok(next) => next,
                        Err(_) => break StreamInterruption::IdleTimeout,
                    }
                } else {
                    events.next().await
                };

                match next {
                    Some(Ok(event)) => {
                        let observation = match continuation.observe_event(&event, rewrite_identity) {
                            Ok(observation) => observation,
                            Err(error) => {
                                yield Err::<Bytes, std::io::Error>(std::io::Error::other(error));
                                return;
                            }
                        };
                        let done = observation.done;
                        yield Ok::<Bytes, std::io::Error>(observation.event);
                        if done {
                            break StreamInterruption::Done;
                        }
                    }
                    Some(Err(error)) => {
                        break StreamInterruption::Body(std::io::Error::other(error));
                    }
                    None => break StreamInterruption::Eof,
                }
            };

            drop(events);
            drop(current_guard.take());

            tracing::debug!(
                reason = interruption.reason(),
                attempt = continuation_attempt,
                "Completion stream ended"
            );

            if matches!(interruption, StreamInterruption::Done) {
                metrics::counter!(
                    "onwards_stream_continuation_terminal_total",
                    "reason" => interruption.reason()
                )
                .increment(1);
                break;
            }

            let final_error = interruption.into_error();
            if !continuation.is_continuable() {
                if let Some(error) = final_error {
                    yield Err::<Bytes, std::io::Error>(error);
                }
                break;
            }

            let mut next_body = None;
            let mut next_guard = None;
            loop {
                if continuation_attempt as usize >= config.max_attempts {
                    break;
                }
                let retry_index = continuation_attempt.saturating_add(1);
                if let Some(backoff) = fallback.backoff.as_ref() {
                    let delay = backoff.delay(retry_index);
                    let delay_ms = delay.as_millis() as u64;
                    let next_total = total_backoff_ms.saturating_add(delay_ms);
                    if fallback
                        .max_total_backoff_ms
                        .is_some_and(|max_total| next_total > max_total)
                    {
                        metrics::counter!(
                            "onwards_stream_continuation_failures_total",
                            "reason" => "backoff_budget"
                        )
                        .increment(1);
                        break;
                    }
                    total_backoff_ms = next_total;
                    tokio::time::sleep(delay).await;
                }

                let Some((_index, target, guard)) = pool.select_next(&mut selection) else {
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "selection_exhausted"
                    )
                    .increment(1);
                    break;
                };
                continuation_attempt = continuation_attempt.saturating_add(1);
                metrics::counter!("onwards_stream_continuation_attempts_total").increment(1);
                let target = target.clone();

                if target
                    .limiter
                    .as_ref()
                    .is_some_and(|limiter| limiter.check().is_err())
                {
                    drop(guard);
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "rate_limit"
                    )
                    .increment(1);
                    if pool.should_fallback_on_rate_limit() {
                        continue;
                    }
                    break;
                }

                let request_body = match continuation.request_body(target.onwards_model.as_deref()) {
                    Ok(body) => body,
                    Err(error) => {
                        drop(guard);
                        metrics::counter!(
                            "onwards_stream_continuation_failures_total",
                            "reason" => "request_body"
                        )
                        .increment(1);
                        tracing::error!(error = %error, "Failed to build completion continuation request");
                        break;
                    }
                };
                let (request, upstream_uri) = match build_upstream_request(
                    &target,
                    &request_metadata,
                    request_body,
                ) {
                    Ok(request) => request,
                    Err(_error) => {
                        drop(guard);
                        metrics::counter!(
                            "onwards_stream_continuation_failures_total",
                            "reason" => "request_build"
                        )
                        .increment(1);
                        tracing::error!(
                            "Failed to build completion continuation upstream request"
                        );
                        break;
                    }
                };

                tracing::debug!(
                    attempt = continuation_attempt,
                    upstream = %upstream_uri,
                    "Requesting completion stream continuation"
                );
                let response = if let Some(timeout_secs) = target.request_timeout_secs {
                    match tokio::time::timeout(
                        Duration::from_secs(timeout_secs),
                        http_client.request(request),
                    )
                    .await
                    {
                        Ok(response) => response,
                        Err(_) => {
                            drop(guard);
                            metrics::counter!(
                                "onwards_stream_continuation_failures_total",
                                "reason" => "header_timeout"
                            )
                            .increment(1);
                            continue;
                        }
                    }
                } else {
                    http_client.request(request).await
                };

                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        drop(guard);
                        metrics::counter!(
                            "onwards_stream_continuation_failures_total",
                            "reason" => "network_error"
                        )
                        .increment(1);
                        tracing::warn!(
                            upstream = %target.url,
                            error = %error,
                            "Completion continuation request failed"
                        );
                        continue;
                    }
                };
                let status = response.status().as_u16();
                if !(200..300).contains(&status) {
                    drop(response);
                    drop(guard);
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "status"
                    )
                    .increment(1);
                    if pool.should_fallback_on_status(status) {
                        continue;
                    }
                    break;
                }
                if !is_event_stream(response.headers()) {
                    drop(response);
                    drop(guard);
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "content_type"
                    )
                    .increment(1);
                    continue;
                }
                if !has_identity_content_encoding(response.headers()) {
                    drop(response);
                    drop(guard);
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "content_encoding"
                    )
                    .increment(1);
                    continue;
                }

                next_body = Some(response.into_body());
                next_guard = Some(guard);
                metrics::counter!("onwards_stream_continuation_resumptions_total").increment(1);
                break;
            }

            match (next_body, next_guard) {
                (Some(body), Some(guard)) => {
                    current_body = body;
                    current_guard = Some(guard);
                    rewrite_identity = true;
                }
                _ => {
                    if let Some(error) = final_error {
                        yield Err::<Bytes, std::io::Error>(error);
                    }
                    break;
                }
            }
        }
    };

    Body::from_stream(stream)
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

    if choices.is_empty() && choice_less_completion_metadata(completion) {
        return CompletionEvent::Recognized {
            text: None,
            terminal: false,
        };
    }

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
    fn initial_recognized_event_receives_stable_fallback_identity() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"choices":[{"text":"hello","finish_reason":null}]}

"#;
        let observation = state.observe_event(event, false).unwrap();
        let rewritten: Value =
            serde_json::from_slice(&observation.event[6..observation.event.len() - 2]).unwrap();
        assert!(rewritten["id"].as_str().unwrap().starts_with("cmpl-"));
        assert_eq!(rewritten["model"], "requested-m");
        assert!(rewritten["created"].is_u64());
        assert_eq!(rewritten["choices"][0]["text"], "hello");
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
    fn missing_initial_identity_is_stable_across_continuations() {
        let mut state = eligible_state(1024);
        let initial = state
            .observe_event(
                br#"data: {"id":"cmpl-first","choices":[{"text":"one","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        let initial: Value =
            serde_json::from_slice(&initial.event[6..initial.event.len() - 2]).unwrap();

        for (model, created) in [("provider-a", 2), ("provider-b", 3)] {
            let event = format!(
                "data: {{\"id\":\"cmpl-next\",\"created\":{created},\"model\":\"{model}\",\"choices\":[{{\"text\":\"two\",\"finish_reason\":null}}]}}\n\n"
            );
            let observation = state.observe_event(event.as_bytes(), true).unwrap();
            let rewritten: Value =
                serde_json::from_slice(&observation.event[6..observation.event.len() - 2]).unwrap();
            assert_eq!(rewritten["id"], "cmpl-first");
            assert_eq!(rewritten["model"], initial["model"]);
            assert_eq!(rewritten["created"], initial["created"]);
        }
    }

    #[test]
    fn response_representation_checks_are_exact_and_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "Text/Event-Stream; charset=utf-8".parse().unwrap(),
        );
        assert!(is_event_stream(&headers));
        assert!(has_identity_content_encoding(&headers));

        headers.insert(CONTENT_ENCODING, "IDENTITY".parse().unwrap());
        assert!(has_identity_content_encoding(&headers));

        headers.insert(
            "content-type",
            "application/x-text/event-streamish".parse().unwrap(),
        );
        headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        assert!(!is_event_stream(&headers));
        assert!(!has_identity_content_encoding(&headers));
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
