// SPDX-License-Identifier: MIT OR Apache-2.0

//! A strict, bounded parser for server-sent events (`text/event-stream`).
//!
//! The search stream is untrusted input, so the parser keeps at most [`MAX_EVENT_BYTES`] of one
//! event in memory, refuses streams longer than [`MAX_STREAM_BYTES`] or with more than
//! [`MAX_EVENTS`] events, and never panics. An event that is too large is skipped (reported as
//! [`SseEvent::Oversized`]) and parsing resumes at the next one. Lines end with LF, CRLF, or CR;
//! `id:`, `retry:`, comments, and unknown fields are ignored; a final event without the blank
//! line that ends it is discarded, as the format requires.

use crate::results::MAX_RESPONSE_BYTES;

/// Largest event (all of its lines) kept, in bytes: the size of a whole `/search` response.
pub(crate) const MAX_EVENT_BYTES: usize = MAX_RESPONSE_BYTES;
/// Longest stream read, in bytes.
pub(crate) const MAX_STREAM_BYTES: usize = 4 * MAX_RESPONSE_BYTES;
/// Most events read from one stream (one per indexer, plus the last one).
pub(crate) const MAX_EVENTS: usize = 400;
/// Longest event type kept; longer ones are reported as an unknown type.
const MAX_NAME_BYTES: usize = 64;
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// One event read from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SseEvent {
    /// A complete event: its type (`message` when none was given) and its `data:` lines
    /// joined with `\n`.
    Event { name: String, data: Vec<u8> },
    /// An event larger than [`MAX_EVENT_BYTES`], skipped.
    Oversized,
}

/// Why a stream was abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SseError {
    /// Longer than [`MAX_STREAM_BYTES`].
    #[error("the stream is too long")]
    TooLong,
    /// More than [`MAX_EVENTS`] events.
    #[error("the stream has too many events")]
    TooManyEvents,
}

/// The data of the event being read.
#[derive(Debug, Default)]
enum Body {
    /// No `data:` line yet.
    #[default]
    Empty,
    /// The `data:` lines so far, joined with `\n`.
    Data(Vec<u8>),
    /// Too large: the rest of the event is dropped.
    Oversized,
}

/// Incremental parser state: feed it the body as it arrives.
#[derive(Debug, Default)]
pub(crate) struct SseParser {
    /// The line read so far.
    line: Vec<u8>,
    /// The current line is too long: the rest of it is dropped.
    line_too_long: bool,
    /// The previous chunk ended with CR, so a leading LF finishes that line ending.
    after_cr: bool,
    /// A line has ended (a byte-order mark is only allowed before the first).
    started: bool,
    name: Vec<u8>,
    body: Body,
    bytes: usize,
    events: usize,
}

impl SseParser {
    /// Parses `chunk`, appending the events it completes to `out`.
    ///
    /// # Errors
    ///
    /// Returns an error once the stream exceeds its limits; stop reading it then.
    pub(crate) fn feed(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) -> Result<(), SseError> {
        let mut rest = chunk;
        self.bytes = self.bytes.saturating_add(chunk.len());
        if self.bytes > MAX_STREAM_BYTES {
            return Err(SseError::TooLong);
        }
        if std::mem::take(&mut self.after_cr)
            && let Some(after) = rest.strip_prefix(b"\n")
        {
            rest = after;
        }
        while let Some(at) = rest.iter().position(|&b| b == b'\n' || b == b'\r') {
            self.push(&rest[..at]);
            let cr = rest[at] == b'\r';
            rest = &rest[at + 1..];
            if cr {
                match rest.strip_prefix(b"\n") {
                    Some(after) => rest = after,
                    None => self.after_cr = rest.is_empty(),
                }
            }
            self.end_line(out)?;
        }
        self.push(rest);
        Ok(())
    }

    /// Adds bytes to the current line, dropping it if it grows too long.
    fn push(&mut self, bytes: &[u8]) {
        if self.line_too_long {
            return;
        }
        if self.line.len() + bytes.len() > MAX_EVENT_BYTES {
            self.line_too_long = true;
            self.line = Vec::new();
        } else {
            self.line.extend_from_slice(bytes);
        }
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) -> Result<(), SseError> {
        let first = !std::mem::replace(&mut self.started, true);
        if std::mem::take(&mut self.line_too_long) {
            self.body = Body::Oversized;
            return Ok(());
        }
        let mut line = std::mem::take(&mut self.line);
        if first && line.starts_with(BOM) {
            line.drain(..BOM.len());
        }
        if line.is_empty() {
            return self.dispatch(out);
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            // A comment.
            Some(0) => (&b""[..], &b""[..]),
            Some(at) => {
                let value = &line[at + 1..];
                (&line[..at], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (&line[..], &b""[..]),
        };
        match field {
            b"event" => value.clone_into(&mut self.name),
            b"data" => self.add_data(value),
            _ => {}
        }
        // Reuse the line's allocation.
        self.line = line;
        self.line.clear();
        Ok(())
    }

    fn add_data(&mut self, value: &[u8]) {
        match &mut self.body {
            Body::Oversized => {}
            Body::Empty if value.len() > MAX_EVENT_BYTES => self.body = Body::Oversized,
            Body::Empty => self.body = Body::Data(value.to_vec()),
            Body::Data(data) if data.len() + 1 + value.len() > MAX_EVENT_BYTES => {
                self.body = Body::Oversized;
            }
            Body::Data(data) => {
                data.push(b'\n');
                data.extend_from_slice(value);
            }
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) -> Result<(), SseError> {
        let name = std::mem::take(&mut self.name);
        let event = match std::mem::take(&mut self.body) {
            // An event with no data is not dispatched.
            Body::Empty => return Ok(()),
            Body::Data(data) => SseEvent::Event {
                name: event_name(&name),
                data,
            },
            Body::Oversized => SseEvent::Oversized,
        };
        self.events += 1;
        if self.events > MAX_EVENTS {
            return Err(SseError::TooManyEvents);
        }
        out.push(event);
        Ok(())
    }
}

/// The event type as text: `message` if none was given, and a type no real event has if it
/// is too long or not UTF-8.
fn event_name(raw: &[u8]) -> String {
    if raw.is_empty() {
        return "message".into();
    }
    match std::str::from_utf8(raw) {
        Ok(name) if raw.len() <= MAX_NAME_BYTES => name.to_owned(),
        _ => "(invalid)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, data: &str) -> SseEvent {
        SseEvent::Event {
            name: name.into(),
            data: data.as_bytes().to_vec(),
        }
    }

    fn parse_all(chunks: &[&[u8]]) -> Result<Vec<SseEvent>, SseError> {
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        for chunk in chunks {
            parser.feed(chunk, &mut out)?;
        }
        Ok(out)
    }

    #[test]
    fn parses_events_as_the_service_sends_them() {
        let body = b"event: indexer\ndata: {\"id\": \"yts\"}\n\nevent: done\ndata: {}\n\n";
        assert_eq!(
            parse_all(&[body]).unwrap(),
            [event("indexer", "{\"id\": \"yts\"}"), event("done", "{}")]
        );
    }

    #[test]
    fn handles_every_line_ending_and_any_chunking() {
        let body: &[u8] =
            b"\xEF\xBB\xBFevent: a\r\ndata: 1\r\n\r\nevent: b\rdata: 2\r\rdata: x\n\n";
        let expected = [event("a", "1"), event("b", "2"), event("message", "x")];
        assert_eq!(parse_all(&[body]).unwrap(), expected);
        // One byte at a time, so CR LF pairs and the byte-order mark are split across chunks.
        let bytes: Vec<&[u8]> = body.chunks(1).collect();
        assert_eq!(parse_all(&bytes).unwrap(), expected);
    }

    #[test]
    fn joins_data_lines_and_ignores_other_fields() {
        let body = b": keep-alive\nid: 7\nretry: 10\nfoo: bar\ndata:a\ndata: b\ndata\n\n";
        assert_eq!(parse_all(&[body]).unwrap(), [event("message", "a\nb\n")]);
    }

    #[test]
    fn events_without_data_or_without_an_ending_are_not_dispatched() {
        let body = b"event: indexer\n\ndata: unfinished";
        assert_eq!(parse_all(&[body]).unwrap(), []);
        // The type of an event without data does not leak into the next one.
        assert_eq!(
            parse_all(&[b"event: x\n\ndata: 1\n\n"]).unwrap(),
            [event("message", "1")]
        );
    }

    #[test]
    fn skips_oversized_events_and_carries_on() {
        let big = "x".repeat(MAX_EVENT_BYTES);
        // One huge line, sent in pieces so the limit trips mid-line.
        let body = format!("event: indexer\ndata: {big}\n\nevent: done\ndata: {{}}\n\n");
        let chunks: Vec<&[u8]> = body.as_bytes().chunks(64 * 1024).collect();
        assert_eq!(
            parse_all(&chunks).unwrap(),
            [SseEvent::Oversized, event("done", "{}")]
        );
        // Many lines that are small alone but too much together.
        let line = format!("data: {}\n", "y".repeat(1024 * 1024));
        let body = format!("{}\nevent: done\ndata: ok\n\n", line.repeat(5));
        assert_eq!(
            parse_all(&[body.as_bytes()]).unwrap(),
            [SseEvent::Oversized, event("done", "ok")]
        );
    }

    #[test]
    fn refuses_streams_over_the_limits() {
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        let chunk = vec![b'a'; MAX_EVENT_BYTES];
        let mut result = Ok(());
        for _ in 0..=MAX_STREAM_BYTES / MAX_EVENT_BYTES {
            result = parser.feed(&chunk, &mut out);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err(SseError::TooLong));

        let many = "data: 1\n\n".repeat(MAX_EVENTS + 1);
        assert_eq!(parse_all(&[many.as_bytes()]), Err(SseError::TooManyEvents));
    }

    #[test]
    fn bad_event_names_never_match_a_real_type() {
        let long = format!("event: {}\ndata: 1\n\n", "n".repeat(MAX_NAME_BYTES + 1));
        assert_eq!(
            parse_all(&[long.as_bytes()]).unwrap(),
            [event("(invalid)", "1")]
        );
        assert_eq!(
            parse_all(&[b"event: \xFF\xFE\ndata: 1\n\n"]).unwrap(),
            [event("(invalid)", "1")]
        );
    }

    #[test]
    fn survives_arbitrary_bytes() {
        // A cheap deterministic fuzz: nothing here may panic.
        let mut state: u32 = 0x1234_5678;
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        for _ in 0..2000 {
            let chunk: Vec<u8> = (0..64)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    b"ev:dta \n\r\xFF"[state as usize % 10]
                })
                .collect();
            if parser.feed(&chunk, &mut out).is_err() {
                break;
            }
        }
    }
}
