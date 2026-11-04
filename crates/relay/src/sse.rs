//! An incremental SSE parser: events end with an empty line, lines end with LF, CRLF, or CR, and a boundary may
//! arrive split across reads. The pre-commit hold's parser goes on as the bus copy's past the commit, so a stream is
//! parsed once and every reader sees the same events in the same order.

use bytes::{Bytes, BytesMut};

/// The name and the data share the parser's buffer, so a reader that keeps an event past the stream copies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub name: Option<Bytes>,
    pub data: Bytes,
}

/// More bytes buffered than the limit without a complete event among them.
#[derive(Debug)]
pub struct Overflow;

pub struct SseParser {
    buf: BytesMut,
    limit: usize,
    /// `None` on the hold's parser, which answers `Overflow` where a skipping parser discards the event.
    skip: Option<Skip>,
}

/// The skipping mode's tally, and the event it is discarding while one is past the limit.
#[derive(Default)]
struct Skip {
    skipped: u32,
    largest: usize,
    discarding: Option<Discard>,
}

impl Skip {
    /// What follows the discarded event's boundary in `bytes`: all of `bytes` when nothing is being discarded, and
    /// nothing while the boundary has not arrived.
    fn past<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let Some(discard) = self.discarding.as_mut() else {
            return bytes;
        };
        match discard.boundary(bytes) {
            Some(end) => {
                let size = discard.bytes + end;
                self.discarding = None;
                self.count(size);
                &bytes[end..]
            }
            None => {
                discard.bytes += bytes.len();
                &[]
            }
        }
    }

    fn count(&mut self, size: usize) {
        self.skipped = self.skipped.saturating_add(1);
        self.largest = self.largest.max(size);
    }
}

/// An event being discarded. Its bytes are dropped as they arrive, so a line ending or a boundary split across two
/// reads is carried as these two facts instead.
struct Discard {
    bytes: usize,
    /// The line under way holds content, so the ending that closes it is no empty line.
    line_has_content: bool,
    /// A CR ended the last read: an LF opening the next completes that CRLF rather than making an empty line.
    cr_ended: bool,
}

impl Discard {
    /// The state past `dropped`, which holds no complete event and so no empty line after content: only its last
    /// byte says where the line under way stands.
    fn after(dropped: &[u8]) -> Discard {
        let last = dropped.last().copied();
        Discard {
            bytes: dropped.len(),
            line_has_content: !matches!(last, None | Some(b'\r' | b'\n')),
            cr_ended: last == Some(b'\r'),
        }
    }

    /// The byte past the empty line that ends the event, under the rule `end_of_first_event` reads, or `None` with
    /// the facts carried to the next read.
    fn boundary(&mut self, bytes: &[u8]) -> Option<usize> {
        let &last = bytes.last()?;
        let mut at = usize::from(self.cr_ended && bytes[0] == b'\n');
        while let Some((len, ending)) = line_end(&bytes[at..]) {
            let next = at + len + ending;
            if len == 0 && !self.line_has_content {
                return Some(next);
            }
            self.line_has_content = false;
            at = next;
        }
        if at < bytes.len() {
            self.line_has_content = true;
        }
        self.cr_ended = last == b'\r';
        None
    }
}

impl SseParser {
    pub fn new(limit: usize) -> SseParser {
        SseParser {
            buf: BytesMut::new(),
            limit,
            skip: None,
        }
    }

    /// The same parser in the skipping mode, its buffered bytes kept: the hold's parser goes on as the bus copy's
    /// past the commit, where an oversized event must be skipped rather than fail the copy.
    pub fn into_skipping(self, limit: usize) -> SkippingParser {
        SkippingParser(SseParser {
            limit,
            skip: Some(Skip::default()),
            ..self
        })
    }

    /// How many events were skipped and the largest one's size in bytes, both 0 on a parser that never skipped.
    pub fn skipped(&self) -> (u32, usize) {
        self.skip
            .as_ref()
            .map_or((0, 0), |skip| (skip.skipped, skip.largest))
    }

    /// Past the limit with no complete event buffered, the hold's parser answers `Overflow`, and a skipping parser
    /// drops what it holds and discards the rest of that event, answering `Ok`. The bound holds for an event still
    /// arriving, so one that arrives whole in a single read is parsed whatever its size.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), Overflow> {
        let bytes = match self.skip.as_mut() {
            Some(skip) => skip.past(bytes),
            None => bytes,
        };
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > self.limit && self.end_of_first_event().is_none() {
            let Some(skip) = self.skip.as_mut() else {
                return Err(Overflow);
            };
            skip.discarding = Some(Discard::after(&self.buf));
            self.buf.clear();
        }
        Ok(())
    }

    pub fn next_event(&mut self) -> Option<SseEvent> {
        let end = self.end_of_first_event()?;
        let raw = self.buf.split_to(end).freeze();
        Some(parse_event(&raw))
    }

    /// The stream ended: whatever is buffered is the last event, complete or not. An event cut off while it was
    /// being discarded counts as skipped, with the bytes discarded so far as its size.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if let Some(skip) = self.skip.as_mut() {
            if let Some(discard) = skip.discarding.take() {
                skip.count(discard.bytes);
            }
        }
        if self.buf.iter().all(|b| matches!(b, b'\r' | b'\n')) {
            self.buf.clear();
            return None;
        }
        let raw = self.buf.split().freeze();
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

/// An `SseParser` in the skipping mode, which never answers `Overflow`, so its `feed` answers nothing.
pub struct SkippingParser(SseParser);

impl SkippingParser {
    /// A parser that skips an event past `limit` rather than failing: it drops what it holds, discards up to and
    /// including the next event boundary, counts the event with the bytes it discarded and parses on. The bus
    /// copy's parser past the commit, so a stream's later events still reach the completion.
    pub fn new(limit: usize) -> SkippingParser {
        SkippingParser(SseParser {
            skip: Some(Skip::default()),
            ..SseParser::new(limit)
        })
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // Both constructors set the skip, so the inner parser drops an oversized event rather than answer `Overflow`.
        let _ = self.0.feed(bytes);
    }

    pub fn next_event(&mut self) -> Option<SseEvent> {
        self.0.next_event()
    }

    pub fn finish(&mut self) -> Option<SseEvent> {
        self.0.finish()
    }

    pub fn skipped(&self) -> (u32, usize) {
        self.0.skipped()
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

/// The name and a single data line are slices of `raw`. A second data line after a non-empty first is joined in a
/// copy, and an empty first line adds no line break ahead of the next.
fn parse_event(raw: &Bytes) -> SseEvent {
    let mut name = None;
    let mut data = Bytes::new();
    let mut joined: Option<BytesMut> = None;
    let mut at = 0;
    while at < raw.len() {
        let (line_end, ending_len) = line_end(&raw[at..]).unwrap_or((raw.len() - at, 0));
        let line = &raw[at..at + line_end];
        at += line_end + ending_len;
        if line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix(b"event:") {
            name = Some(raw.slice_ref(v.strip_prefix(b" ").unwrap_or(v)));
        }
        if let Some(v) = line.strip_prefix(b"data:") {
            let v = v.strip_prefix(b" ").unwrap_or(v);
            match joined.as_mut() {
                Some(joined) => {
                    joined.extend_from_slice(b"\n");
                    joined.extend_from_slice(v);
                }
                None if data.is_empty() => data = raw.slice_ref(v),
                None => {
                    let mut copy = BytesMut::from(&data[..]);
                    copy.extend_from_slice(b"\n");
                    copy.extend_from_slice(v);
                    joined = Some(copy);
                }
            }
        }
    }
    SseEvent {
        name,
        data: joined.map_or(data, BytesMut::freeze),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(parser: &mut SseParser) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(event) = parser.next_event() {
            out.push(String::from_utf8(event.name.unwrap_or_default().to_vec()).unwrap());
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
            (second.name.as_deref(), &second.data[..]),
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
            "a trailing CR ends the empty line, and the LF that may follow is skipped as a leading empty line"
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
    fn data_lines_join_with_a_line_break_and_an_empty_first_line_adds_none() {
        let data = |raw: &[u8]| {
            let mut p = SseParser::new(1024);
            p.feed(raw).unwrap();
            p.next_event().unwrap().data.to_vec()
        };
        assert_eq!(data(b"data: a\ndata: b\n\n"), b"a\nb");
        assert_eq!(data(b"data:\ndata: b\n\n"), b"b");
        assert_eq!(
            data(b"data: a\ndata:\n\n"),
            b"a\n",
            "an empty line after a first one still adds its break"
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

    /// `bytes` in 8-byte reads, each offset in `cuts` cutting the read it falls in, so a line ending or a boundary
    /// can straddle two reads.
    fn reads(bytes: &[u8], cuts: &[usize]) -> Vec<Vec<u8>> {
        let mut ends: Vec<usize> = (8..bytes.len())
            .step_by(8)
            .chain(cuts.iter().copied())
            .chain([bytes.len()])
            .collect();
        ends.sort_unstable();
        ends.dedup();
        let mut start = 0;
        ends.into_iter()
            .map(|end| {
                let read = bytes[start..end].to_vec();
                start = end;
                read
            })
            .collect()
    }

    /// The data of every event `parser` yields over `reads`, drained after each read as the bus copy drains its
    /// parser, so the buffer holds unread bytes alone when the next read lands.
    fn yielded(parser: &mut SkippingParser, reads: &[Vec<u8>]) -> Vec<String> {
        let mut out = Vec::new();
        for read in reads {
            parser.feed(read);
            while let Some(event) = parser.next_event() {
                out.push(String::from_utf8(event.data.to_vec()).unwrap());
            }
        }
        out
    }

    /// A 40-byte event: `data: ` and `x` up to `ending`, which closes it.
    fn oversized(ending: &str) -> String {
        format!("data: {}{ending}", "x".repeat(40 - 6 - ending.len()))
    }

    #[test]
    fn a_skipping_parser_skips_an_event_past_its_limit_and_parses_on() {
        let stream = format!("{}data: a\n\ndata: b\n\n", oversized("\n\n"));
        for cuts in [&[][..], &[39]] {
            let mut p = SkippingParser::new(16);
            assert_eq!(
                yielded(&mut p, &reads(stream.as_bytes(), cuts)),
                ["a", "b"],
                "cut at {cuts:?}"
            );
            assert_eq!(p.skipped(), (1, 40), "cut at {cuts:?}");
        }
        let mut held = SseParser::new(16);
        let overflowed = reads(stream.as_bytes(), &[]).iter().any(|read| {
            let failed = held.feed(read).is_err();
            while held.next_event().is_some() {}
            failed
        });
        assert!(
            overflowed,
            "the hold's parser answers Overflow on the same bytes"
        );
    }

    #[test]
    fn a_parser_switched_to_skipping_keeps_what_it_buffered() {
        let mut p = SseParser::new(16);
        p.feed(b"data:").unwrap();
        let mut p = p.into_skipping(16);
        let mut rest = vec![b" a\n\n".to_vec()];
        rest.extend(reads(oversized("\n\n").as_bytes(), &[]));
        rest.push(b"data: b\n\n".to_vec());
        assert_eq!(
            yielded(&mut p, &rest),
            ["a", "b"],
            "the buffered half joins the rest of its event"
        );
        assert_eq!(p.skipped(), (1, 40));
    }

    #[test]
    fn a_discarded_event_ends_at_an_empty_line_under_any_line_ending() {
        // Each boundary straddles two reads, the case that needs the facts carried between them.
        let cases = [
            ("cr only", format!("{}data: a\r\r", oversized("\r\r")), 39),
            (
                "crlf then lf",
                format!("{}data: a\n\n", oversized("\r\n\n")),
                38,
            ),
            (
                "lf then cr",
                format!("{}data: a\n\n", oversized("\n\r")),
                39,
            ),
        ];
        for (name, stream, cut) in cases {
            let mut p = SkippingParser::new(16);
            assert_eq!(
                yielded(&mut p, &reads(stream.as_bytes(), &[cut])),
                ["a"],
                "{name}"
            );
            assert_eq!(p.skipped(), (1, 40), "{name}");
        }
    }

    #[test]
    fn a_discard_entered_on_a_read_that_ends_a_line_reads_the_next_read_as_that_ending_left_it() {
        // The third 8-byte read crosses the limit and ends on the line's own ending, so the discard's state comes
        // from that byte alone: after an LF the next read opens on an empty line, after a CR on the LF that
        // completes it.
        let line = format!("data: {}", "x".repeat(17));
        let cases = [
            ("lf", format!("{line}\n\n"), 25),
            ("cr", format!("{line}\r\n\r\n"), 27),
        ];
        for (name, event, size) in cases {
            assert_eq!(event.len(), size, "{name}");
            let stream = format!("{event}data: a\n\n");
            let mut p = SkippingParser::new(16);
            assert_eq!(
                yielded(&mut p, &reads(stream.as_bytes(), &[])),
                ["a"],
                "{name}"
            );
            assert_eq!(p.skipped(), (1, size), "{name}");
        }
    }

    #[test]
    fn a_crlf_cut_inside_a_discarded_event_ends_no_event() {
        // The CR closes the fourth read and its LF opens the fifth: read as an empty line, that LF would end the
        // event early and yield its tail as an event of its own.
        let event = format!("data: {}\r\n:ok\r\n\r\n", "x".repeat(25));
        assert_eq!((event.len(), &event[31..33]), (40, "\r\n"));
        let stream = format!("{event}data: a\n\n");
        let mut p = SkippingParser::new(16);
        assert_eq!(yielded(&mut p, &reads(stream.as_bytes(), &[38])), ["a"]);
        assert_eq!(p.skipped(), (1, 40));
    }

    #[test]
    fn a_stream_ending_inside_a_discarded_event_counts_it() {
        let cut_short = format!("data: {}", "x".repeat(26));
        let mut p = SkippingParser::new(16);
        assert!(yielded(&mut p, &reads(cut_short.as_bytes(), &[])).is_empty());
        assert!(
            p.finish().is_none(),
            "nothing of the discarded event is left to parse"
        );
        assert_eq!(p.skipped(), (1, 32));
    }
}
