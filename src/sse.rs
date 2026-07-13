//! SSE (Server-Sent Events) stream buffering
//!
//! This module provides a stream wrapper that buffers incomplete SSE events.
//! Some AI providers send partial chunks that split JSON data and line endings
//! across network packets. This buffer accumulates bytes until a complete SSE
//! event is received before forwarding it unchanged.

use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Maximum buffer size per SSE stream (64KB).
///
/// This prevents memory exhaustion from malicious or buggy providers that
/// send endless data without event delimiters. Typical SSE events are under
/// 1KB, so 64KB provides ample headroom while capping worst-case memory at
/// ~64MB for 1000 concurrent streams.
const MAX_SSE_BUFFER_SIZE: usize = 64 * 1024;

/// A framing failure detected while reassembling an SSE stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseFramingError {
    /// The source ended with bytes that did not form a complete SSE event.
    IncompleteEvent,
    /// A single event exceeded the bounded reassembly buffer.
    BufferOverflow { limit: usize },
}

impl fmt::Display for SseFramingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteEvent => {
                write!(f, "upstream SSE stream ended with an incomplete event")
            }
            Self::BufferOverflow { limit } => {
                write!(f, "upstream SSE event exceeded the {limit}-byte limit")
            }
        }
    }
}

impl Error for SseFramingError {}

/// An error from either the source body or SSE framing validation.
#[derive(Debug)]
pub enum SseStreamError<E> {
    Source(E),
    Framing(SseFramingError),
}

impl<E: fmt::Display> fmt::Display for SseStreamError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::Framing(error) => error.fmt(f),
        }
    }
}

impl<E: Error + 'static> Error for SseStreamError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Framing(error) => Some(error),
        }
    }
}

/// A stream wrapper that buffers SSE events until they are complete.
///
/// SSE events are delimited by a blank line. LF, CRLF, bare CR, and mixed line
/// endings are accepted, including line endings fragmented across body chunks.
/// An optional UTF-8 BOM at the beginning of the stream is preserved.
///
/// The buffer is capped at [`MAX_SSE_BUFFER_SIZE`] bytes to prevent memory
/// exhaustion from malicious or buggy upstream providers.
pub struct SseBufferedStream<S> {
    inner: S,
    buffer: BytesMut,
    source_done: bool,
    terminated: bool,
}

impl<S> SseBufferedStream<S> {
    /// Wrap an existing stream with SSE buffering.
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            buffer: BytesMut::new(),
            source_done: false,
            terminated: false,
        }
    }
}

impl<S, E> Stream for SseBufferedStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, SseStreamError<E>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        loop {
            if this.terminated {
                return Poll::Ready(None);
            }

            if let Some(end) = find_event_boundary(&this.buffer, this.source_done) {
                if end > MAX_SSE_BUFFER_SIZE {
                    this.buffer.clear();
                    this.terminated = true;
                    return Poll::Ready(Some(Err(SseStreamError::Framing(
                        SseFramingError::BufferOverflow {
                            limit: MAX_SSE_BUFFER_SIZE,
                        },
                    ))));
                }
                let complete = this.buffer.split_to(end);
                return Poll::Ready(Some(Ok(complete.freeze())));
            }

            if this.buffer.len() > MAX_SSE_BUFFER_SIZE {
                this.buffer.clear();
                this.terminated = true;
                return Poll::Ready(Some(Err(SseStreamError::Framing(
                    SseFramingError::BufferOverflow {
                        limit: MAX_SSE_BUFFER_SIZE,
                    },
                ))));
            }

            if this.source_done {
                if this.buffer.is_empty() {
                    this.terminated = true;
                    return Poll::Ready(None);
                }
                this.buffer.clear();
                this.terminated = true;
                return Poll::Ready(Some(Err(SseStreamError::Framing(
                    SseFramingError::IncompleteEvent,
                ))));
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buffer.extend_from_slice(&chunk);
                }
                Poll::Ready(Some(Err(e))) => {
                    this.buffer.clear();
                    this.terminated = true;
                    return Poll::Ready(Some(Err(SseStreamError::Source(e))));
                }
                Poll::Ready(None) => {
                    this.source_done = true;
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

/// Find the byte immediately after the first complete event boundary.
fn find_event_boundary(buf: &[u8], source_done: bool) -> Option<usize> {
    let mut line_start = 0;
    let mut index = 0;

    while index < buf.len() {
        let line_end = match buf[index] {
            b'\n' => index + 1,
            b'\r' if index + 1 < buf.len() && buf[index + 1] == b'\n' => index + 2,
            b'\r' if index + 1 < buf.len() || source_done => index + 1,
            b'\r' => return None,
            _ => {
                index += 1;
                continue;
            }
        };

        if index == line_start {
            return Some(line_end);
        }
        line_start = line_end;
        index = line_end;
    }

    None
}

/// Collect an event's `data:` fields according to SSE line and colon rules.
///
/// The optional stream-start UTF-8 BOM is ignored for field-name matching. The
/// caller still owns the original event bytes, so parsing never normalizes what
/// is forwarded downstream.
pub(crate) fn event_data(event: &[u8]) -> Option<Vec<u8>> {
    let mut position = if event.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    let mut data = Vec::new();
    let mut found_data = false;

    while position < event.len() {
        let line_start = position;
        while position < event.len() && !matches!(event[position], b'\r' | b'\n') {
            position += 1;
        }
        let line = &event[line_start..position];

        if position < event.len() {
            if event[position] == b'\r'
                && position + 1 < event.len()
                && event[position + 1] == b'\n'
            {
                position += 2;
            } else {
                position += 1;
            }
        }

        let Some(value) = line.strip_prefix(b"data:") else {
            continue;
        };
        if found_data {
            data.push(b'\n');
        }
        found_data = true;
        data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
    }

    found_data.then_some(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::convert::Infallible;

    /// Helper to create a stream from chunks
    fn chunks_to_stream(
        chunks: Vec<&'static [u8]>,
    ) -> impl Stream<Item = Result<Bytes, Infallible>> + Unpin {
        futures_util::stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from_static(c))))
    }

    #[tokio::test]
    async fn test_complete_event_passes_through() {
        let chunks = vec![b"data: {\"hello\": \"world\"}\n\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: {\"hello\": \"world\"}\n\n"
        );
    }

    #[tokio::test]
    async fn test_split_event_is_buffered() {
        // Event split across two chunks
        let chunks = vec![
            b"data: {\"hel".as_slice(),
            b"lo\": \"world\"}\n\n".as_slice(),
        ];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: {\"hello\": \"world\"}\n\n"
        );
    }

    #[tokio::test]
    async fn test_multiple_events_in_one_chunk() {
        let chunks = vec![b"data: first\n\ndata: second\n\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), b"data: first\n\n");
        assert_eq!(results[1].as_ref().unwrap().as_ref(), b"data: second\n\n");
    }

    #[tokio::test]
    async fn test_event_split_at_newline() {
        // Split right at the delimiter
        let chunks = vec![b"data: test\n".as_slice(), b"\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), b"data: test\n\n");
    }

    #[tokio::test]
    async fn test_multiple_events_across_chunks() {
        let chunks = vec![
            b"data: first\n\ndata: sec".as_slice(),
            b"ond\n\ndata: third\n\n".as_slice(),
        ];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].as_ref().unwrap().as_ref(), b"data: first\n\n");
        assert_eq!(results[1].as_ref().unwrap().as_ref(), b"data: second\n\n");
        assert_eq!(results[2].as_ref().unwrap().as_ref(), b"data: third\n\n");
    }

    #[tokio::test]
    async fn test_incomplete_event_at_stream_end_is_a_framing_error() {
        let chunks = vec![b"data: incomplete".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            Err(SseStreamError::Framing(SseFramingError::IncompleteEvent))
        ));
    }

    #[tokio::test]
    async fn test_empty_stream() {
        let chunks: Vec<&[u8]> = vec![];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 0);
    }

    #[tokio::test]
    async fn test_json_split_across_many_chunks() {
        // Simulate very fragmented delivery
        let chunks = vec![
            b"da".as_slice(),
            b"ta: ".as_slice(),
            b"{\"delta\"".as_slice(),
            b": {\"".as_slice(),
            b"content\": \"Hello".as_slice(),
            b"\"}}\n".as_slice(),
            b"\n".as_slice(),
        ];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: {\"delta\": {\"content\": \"Hello\"}}\n\n"
        );
    }

    #[tokio::test]
    async fn test_fragmented_lf_crlf_cr_and_mixed_boundaries_preserve_exact_bytes() {
        for (chunks, expected) in [
            (
                vec![b"data: lf\n".as_slice(), b"\n".as_slice()],
                b"data: lf\n\n".as_slice(),
            ),
            (
                vec![
                    b"data: crlf\r".as_slice(),
                    b"\n\r".as_slice(),
                    b"\n".as_slice(),
                ],
                b"data: crlf\r\n\r\n".as_slice(),
            ),
            (
                vec![b"data: cr\r".as_slice(), b"\r".as_slice()],
                b"data: cr\r\r".as_slice(),
            ),
            (
                vec![b"data: mixed\r\n\r".as_slice(), b"\n".as_slice()],
                b"data: mixed\r\n\r\n".as_slice(),
            ),
        ] {
            let stream = SseBufferedStream::new(chunks_to_stream(chunks));
            let results: Vec<_> = stream.collect().await;

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].as_ref().unwrap().as_ref(), expected);
        }
    }

    #[tokio::test]
    async fn test_crlf_event_is_available_before_source_eof() {
        let source = futures_util::stream::once(async {
            Ok::<_, Infallible>(Bytes::from_static(b"data: ready\r\n\r\n"))
        })
        .chain(futures_util::stream::pending());
        let mut stream = SseBufferedStream::new(Box::pin(source));

        let event = tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
            .await
            .expect("complete CRLF event should not wait for EOF")
            .expect("event")
            .expect("valid frame");

        assert_eq!(event.as_ref(), b"data: ready\r\n\r\n");
    }

    #[tokio::test]
    async fn test_utf8_bom_is_preserved_at_stream_start() {
        let chunks = vec![
            b"\xef".as_slice(),
            b"\xbb\xbfdata:value\r".as_slice(),
            b"\r".as_slice(),
        ];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"\xef\xbb\xbfdata:value\r\r"
        );
    }

    #[tokio::test]
    async fn test_preserves_multiline_data() {
        // SSE can have multi-line data fields
        let chunks = vec![b"data: line1\ndata: line2\n\n".as_slice()];
        let stream = SseBufferedStream::new(chunks_to_stream(chunks));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap().as_ref(),
            b"data: line1\ndata: line2\n\n"
        );
    }

    #[tokio::test]
    async fn test_buffer_overflow_is_a_framing_error() {
        // Create a chunk larger than MAX_SSE_BUFFER_SIZE without \n\n
        let large_chunk = vec![b'x'; MAX_SSE_BUFFER_SIZE + 1];
        let chunks: Vec<&[u8]> = vec![&large_chunk];
        let stream = SseBufferedStream::new(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Bytes::from(c.to_vec()))),
        ));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert!(matches!(
            results[0],
            Err(SseStreamError::Framing(SseFramingError::BufferOverflow {
                limit: MAX_SSE_BUFFER_SIZE
            }))
        ));
    }

    #[tokio::test]
    async fn test_source_body_error_remains_distinct_from_framing_errors() {
        let source = futures_util::stream::iter(vec![Err::<Bytes, _>(std::io::Error::other(
            "source failed",
        ))]);
        let results: Vec<_> = SseBufferedStream::new(source).collect().await;

        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(SseStreamError::Source(_))));
        assert_eq!(
            results[0].as_ref().unwrap_err().to_string(),
            "source failed"
        );
    }

    #[tokio::test]
    async fn test_buffer_at_limit_still_works() {
        // Create a chunk exactly at MAX_SSE_BUFFER_SIZE with \n\n at the end
        let mut chunk = vec![b'x'; MAX_SSE_BUFFER_SIZE - 2];
        chunk.extend_from_slice(b"\n\n");
        let chunks: Vec<&[u8]> = vec![&chunk];
        let stream = SseBufferedStream::new(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Bytes::from(c.to_vec()))),
        ));
        let results: Vec<_> = stream.collect().await;

        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
        assert_eq!(results[0].as_ref().unwrap().len(), MAX_SSE_BUFFER_SIZE);
    }
}
