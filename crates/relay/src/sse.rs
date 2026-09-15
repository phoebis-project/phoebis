//! An incremental SSE parser: events end with an empty line, lines end with LF, CRLF, or CR, and a boundary may
//! arrive split across reads. Shared by the pre-commit hold and the post-commit observer, so both see the same
//! events in the same order.

use bytes::BytesMut;

#[derive(Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub name: Option<Vec<u8>>,
    pub data: Vec<u8>,
}

/// More bytes buffered than the limit without a complete event among them.
#[derive(Debug)]
pub struct Overflow;

pub struct SseParser {
    buf: BytesMut,
    limit: usize,
}

impl SseParser {
    pub fn new(limit: usize) -> SseParser {
        SseParser {
            buf: BytesMut::new(),
            limit,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), Overflow> {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.limit && self.end_of_first_event().is_none() {
            return Err(Overflow);
        }
        Ok(())
    }

    pub fn next_event(&mut self) -> Option<SseEvent> {
        let end = self.end_of_first_event()?;
        let raw = self.buf.split_to(end);
        Some(parse_event(&raw))
    }

    /// The stream ended: whatever is buffered is the last event, complete or not.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buf.iter().all(|b| matches!(b, b'\r' | b'\n')) {
            self.buf.clear();
            return None;
        }
        let raw = self.buf.split();
        Some(parse_event(&raw))
    }

    /// The byte past the empty line that ends the first complete event, or `None` while it is still arriving.
    fn end_of_first_event(&self) -> Option<usize> {
        let buf = &self.buf[..];
        let mut at = 0;
        let mut saw_content = false;
        while at < buf.len() {
            let (line_end, ending_len) = line_end(&buf[at..])?;
            let line = &buf[at..at + line_end];
            let next = at + line_end + ending_len;
            if line.is_empty() {
                if saw_content {
                    return Some(next);
                }
                // A leading empty line is skipped, as the SSE grammar allows.
                at = next;
                continue;
            }
            saw_content = true;
            at = next;
        }
        None
    }
}

/// The first line ending in `buf` as (length before it, length of it). A CR that is the last byte ends its line
/// on its own: an LF arriving on the next read lands as a leading empty line, which the event scan skips, so a
/// CRLF split across reads still produces one event carrying the same data.
fn line_end(buf: &[u8]) -> Option<(usize, usize)> {
    for (i, b) in buf.iter().enumerate() {
        match b {
            b'\n' => return Some((i, 1)),
            b'\r' => return Some((i, if buf.get(i + 1) == Some(&b'\n') { 2 } else { 1 })),
            _ => {}
        }
    }
    None
}

fn parse_event(raw: &[u8]) -> SseEvent {
    let mut name = None;
    let mut data = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let (line_end, ending_len) = line_end(&raw[at..]).unwrap_or((raw.len() - at, 0));
        let line = &raw[at..at + line_end];
        at += line_end + ending_len;
        if line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix(b"event:") {
            name = Some(v.strip_prefix(b" ").unwrap_or(v).to_vec());
        }
        if let Some(v) = line.strip_prefix(b"data:") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(v.strip_prefix(b" ").unwrap_or(v));
        }
    }
    SseEvent { name, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(parser: &mut SseParser) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(event) = parser.next_event() {
            out.push(String::from_utf8(event.name.unwrap_or_default()).unwrap());
        }
        out
    }

    #[test]
    fn crlf_only_streams_commit() {
        let mut p = SseParser::new(1024);
        p.feed(b"event: ping\r\ndata: {}\r\n\r\nevent: message_start\r\ndata: {\"a\":1}\r\n\r\n")
            .unwrap();
        assert_eq!(names(&mut p), ["ping", "message_start"]);
    }

    #[test]
    fn mixed_line_endings_do_not_merge_events() {
        let mut p = SseParser::new(1024);
        p.feed(
            b"event: message_start\r\ndata: {}\r\n\r\nevent: error\ndata: {\"type\":\"error\"}\n\n",
        )
        .unwrap();
        let first = p.next_event().unwrap();
        assert_eq!(first.name.as_deref(), Some(b"message_start".as_slice()));
        let second = p.next_event().unwrap();
        assert_eq!(
            (second.name.as_deref(), second.data.as_slice()),
            (
                Some(b"error".as_slice()),
                b"{\"type\":\"error\"}".as_slice()
            )
        );
        let mut cr = SseParser::new(1024);
        cr.feed(b"event: ping\rdata: {}\r\revent: error\rdata: x\r\r")
            .unwrap();
        assert_eq!(names(&mut cr), ["ping", "error"]);
    }

    #[test]
    fn a_boundary_split_across_two_reads_is_found() {
        let mut p = SseParser::new(1024);
        p.feed(b"event: message_start\r\ndata: {}\r\n\r").unwrap();
        assert_eq!(
            p.next_event().unwrap().name.as_deref(),
            Some(b"message_start".as_slice()),
            "a trailing CR ends the empty line; the LF that may follow is skipped as a leading empty line"
        );
        p.feed(b"\nevent: ping\n\n").unwrap();
        assert_eq!(names(&mut p), ["ping"]);
        let mut q = SseParser::new(1024);
        q.feed(b"event: error\ndata: {\"type\":\"error\"}\n")
            .unwrap();
        assert!(q.next_event().is_none());
        q.feed(b"\n").unwrap();
        assert_eq!(
            q.next_event().unwrap().name.as_deref(),
            Some(b"error".as_slice())
        );
        assert!(q.finish().is_none());
        let mut tail = SseParser::new(1024);
        tail.feed(b"event: message_stop\ndata: {}").unwrap();
        assert_eq!(
            tail.finish().unwrap().name.as_deref(),
            Some(b"message_stop".as_slice()),
            "the last event needs no trailing empty line at end of stream"
        );
    }

    #[test]
    fn the_buffer_is_bounded_without_a_boundary() {
        let mut p = SseParser::new(16);
        assert!(p.feed(b"data: 0123456789").is_ok());
        assert!(
            p.feed(b"0123456789").is_err(),
            "past the limit with no complete event"
        );
        let mut q = SseParser::new(16);
        q.feed(b"data: a\n\n").unwrap();
        q.next_event();
        assert!(
            q.feed(b"data: bbbbbbbbb\n\n").is_ok(),
            "consumed events free the buffer"
        );
    }
}
