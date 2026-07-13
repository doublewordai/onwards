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
use crate::sse::{SseBufferedStream, SseFramingError, SseStreamError, event_data};
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
        .and_then(|value| {
            value
                .parse::<mime::Mime>()
                .ok()
                .map(|media_type| (value, media_type))
        })
        .is_some_and(|(raw_value, media_type)| {
            media_type.type_() == mime::TEXT
                && media_type
                    .subtype()
                    .as_str()
                    .eq_ignore_ascii_case("event-stream")
                && (!raw_value.contains(';') || media_type.params().next().is_some())
                && media_type
                    .params()
                    .all(|(_, value)| !value.as_str().is_empty())
        })
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
    pub safe: bool,
    pub accepted: bool,
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
        let request_object = request.as_object()?;
        const UNSUPPORTED_CONTROLS: &[&str] = &[
            "tools",
            "tool_choice",
            "functions",
            "function_call",
            "response_format",
            "json_schema",
            "grammar",
            "guided_json",
            "guided_regex",
            "guided_choice",
            "guided_grammar",
        ];
        if UNSUPPORTED_CONTROLS
            .iter()
            .any(|key| request_object.contains_key(*key))
        {
            return None;
        }
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
                safe: true,
                accepted: false,
            });
        };

        let Ok(data) = std::str::from_utf8(&data) else {
            return Ok(self.reject_event(event));
        };
        if data == "[DONE]" {
            self.terminal = true;
            return Ok(EventObservation {
                event: Bytes::copy_from_slice(event),
                terminal: true,
                done: true,
                safe: true,
                accepted: true,
            });
        }

        let Ok(mut completion) = serde_json::from_str::<Value>(data) else {
            return Ok(self.reject_event(event));
        };
        let (text, terminal) = match completion_event(&completion) {
            CompletionEvent::Unsafe => return Ok(self.reject_event(event)),
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
            safe: true,
            accepted: true,
        })
    }

    /// Returns whether an interrupted response may issue another completion request.
    pub fn is_continuable(&self) -> bool {
        self.continuable && !self.terminal
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
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

    fn reject_event(&mut self, event: &[u8]) -> EventObservation {
        self.continuable = false;
        EventObservation {
            event: Bytes::copy_from_slice(event),
            terminal: self.terminal,
            done: false,
            safe: false,
            accepted: false,
        }
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
    Framing(SseFramingError),
    IdleTimeout,
}

impl StreamInterruption {
    fn into_error(self) -> Option<std::io::Error> {
        match self {
            Self::Done | Self::Eof => None,
            Self::Body(error) => Some(error),
            Self::Framing(error) => Some(std::io::Error::other(error)),
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
            Self::Framing(_) => "framing_error",
            Self::IdleTimeout => "idle_timeout",
        }
    }

    fn exhausted_reason(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Eof => "exhausted_eof",
            Self::Body(_) => "exhausted_body_error",
            Self::Framing(_) => "framing_error",
            Self::IdleTimeout => "exhausted_idle_timeout",
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
        use tracing::Instrument;

        let mut current_body = initial_body;
        let mut current_guard = Some(initial_guard);
        let mut rewrite_identity = false;
        let mut awaiting_resumption_event = false;
        let mut finish_reason_recorded = false;
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
                        if rewrite_identity && !observation.safe {
                            metrics::counter!(
                                "onwards_stream_continuation_failures_total",
                                "reason" => "unsafe_continuation_event"
                            )
                            .increment(1);
                            yield Err::<Bytes, std::io::Error>(std::io::Error::other(
                                "unsafe completion event from continuation provider",
                            ));
                            return;
                        }
                        if !rewrite_identity && !observation.safe {
                            metrics::counter!(
                                "onwards_stream_continuation_failures_total",
                                "reason" => "unsafe_initial_event"
                            )
                            .increment(1);
                        }
                        if rewrite_identity && awaiting_resumption_event && observation.accepted {
                            awaiting_resumption_event = false;
                            metrics::counter!("onwards_stream_continuation_resumptions_total")
                                .increment(1);
                        }
                        if observation.terminal && !observation.done && !finish_reason_recorded {
                            finish_reason_recorded = true;
                            metrics::counter!(
                                "onwards_stream_continuation_terminal_total",
                                "reason" => "finish_reason"
                            )
                            .increment(1);
                        }
                        let done = observation.done;
                        yield Ok::<Bytes, std::io::Error>(observation.event);
                        if done {
                            break StreamInterruption::Done;
                        }
                    }
                    Some(Err(SseStreamError::Source(error))) => {
                        break StreamInterruption::Body(std::io::Error::other(error));
                    }
                    Some(Err(SseStreamError::Framing(error))) => {
                        break StreamInterruption::Framing(error);
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

            if matches!(interruption, StreamInterruption::Framing(_)) {
                metrics::counter!(
                    "onwards_stream_continuation_failures_total",
                    "reason" => "framing_error"
                )
                .increment(1);
                if let Some(error) = interruption.into_error() {
                    yield Err::<Bytes, std::io::Error>(error);
                }
                break;
            }

            if continuation.is_terminal() {
                break;
            }

            let exhausted_reason = interruption.exhausted_reason();
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
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => "max_attempts"
                    )
                    .increment(1);
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
                let attempt_span = tracing::info_span!(
                    "onwards.stream_continuation_attempt",
                    attempt = continuation_attempt,
                    outcome = tracing::field::Empty,
                    http.response.status_code = tracing::field::Empty,
                );

                if target
                    .limiter
                    .as_ref()
                    .is_some_and(|limiter| limiter.check().is_err())
                {
                    attempt_span.record("outcome", "rate_limit");
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
                let request_future = http_client
                    .request(request)
                    .instrument(attempt_span.clone());
                let response = if let Some(timeout_secs) = target.request_timeout_secs {
                    match tokio::time::timeout(
                        Duration::from_secs(timeout_secs),
                        request_future,
                    )
                    .await
                    {
                        Ok(response) => response,
                        Err(_) => {
                            attempt_span.record("outcome", "header_timeout");
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
                    request_future.await
                };

                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        attempt_span.record("outcome", "network_error");
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
                attempt_span.record("http.response.status_code", status);
                if !(200..300).contains(&status) {
                    attempt_span.record("outcome", "status");
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
                    attempt_span.record("outcome", "content_type");
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
                    attempt_span.record("outcome", "content_encoding");
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
                attempt_span.record("outcome", "headers_accepted");
                break;
            }

            match (next_body, next_guard) {
                (Some(body), Some(guard)) => {
                    current_body = body;
                    current_guard = Some(guard);
                    rewrite_identity = true;
                    awaiting_resumption_event = true;
                }
                _ => {
                    metrics::counter!(
                        "onwards_stream_continuation_failures_total",
                        "reason" => exhausted_reason
                    )
                    .increment(1);
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
    Unsafe,
}

fn completion_event(completion: &Value) -> CompletionEvent<'_> {
    let Some(object) = completion.as_object() else {
        return CompletionEvent::Unsafe;
    };
    if object.contains_key("error") {
        return CompletionEvent::Unsafe;
    }
    if object
        .get("object")
        .is_some_and(|value| value.as_str() != Some("text_completion"))
    {
        return CompletionEvent::Unsafe;
    }

    let Some(choices_value) = object.get("choices") else {
        return if choice_less_completion_metadata(completion) {
            CompletionEvent::Recognized {
                text: None,
                terminal: false,
            }
        } else {
            CompletionEvent::Unsafe
        };
    };
    let Some(choices) = choices_value.as_array() else {
        return CompletionEvent::Unsafe;
    };

    if choices.is_empty() && choice_less_completion_metadata(completion) {
        return CompletionEvent::Recognized {
            text: None,
            terminal: false,
        };
    }

    if choices.len() != 1 {
        return CompletionEvent::Unsafe;
    }

    let Some(choice) = choices.first().and_then(Value::as_object) else {
        return CompletionEvent::Unsafe;
    };
    const SUPPORTED_CHOICE_FIELDS: &[&str] = &["index", "text", "finish_reason", "logprobs"];
    if choice
        .keys()
        .any(|key| !SUPPORTED_CHOICE_FIELDS.contains(&key.as_str()))
        || choice.get("index").and_then(Value::as_u64) != Some(0)
    {
        return CompletionEvent::Unsafe;
    }
    let Some(text) = choice.get("text").and_then(Value::as_str) else {
        return CompletionEvent::Unsafe;
    };
    let terminal = match choice.get("finish_reason") {
        None | Some(Value::Null) => false,
        Some(Value::String(_)) => true,
        Some(_) => return CompletionEvent::Unsafe,
    };

    CompletionEvent::Recognized {
        text: Some(text),
        terminal,
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
    fn advanced_generation_controls_are_ineligible() {
        let config = StreamContinuationConfig {
            enabled: true,
            endpoints: vec!["/v1/completions".to_string()],
            max_attempts: 1,
            max_buffered_bytes: 1024,
            idle_timeout_ms: None,
        };

        for key in [
            "tools",
            "tool_choice",
            "functions",
            "function_call",
            "response_format",
            "json_schema",
            "grammar",
            "guided_json",
            "guided_regex",
            "guided_choice",
            "guided_grammar",
        ] {
            let mut request = serde_json::json!({
                "prompt": "hello",
                "stream": true
            });
            request[key] = serde_json::json!({"enabled": true});
            let body = serde_json::to_vec(&request).unwrap();

            assert!(
                CompletionContinuation::from_request(
                    "/v1/completions",
                    &Method::POST,
                    &body,
                    &config,
                )
                .is_none(),
                "{key} must disable continuation"
            );
        }
    }

    #[test]
    fn interrupted_completion_builds_prefix_request() {
        let mut state = eligible_state(1024);
        state
            .observe_event(
                br#"data: {"id":"cmpl-a","created":1,"model":"m","choices":[{"index":0,"text":"hello","finish_reason":null}]}

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
                    br#"data: {"choices":[{"index":0,"text":"","finish_reason":"stop"}]}

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
                br#"data: {"id":"cmpl-first","created":1,"model":"first","choices":[{"index":0,"text":"one","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();

        let observation = state
            .observe_event(
                b"data: {\"id\":\"cmpl-next\",\"created\":2,\ndata: \"model\":\"next\",\"choices\":[{\"index\":0,\"text\":\"two\",\"finish_reason\":null}]}\n\n",

                true,
            )
            .unwrap();

        let rewritten = String::from_utf8(observation.event.to_vec()).unwrap();
        assert_eq!(
            rewritten,
            "data: {\"choices\":[{\"finish_reason\":null,\"index\":0,\"text\":\"two\"}],\"created\":1,\"id\":\"cmpl-first\",\"model\":\"first\"}\n\n"
        );
    }

    #[test]
    fn comments_are_safe_but_malformed_events_disable_continuation() {
        let mut state = eligible_state(1024);
        let comment = b": keepalive\n\n";
        let observation = state.observe_event(comment, false).unwrap();
        assert_eq!(observation.event.as_ref(), comment);
        assert!(state.is_continuable());

        let malformed = b"data: {not-json}\n\n";
        let observation = state.observe_event(malformed, false).unwrap();
        assert_eq!(observation.event.as_ref(), malformed);
        assert!(!state.is_continuable());
    }

    #[test]
    fn multiple_choices_disable_continuation_without_appending_a_prefix() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"choices":[{"index":0,"text":"one","finish_reason":null},{"index":1,"text":"two","finish_reason":"stop"}]}

"#;
        let observation = state.observe_event(event, true).unwrap();

        assert_eq!(observation.event.as_ref(), event);
        assert!(!observation.terminal);
        assert!(!state.is_continuable());
        assert!(state.generated_text.is_empty());
    }

    #[test]
    fn buffer_exhaustion_disables_continuation_without_retaining_new_text() {
        let mut state = eligible_state(5);
        state
            .observe_event(
                br#"data: {"choices":[{"index":0,"text":"hello","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        state
            .observe_event(
                br#"data: {"choices":[{"index":0,"text":"!","finish_reason":null}]}

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
        let event = br#"data: {"choices":[{"index":0,"text":"hello","finish_reason":null}]}

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
        for event in [
            br#"data: {"id":"chat-next","choices":[{"index":0,"delta":{"content":"two"},"finish_reason":"stop"}]}

"#
            .as_slice(),
            br#"data: {"id":"empty-choice","choices":[{}]}

"#
            .as_slice(),
        ] {
            let mut state = eligible_state(1024);
            state
                .observe_event(
                br#"data: {"id":"cmpl-first","created":1,"model":"first","choices":[{"index":0,"text":"one","finish_reason":null}]}

"#,
                    false,
                )
                .unwrap();
            let observation = state.observe_event(event, true).unwrap();
            assert_eq!(observation.event.as_ref(), event);
            assert!(!observation.terminal, "unsafe finish reasons are not trusted");
            assert!(!state.is_continuable());
        }
    }

    #[test]
    fn unrecognized_json_without_choices_passes_through_and_disables_continuation() {
        let mut state = eligible_state(1024);
        let event = br#"data: {"id":"other","type":"notification","payload":{"value":1}}

"#;

        let observation = state.observe_event(event, true).unwrap();
        assert_eq!(observation.event.as_ref(), event);
        assert!(!observation.terminal);
        assert!(!state.is_continuable());
    }

    #[test]
    fn unsafe_completion_shapes_fail_closed_without_changing_initial_bytes() {
        for event in [
            b"data: {not-json}\n\n".as_slice(),
            br#"data: {"error":{"message":"failed"}}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":0,"text":"x","finish_reason":null}],"error":{"message":"failed"}}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":null}]}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":0,"message":{"content":"x"},"finish_reason":null}]}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":0,"text":"x","finish_reason":null,"tool_calls":[]}]}

"#
            .as_slice(),
            br#"data: {"choices":[{"text":"x","finish_reason":null}]}

"#
            .as_slice(),
            br#"data: {"choices":[{"index":1,"text":"x","finish_reason":null}]}

"#
            .as_slice(),
            br#"data: {"type":"notification","payload":{"value":1}}

"#
            .as_slice(),
        ] {
            let mut state = eligible_state(1024);
            let observation = state.observe_event(event, false).unwrap();

            assert_eq!(observation.event.as_ref(), event);
            assert!(!state.is_continuable(), "unsafe event: {event:?}");
        }
    }

    #[test]
    fn done_without_post_colon_space_and_bom_are_recognized() {
        let mut state = eligible_state(1024);
        let event = b"\xef\xbb\xbfdata:[DONE]\r\n\r\n";
        let observation = state.observe_event(event, false).unwrap();

        assert_eq!(observation.event.as_ref(), event);
        assert!(observation.done);
        assert!(observation.terminal);
        assert!(!state.is_continuable());
    }

    #[test]
    fn observation_flags_distinguish_comments_content_and_unsafe_events() {
        let mut state = eligible_state(1024);
        let comment = state.observe_event(b": ping\n\n", false).unwrap();
        assert!(comment.safe);
        assert!(!comment.accepted);

        let content = state
            .observe_event(
                b"data:{\"choices\":[{\"index\":0,\"text\":\"x\",\"finish_reason\":null}]}\n\n",
                false,
            )
            .unwrap();
        assert!(content.safe);
        assert!(content.accepted);

        let unsafe_event = state.observe_event(b"data:{not-json}\n\n", false).unwrap();
        assert!(!unsafe_event.safe);
        assert!(!unsafe_event.accepted);
    }

    #[test]
    fn missing_initial_identity_is_stable_across_continuations() {
        let mut state = eligible_state(1024);
        let initial = state
            .observe_event(
                br#"data: {"id":"cmpl-first","choices":[{"index":0,"text":"one","finish_reason":null}]}

"#,
                false,
            )
            .unwrap();
        let initial: Value =
            serde_json::from_slice(&initial.event[6..initial.event.len() - 2]).unwrap();

        for (model, created) in [("provider-a", 2), ("provider-b", 3)] {
            let event = format!(
                "data: {{\"id\":\"cmpl-next\",\"created\":{created},\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"text\":\"two\",\"finish_reason\":null}}]}}\n\n"
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
    fn event_stream_media_type_requires_one_fully_valid_mime_value() {
        for valid in [
            "text/event-stream",
            "Text/Event-Stream; charset=utf-8",
            "TEXT/EVENT-STREAM; Charset=\"utf-8\"",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, valid.parse().unwrap());
            assert!(is_event_stream(&headers), "valid MIME rejected: {valid}");
        }

        for invalid in [
            "text/event-stream;",
            "text/event-stream; charset",
            "text/event-stream; charset=",
            "text/event-stream, application/json",
            "text/event-stream; charset=utf-8, application/json",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, invalid.parse().unwrap());
            assert!(
                !is_event_stream(&headers),
                "invalid MIME accepted: {invalid}"
            );
        }

        let mut headers = HeaderMap::new();
        headers.append(CONTENT_TYPE, "text/event-stream".parse().unwrap());
        headers.append(CONTENT_TYPE, "text/event-stream".parse().unwrap());
        assert!(!is_event_stream(&headers));
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
                br#"data: {"id":"cmpl-next","created":2,"model":"next","choices":[{"index":0,"text":"two","finish_reason":null}]}

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
        let event =
            "data: {\"choices\":[{\"index\":0,\"text\":\"éé\",\"finish_reason\":null}]}\n\n";

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
