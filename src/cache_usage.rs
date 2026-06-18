//! OpenAI-shaped request sanitisation and response usage injection for the
//! cache-classification seam.
//!
//! This is the §4 "adapter at the edge" for OpenAI-compatible endpoints,
//! realised as onwards helpers. It does two jobs, both gated on a wired classifier
//! in [`crate::handlers`]:
//!
//! 1. **Outbound request sanitisation** ([`strip_cache_control`]): recursively
//!    remove every `cache_control` field from the request body, and ensure
//!    `stream_options.include_usage = true` so a streaming response carries a
//!    terminal usage frame to edit. Stripping is the safe first-pass policy:
//!    upstreams vary and may reject unknown fields, and the marker is consumed
//!    here as a billing signal, not forwarded.
//!
//! 2. **Response usage injection** ([`inject_into_usage_json`] /
//!    [`inject_into_sse_body`]): splice the neutral [`CacheStats`] into the
//!    OpenAI `usage` object per §5.2 — `prompt_tokens_details.cached_tokens`
//!    plus the doubleword extension fields. Non-streaming edits the JSON body;
//!    streaming edits *only* the terminal usage frame before `[DONE]`, leaving
//!    every delta chunk untouched and never buffering the whole stream.
//!
//! With the no-op classifier every injected field is zero.

use crate::cache::CacheStats;
use axum::response::Response;
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::Value;
use tracing::error;

/// Recursively remove all `cache_control` fields from a JSON value.
///
/// Walks objects and arrays; at every object level it drops the
/// `cache_control` key (wherever a marker can ride — top-level, message,
/// content block) and recurses into the remaining values. Returns true if any
/// marker was removed.
fn remove_cache_control(value: &mut Value) -> bool {
    let mut removed = false;
    match value {
        Value::Object(map) => {
            if map.remove("cache_control").is_some() {
                removed = true;
            }
            for v in map.values_mut() {
                removed |= remove_cache_control(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                removed |= remove_cache_control(v);
            }
        }
        _ => {}
    }
    removed
}

/// Sanitise an outbound request body for the cache path: strip every
/// `cache_control` marker (recursively) and, for streaming requests, ensure
/// `stream_options.include_usage = true` so a terminal usage frame exists to
/// inject into.
///
/// Returns `Some(bytes)` with the rewritten body when anything changed, or
/// `None` to leave the original body untouched (so the caller can cheaply
/// skip re-serialisation when there was no marker and usage was already set).
///
/// This is independent of the classifier result and runs on the request pass, so
/// it never reintroduces blocking on the model call.
pub fn strip_cache_control(body: &[u8]) -> Option<Bytes> {
    let mut json: Value = serde_json::from_slice(body).ok()?;

    let stripped = remove_cache_control(&mut json);

    // Ensure include_usage for streaming requests so the terminal usage frame
    // (the thing we inject into) is emitted. Mirror stream_usage.rs semantics:
    // only touch streaming requests, and only the include_usage flag.
    let mut usage_set = false;
    if let Some(obj) = json.as_object_mut() {
        let is_streaming = obj.get("stream").and_then(Value::as_bool) == Some(true);
        if is_streaming {
            let opts = obj
                .entry("stream_options")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(opts_obj) = opts.as_object_mut() {
                let already = opts_obj.get("include_usage").and_then(Value::as_bool) == Some(true);
                if !already {
                    opts_obj.insert("include_usage".to_string(), serde_json::json!(true));
                    usage_set = true;
                }
            }
        }
    }

    if stripped || usage_set {
        serde_json::to_vec(&json).ok().map(Bytes::from)
    } else {
        None
    }
}

/// Splice the OpenAI-shaped cache fields (per §5.2) into a `usage` JSON object
/// in place. `prompt_tokens` is left as the full input count (OpenAI
/// semantics) — only the cache breakdown is added:
///
/// - `prompt_tokens_details.cached_tokens` = read tokens
/// - `cache_read_input_tokens` (extension) = read tokens
/// - `cache_creation_input_tokens` (extension) = total creation tokens
/// - `cache_creation.{ephemeral_5m,1h,24h}_input_tokens` (extension) = per-tier
fn splice_cache_fields(usage: &mut serde_json::Map<String, Value>, stats: &CacheStats) {
    // prompt_tokens_details.cached_tokens — the stock OpenAI field.
    let details = usage
        .entry("prompt_tokens_details")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(details_obj) = details.as_object_mut() {
        details_obj.insert("cached_tokens".to_string(), serde_json::json!(stats.read));
    }

    // doubleword extension fields (OpenAI clients ignore unknown response fields).
    usage.insert(
        "cache_read_input_tokens".to_string(),
        serde_json::json!(stats.read),
    );
    usage.insert(
        "cache_creation_input_tokens".to_string(),
        serde_json::json!(stats.creation_total()),
    );
    usage.insert(
        "cache_creation".to_string(),
        serde_json::json!({
            "ephemeral_5m_input_tokens": stats.creation_5m,
            "ephemeral_1h_input_tokens": stats.creation_1h,
            "ephemeral_24h_input_tokens": stats.creation_24h,
        }),
    );
}

/// Inject the cache stats into a non-streaming chat-completion JSON body.
///
/// Returns the rewritten body, or `None` if the body could not be parsed or
/// has no `usage` object to edit (in which case the caller leaves it
/// untouched). When `stats.is_zero()` this still injects the zeroed fields so
/// the response shape is consistent for a wired classifier — callers gate the
/// whole cache path on `Some(classifier)`, so a dormant onwards never reaches here.
pub fn inject_into_usage_json(body: &[u8], stats: &CacheStats) -> Option<Bytes> {
    let mut json: Value = serde_json::from_slice(body).ok()?;
    let obj = json.as_object_mut()?;
    let usage = obj.get_mut("usage")?.as_object_mut()?;
    splice_cache_fields(usage, stats);
    serde_json::to_vec(&json).ok().map(Bytes::from)
}

/// Inject the cache stats into the terminal usage frame of an SSE body.
///
/// Scans `data:` lines for the chunk that carries a `usage` object (the
/// terminal frame emitted when `include_usage=true`), edits only that frame,
/// and leaves every other line — including `[DONE]` and all content deltas —
/// byte-for-byte unchanged. SSE framing (the `\n\n` delimiters and trailing
/// newlines) is preserved.
///
/// Returns the rewritten body, or `None` if no usage-bearing frame was found
/// (so the caller forwards the original).
pub fn inject_into_sse_body(body: &[u8], stats: &CacheStats) -> Option<Bytes> {
    let body_str = std::str::from_utf8(body).ok()?;

    let mut out = String::with_capacity(body_str.len() + 256);
    let mut edited = false;

    // Preserve exact line structure: split on '\n' keeping empties, rejoin with
    // '\n', then restore the trailing-newline count from the input.
    let mut first = true;
    for line in body_str.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;

        if !edited && let Some(data) = line.strip_prefix("data: ") {
            let trimmed = data.trim();
            if trimmed != "[DONE]"
                && let Ok(mut chunk) = serde_json::from_str::<Value>(trimmed)
                && let Some(chunk_obj) = chunk.as_object_mut()
                && let Some(usage) = chunk_obj.get_mut("usage")
                && let Some(usage_obj) = usage.as_object_mut()
            {
                splice_cache_fields(usage_obj, stats);
                if let Ok(reserialized) = serde_json::to_string(&chunk) {
                    out.push_str("data: ");
                    out.push_str(&reserialized);
                    edited = true;
                    continue;
                }
            }
        }
        out.push_str(line);
    }

    if edited { Some(Bytes::from(out)) } else { None }
}

/// Inject the cache stats into a chat-completion response, dispatching on
/// content type:
///
/// - **Streaming SSE**: wrap the body stream and edit only the first
///   usage-bearing chunk as it flows, leaving all other chunks untouched. The
///   stream is never fully buffered.
/// - **Non-streaming**: buffer the (already-complete) JSON body, splice the
///   cache fields into `usage`, and rebuild the body.
///
/// If injection finds nothing to edit (e.g. no `usage`), the response is
/// returned with its body preserved.
pub async fn inject_cache_stats_into_response(
    mut response: Response,
    stats: &CacheStats,
) -> Response {
    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    if is_sse {
        use futures_util::StreamExt;

        let stats = *stats;
        // `edited` flips true after the terminal usage frame is rewritten, so
        // we only ever touch one event and skip the JSON parse on every other.
        // State is moved into the stream closure.
        let mut edited = false;
        let body_stream = BodyExt::into_data_stream(std::mem::take(response.body_mut()));
        // Providers can split a single SSE event across body chunks (HTTP framing
        // is not aligned to `\n\n` event boundaries). SseBufferedStream re-aggregates
        // so each item we inspect is a complete event — otherwise a split terminal
        // usage frame would be missed and injection silently skipped.
        let buffered = crate::sse::SseBufferedStream::new(body_stream);
        let transformed = buffered.map(move |chunk_result| match chunk_result {
            Ok(chunk) => {
                if edited {
                    return Ok::<_, std::io::Error>(chunk);
                }
                match inject_into_sse_body(&chunk, &stats) {
                    Some(rewritten) => {
                        edited = true;
                        Ok(rewritten)
                    }
                    None => Ok(chunk),
                }
            }
            Err(e) => Err(std::io::Error::other(e)),
        });

        *response.body_mut() = axum::body::Body::from_stream(transformed);
        // The body is now a streaming body of unknown length; drop any stale
        // Content-Length so it can't mismatch the rewritten stream.
        response
            .headers_mut()
            .remove(axum::http::header::CONTENT_LENGTH);
        response
    } else {
        // Only JSON bodies can carry a chat-completion `usage`. If Content-Type is
        // explicitly non-JSON (e.g. a file download), don't buffer it — return as-is
        // to preserve pass-through performance. Missing/unknown CT falls through to
        // the JSON attempt (chat completions are JSON).
        let is_json = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("application/json"))
            .unwrap_or(true);
        if !is_json {
            return response;
        }
        // Non-streaming: buffer the complete JSON body and splice.
        let (mut parts, body) = response.into_parts();
        let body_bytes = match BodyExt::collect(body).await {
            Ok(collected) => collected.to_bytes(),
            Err(e) => {
                error!("Failed to buffer response body for cache injection: {}", e);
                // Cannot recover the consumed body; return an empty body. This
                // path is only reached on a genuine body-read error, which
                // would have failed the client read anyway.
                return Response::from_parts(parts, axum::body::Body::empty());
            }
        };

        // In both arms the body has been fully buffered, so any chunked
        // Transfer-Encoding no longer applies — drop it and set the real
        // Content-Length so the framing is consistent.
        match inject_into_usage_json(&body_bytes, stats) {
            Some(rewritten) => {
                let len = rewritten.len();
                parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
                // We emit plain (uncompressed) JSON — `inject_into_usage_json` only
                // succeeds on a body it could parse as JSON — so drop any stale
                // Content-Encoding that would tell the client to decode it again.
                parts.headers.remove(axum::http::header::CONTENT_ENCODING);
                parts.headers.insert(
                    axum::http::header::CONTENT_LENGTH,
                    axum::http::HeaderValue::from(len),
                );
                Response::from_parts(parts, axum::body::Body::from(rewritten))
            }
            None => {
                // No usage to edit: the bytes are returned untouched, so KEEP any
                // Content-Encoding (they may still be compressed). Only fix framing.
                let len = body_bytes.len();
                parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
                parts.headers.insert(
                    axum::http::header::CONTENT_LENGTH,
                    axum::http::HeaderValue::from(len),
                );
                Response::from_parts(parts, axum::body::Body::from(body_bytes))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> CacheStats {
        CacheStats {
            read: 1024,
            creation_5m: 10,
            creation_1h: 20,
            creation_24h: 30,
        }
    }

    #[test]
    fn strip_removes_nested_cache_control() {
        let body = br#"{
            "model": "m",
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "sys", "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]},
                {"role": "user", "content": "hi", "cache_control": {"type": "ephemeral"}}
            ],
            "cache_control": {"type": "ephemeral"}
        }"#;
        let out = strip_cache_control(body).expect("should rewrite");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("cache_control").is_none());
        let msgs = v["messages"].as_array().unwrap();
        assert!(msgs[1].get("cache_control").is_none());
        let content = msgs[0]["content"].as_array().unwrap();
        assert!(content[0].get("cache_control").is_none());
        // body content otherwise intact
        assert_eq!(content[0]["text"], "sys");
    }

    #[test]
    fn strip_returns_none_when_nothing_to_do() {
        let body = br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        assert!(strip_cache_control(body).is_none());
    }

    #[test]
    fn strip_sets_include_usage_for_streaming() {
        let body = br#"{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let out = strip_cache_control(body).expect("should rewrite (include_usage)");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], true);
    }

    #[test]
    fn strip_leaves_non_streaming_usage_alone() {
        // no cache_control, not streaming → nothing to do
        let body = br#"{"model":"m","stream":false,"messages":[{"role":"user","content":"hi"}]}"#;
        assert!(strip_cache_control(body).is_none());
    }

    #[test]
    fn inject_non_streaming_adds_cache_fields() {
        let body = br#"{
            "id":"c1","object":"chat.completion","model":"m",
            "choices":[],
            "usage":{"prompt_tokens":1100,"completion_tokens":5,"total_tokens":1105}
        }"#;
        let out = inject_into_usage_json(body, &stats()).expect("should inject");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let usage = &v["usage"];
        // prompt_tokens untouched (full input count)
        assert_eq!(usage["prompt_tokens"], 1100);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 1024);
        assert_eq!(usage["cache_read_input_tokens"], 1024);
        assert_eq!(usage["cache_creation_input_tokens"], 60);
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 10);
        assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 20);
        assert_eq!(usage["cache_creation"]["ephemeral_24h_input_tokens"], 30);
    }

    #[test]
    fn inject_non_streaming_zero_stats_injects_zeros() {
        let body = br#"{"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let out = inject_into_usage_json(body, &CacheStats::default()).expect("should inject");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], 0);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 0);
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 0);
    }

    #[test]
    fn inject_non_streaming_none_when_no_usage() {
        let body = br#"{"id":"c1","choices":[]}"#;
        assert!(inject_into_usage_json(body, &stats()).is_none());
    }

    #[test]
    fn inject_sse_edits_only_terminal_usage_frame() {
        // A delta chunk (no usage), then a terminal usage frame, then [DONE].
        let body = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":1100,\"completion_tokens\":5,\"total_tokens\":1105}}\n\ndata: [DONE]\n\n";
        let out = inject_into_sse_body(body.as_bytes(), &stats()).expect("should inject");
        let out_str = std::str::from_utf8(&out).unwrap();

        // Delta chunk content untouched.
        assert!(out_str.contains("\"delta\":{\"content\":\"hi\"}"));
        // Usage frame got the cache fields.
        assert!(out_str.contains("\"cached_tokens\":1024"));
        assert!(out_str.contains("\"cache_read_input_tokens\":1024"));
        assert!(out_str.contains("\"cache_creation_input_tokens\":60"));
        assert!(out_str.contains("\"ephemeral_1h_input_tokens\":20"));
        // [DONE] preserved.
        assert!(out_str.contains("data: [DONE]"));
        // Framing preserved: ends with \n\n.
        assert!(out_str.ends_with("\n\n"));
        // The delta chunk should appear before the usage frame (order preserved).
        let delta_pos = out_str.find("\"content\":\"hi\"").unwrap();
        let usage_pos = out_str.find("cached_tokens").unwrap();
        assert!(delta_pos < usage_pos);
    }

    #[test]
    fn inject_sse_none_when_no_usage_frame() {
        let body = "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        assert!(inject_into_sse_body(body.as_bytes(), &stats()).is_none());
    }

    #[test]
    fn inject_sse_only_edits_first_usage_frame() {
        // Defensive: a single usage frame is the norm; ensure we stop after the
        // first edit and don't touch a second (degenerate) usage line.
        let body = "data: {\"usage\":{\"prompt_tokens\":1}}\n\ndata: {\"usage\":{\"prompt_tokens\":2}}\n\ndata: [DONE]\n\n";
        let out = inject_into_sse_body(body.as_bytes(), &stats()).expect("should inject");
        let out_str = std::str::from_utf8(&out).unwrap();
        // exactly one injected cached_tokens.
        assert_eq!(out_str.matches("cached_tokens").count(), 1);
    }

    /// Robustness: providers can split a single SSE event across body chunks.
    /// `inject_cache_stats_into_response` wraps the stream in `SseBufferedStream`,
    /// which re-aggregates to complete `\n\n`-delimited events, so the terminal
    /// usage frame is still found and injected even when split mid-JSON.
    #[tokio::test]
    async fn inject_sse_handles_event_split_across_body_chunks() {
        use futures_util::stream;
        let chunk1 = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_to";
        let chunk2 = "kens\":1100,\"completion_tokens\":5,\"total_tokens\":1105}}\n\n";
        let chunk3 = "data: [DONE]\n\n";
        let body = axum::body::Body::from_stream(stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from(chunk1)),
            Ok(Bytes::from(chunk2)),
            Ok(Bytes::from(chunk3)),
        ]));
        let response = Response::builder()
            .header("content-type", "text/event-stream")
            .body(body)
            .unwrap();

        let out = inject_cache_stats_into_response(response, &stats()).await;
        let collected = BodyExt::collect(out.into_body()).await.unwrap().to_bytes();
        let s = std::str::from_utf8(&collected).unwrap();
        // The split usage frame was re-aggregated and injected.
        assert!(
            s.contains("\"cached_tokens\":1024"),
            "usage frame should be injected across split chunks: {s}"
        );
        assert!(s.contains("\"cache_read_input_tokens\":1024"));
        assert!(s.contains("data: [DONE]"));
    }
}
