//! The caller's request head as it arrived on the wire.
//!
//! hyper 1.11 keeps the original header spellings in an extension with no server-side accessor, so the only
//! way to relay a caller's own casing and order is to read the head before hyper parses it: the recorder below
//! wraps the accepted stream, copies what hyper reads until `httparse` calls the head complete, and leaves it
//! in a queue the connection's service drains into the request's extensions.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::http::Request;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Header slots one head may carry. A head with more is not recorded rather than recorded in part.
const MAX_HEADERS: usize = 64;

/// Bytes one head may span before the recorder gives up on the connection, so a caller that never terminates
/// its head cannot grow the copy without bound.
const MAX_HEAD: usize = 64 << 10;

/// One caller's head as it was spelled on the wire: each name as the caller wrote it, each value as it was
/// sent, in the order they arrived. `http::HeaderMap` documents its iteration order as arbitrary, so this list
/// is the only authoritative record of either.
///
/// The request line rides along so a head is only ever used for the request it was read from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OriginalHead {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl OriginalHead {
    /// One `httparse` pass over the bytes read so far, with the length the head spans beside it. `None` for
    /// anything that is not (yet) a complete HTTP/1 head, which is also what an HTTP/2 preface and a head past
    /// `MAX_HEADERS` yield.
    pub fn parse(raw: &[u8]) -> Option<(OriginalHead, usize)> {
        match scan(raw) {
            Parsed::Complete(head, used) => Some((head, used)),
            Parsed::Failed | Parsed::Partial => None,
        }
    }

    /// Whether hyper parsed this head out of the same bytes. A head that does not match means the recorder
    /// and hyper disagree about where the connection is, and relaying on it would put one request's headers
    /// on another request's wire.
    pub fn matches<B>(&self, request: &Request<B>) -> bool {
        self.method == request.method().as_str()
            && request
                .uri()
                .path_and_query()
                .is_some_and(|target| self.target == target.as_str())
    }

    /// The first value under `name`, matched without regard to case.
    pub fn get(&self, name: &str) -> Option<&str> {
        phoebis_transport::response::header(&self.headers, name)
    }
}

/// What one `httparse` pass over the bytes read so far found.
enum Parsed {
    /// A head and the bytes it spans. hyper runs the same `httparse` version over the same bytes with the
    /// same default configuration, so this length is the boundary hyper reads too, bare LF or CRLF.
    Complete(OriginalHead, usize),
    /// No head yet; more bytes may finish one.
    Partial,
    /// Not an HTTP/1 head at all, or one past `MAX_HEADERS`.
    Failed,
}

fn scan(raw: &[u8]) -> Parsed {
    let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut slots);
    let used = match request.parse(raw) {
        Ok(httparse::Status::Complete(used)) => used,
        Ok(httparse::Status::Partial) => return Parsed::Partial,
        Err(_) => return Parsed::Failed,
    };
    let (Some(method), Some(target)) = (request.method, request.path) else {
        return Parsed::Failed;
    };
    Parsed::Complete(
        OriginalHead {
            method: method.to_string(),
            target: target.to_string(),
            headers: request
                .headers
                .iter()
                // A value that is not UTF-8 cannot be re-emitted as the caller wrote it, and the `HeaderMap`
                // walk the relay falls back to drops it just the same.
                .filter_map(|h| {
                    Some((
                        h.name.to_string(),
                        std::str::from_utf8(h.value).ok()?.into(),
                    ))
                })
                .collect(),
        },
        used,
    )
}

/// Heads recorded on one connection, waiting for the requests hyper parses out of the same bytes.
pub type HeadQueue = Arc<Mutex<VecDeque<OriginalHead>>>;

pub fn head_queue() -> HeadQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// Body bytes the head declares, or `None` for framing the recorder cannot skip. Chunked is the one that
/// matters: its length lives in the body, so the recorder would have to decode the framing to find the next
/// head and instead stops recording the connection.
fn declared_body(head: &OriginalHead) -> Option<u64> {
    if head.get("transfer-encoding").is_some() {
        return None;
    }
    match head.get("content-length") {
        None => Some(0),
        Some(value) => value.trim().parse().ok(),
    }
}

enum Recording {
    /// Copying bytes into `head` until `httparse` calls one complete.
    Head,
    /// Skipping the declared body, with this many bytes left of it.
    Body(u64),
    /// Nothing more on this connection is recorded; its requests reach the relay without a head and the
    /// outbound builder falls back to the `HeaderMap`.
    Off,
}

/// The accepted stream with a copy of each request head taken as hyper reads it. Writes are delegated
/// untouched; the response half is the relay's own bytes and needs no record.
pub struct HeadRecorder<S> {
    head: Vec<u8>,
    inner: S,
    queue: HeadQueue,
    state: Recording,
}

impl<S> HeadRecorder<S> {
    pub fn new(inner: S, queue: HeadQueue) -> HeadRecorder<S> {
        HeadRecorder {
            head: Vec::new(),
            inner,
            queue,
            state: Recording::Head,
        }
    }

    /// The stream, for a connection that leaves HTTP. What the recorder held of a head goes with it.
    pub fn into_inner(self) -> S {
        self.inner
    }

    fn observe(&mut self, data: &[u8]) {
        // Each head found hands back how far into the read it ended, so a keep-alive connection whose next head
        // arrived in the same segment is picked up without recursing per request.
        let mut at = 0;
        while let Some(consumed) = self.feed(&data[at..]) {
            at += consumed;
        }
    }

    /// Consumes `data` up to and including the next head, returning how many of its bytes that took.
    fn feed(&mut self, data: &[u8]) -> Option<usize> {
        let mut at = 0;
        loop {
            match self.state {
                Recording::Off => return None,
                Recording::Body(remaining) => {
                    let take = remaining.min((data.len() - at) as u64);
                    at += take as usize;
                    self.state = if take == remaining {
                        Recording::Head
                    } else {
                        Recording::Body(remaining - take)
                    };
                    if at == data.len() {
                        return None;
                    }
                }
                Recording::Head => {
                    // A head may straddle two reads, so every read re-parses what the connection has sent so
                    // far: the boundary has to be the parser's, not a search of its own, or the recorder and
                    // hyper end the head in different places and the next request travels under this one. A
                    // read that starts a head is scanned where it lies and copied only when the head runs on.
                    let held = self.head.len();
                    let scanned = if held == 0 {
                        scan(&data[at..])
                    } else {
                        self.head.extend_from_slice(&data[at..]);
                        scan(&self.head)
                    };
                    let (head, used) = match scanned {
                        Parsed::Complete(head, used) => (head, used),
                        Parsed::Partial => {
                            if held == 0 {
                                self.head.extend_from_slice(&data[at..]);
                            }
                            if self.head.len() > MAX_HEAD {
                                self.stop();
                            }
                            return None;
                        }
                        Parsed::Failed => {
                            self.stop();
                            return None;
                        }
                    };
                    if used > MAX_HEAD {
                        self.stop();
                        return None;
                    }
                    self.state = match declared_body(&head) {
                        Some(body) => {
                            self.queue
                                .lock()
                                .expect("head queue poisoned")
                                .push_back(head);
                            match body {
                                0 => Recording::Head,
                                n => Recording::Body(n),
                            }
                        }
                        None => Recording::Off,
                    };
                    self.head.clear();
                    // The held bytes came before this read, so the head ends `used - held` bytes into it.
                    return Some(at + used - held);
                }
            }
        }
    }

    fn stop(&mut self) {
        self.state = Recording::Off;
        self.head = Vec::new();
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for HeadRecorder<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            this.observe(&buf.filled()[before..]);
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for HeadRecorder<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    const HEAD: &[u8] = b"POST /v1/messages HTTP/1.1\r\nHost: api.anthropic.com\r\nX-Stainless-Arch: x64\r\nContent-Length: 2\r\n\r\n{}";

    async fn drain(bytes: Vec<u8>) -> VecDeque<OriginalHead> {
        let queue = head_queue();
        let mut recorder = HeadRecorder::new(std::io::Cursor::new(bytes), queue.clone());
        let mut sink = Vec::new();
        recorder.read_to_end(&mut sink).await.unwrap();
        let heads = queue.lock().unwrap().clone();
        heads
    }

    #[tokio::test]
    async fn a_head_is_recorded_with_the_callers_spelling_and_order() {
        let heads = drain(HEAD.to_vec()).await;
        assert_eq!(
            heads[0].headers,
            vec![
                ("Host".to_string(), "api.anthropic.com".to_string()),
                ("X-Stainless-Arch".to_string(), "x64".to_string()),
                ("Content-Length".to_string(), "2".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn the_declared_body_is_skipped_so_the_next_head_on_the_connection_is_read() {
        let heads = drain([HEAD, HEAD].concat()).await;
        assert_eq!(heads.len(), 2, "both heads of a keep-alive connection");
        assert_eq!(heads[0], heads[1]);
    }

    #[tokio::test]
    async fn a_chunked_request_stops_the_recording_for_the_rest_of_the_connection() {
        let chunked =
            b"POST /v1/messages HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        let heads = drain([chunked.as_slice(), HEAD].concat()).await;
        assert!(
            heads.is_empty(),
            "neither the chunked request nor what follows it on the connection is recorded"
        );
    }

    #[tokio::test]
    async fn a_head_past_the_cap_and_one_that_does_not_parse_are_not_recorded() {
        let oversized = format!(
            "POST / HTTP/1.1\r\nX-Big: {}\r\n\r\n",
            "x".repeat(MAX_HEAD + 1)
        );
        assert!(drain(oversized.into_bytes()).await.is_empty());
        // The HTTP/2 preface is the shape a caller that negotiated h2 opens with; httparse refuses its
        // version, so the connection falls back rather than recording a head with no headers.
        let preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        assert!(drain(preface.to_vec()).await.is_empty());
    }

    #[test]
    fn three_heads_are_found_whether_they_arrive_in_one_read_or_split_across_two() {
        let head = |target: &str| {
            format!("POST {target} HTTP/1.1\r\nHost: h\r\nContent-Length: 2\r\n\r\n{{}}")
                .into_bytes()
        };
        let stream = [head("/a"), head("/b"), head("/c")].concat();
        let targets = |reads: &[&[u8]]| {
            let queue = head_queue();
            let mut recorder =
                HeadRecorder::new(std::io::Cursor::new(Vec::<u8>::new()), queue.clone());
            for read in reads {
                recorder.observe(read);
            }
            let targets: Vec<String> = queue
                .lock()
                .unwrap()
                .iter()
                .map(|h| h.target.clone())
                .collect();
            targets
        };
        // In one read the second head starts past the first one's body, so its end counts from there.
        assert_eq!(targets(&[&stream]), ["/a", "/b", "/c"]);
        // Split inside the second head: the second read finishes the held bytes and carries the third whole, so
        // the third is found only where the end counts past the held bytes.
        let split = head("/a").len() + 20;
        assert_eq!(
            targets(&[&stream[..split], &stream[split..]]),
            ["/a", "/b", "/c"]
        );
    }

    #[test]
    fn a_head_matches_only_the_request_it_was_read_from() {
        let (head, _) = OriginalHead::parse(HEAD).expect("the head parses");
        let request = |method: &str, target: &str| {
            Request::builder()
                .method(method)
                .uri(target)
                .body(())
                .unwrap()
        };
        assert!(head.matches(&request("POST", "/v1/messages")));
        assert!(
            !head.matches(&request("POST", "/v1/messages/count_tokens")),
            "another target on the same connection is not this head"
        );
        assert!(!head.matches(&request("GET", "/v1/messages")));
    }

    #[test]
    fn a_head_with_more_slots_than_the_parser_holds_is_refused() {
        let mut raw = b"POST / HTTP/1.1\r\n".to_vec();
        for n in 0..=MAX_HEADERS {
            raw.extend_from_slice(format!("X-{n}: v\r\n").as_bytes());
        }
        raw.extend_from_slice(b"\r\n");
        assert_eq!(OriginalHead::parse(&raw), None);
    }
}
