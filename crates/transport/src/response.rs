//! Upstream response head parsing and body framing, streaming payload bytes as they arrive.

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::TransportError;

const MAX_HEAD: usize = 64 * 1024;
// Bounds a streaming-1xx upstream: MAX_HEAD only caps the size of the head being parsed, not how many can precede it.
const MAX_INTERIM: u32 = 8;

/// An upstream declares a chunk in a few bytes; a size line or a trailer block past these is a peer that
/// stopped speaking HTTP, and reading on would only grow the buffer until the idle timeout.
pub const MAX_CHUNK_LINE: usize = 1024;
pub const MAX_TRAILERS: usize = 16 * 1024;

pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    pub fn get(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }
}

/// The first value under `name` in a recorded header list, matched without regard to case, for every head the
/// relay keeps as name and value pairs.
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub async fn read_head<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut BytesMut,
) -> Result<ResponseHead, TransportError> {
    let mut interim = 0u32;
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 128];
        let mut resp = httparse::Response::new(&mut headers);
        match resp
            .parse(buf)
            .map_err(|e| TransportError::Response(e.to_string()))?
        {
            httparse::Status::Complete(n) => {
                let head = ResponseHead {
                    status: resp.code.unwrap_or(0),
                    headers: resp
                        .headers
                        .iter()
                        .map(|h| {
                            (
                                h.name.to_string(),
                                String::from_utf8_lossy(h.value).into_owned(),
                            )
                        })
                        .collect(),
                };
                buf.advance(n);
                // 1xx responses precede the real head (e.g. 100 Continue, 103 Early Hints); skip past them.
                if (100..200).contains(&head.status) {
                    interim += 1;
                    if interim > MAX_INTERIM {
                        return Err(TransportError::Response(
                            "too many interim responses".into(),
                        ));
                    }
                    continue;
                }
                return Ok(head);
            }
            httparse::Status::Partial => {
                if buf.len() > MAX_HEAD {
                    return Err(TransportError::Response("head too large".into()));
                }
                if !refill(r, buf).await? {
                    return Err(TransportError::Response(
                        "connection closed before head".into(),
                    ));
                }
            }
        }
    }
}

pub enum Framing {
    ContentLength(u64),
    Chunked,
    UntilClose,
}

pub fn framing(head: &ResponseHead) -> Framing {
    // 204 and 304 never carry a body, regardless of any Content-Length or Transfer-Encoding claimed; otherwise the
    // body reader would wait on bytes the peer is never going to send.
    if matches!(head.status, 204 | 304) {
        return Framing::ContentLength(0);
    }
    if head
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        return Framing::Chunked;
    }
    if let Some(n) = head
        .get("content-length")
        .and_then(|v| v.trim().parse().ok())
    {
        return Framing::ContentLength(n);
    }
    Framing::UntilClose
}

pub fn keep_alive(head: &ResponseHead, framing: &Framing) -> bool {
    !matches!(framing, Framing::UntilClose)
        && !head
            .get("connection")
            .map(|v| v.eq_ignore_ascii_case("close"))
            .unwrap_or(false)
}

/// Reads more into `buf` and answers whether any bytes arrived. Free rather than a method, since
/// `next_chunk` calls it while it holds a borrow of the reader's framing.
async fn refill<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut BytesMut,
) -> Result<bool, TransportError> {
    // A prior split may have handed the buffer's capacity to a returned Bytes.
    buf.reserve(8 * 1024);
    Ok(r.read_buf(buf).await? != 0)
}

/// `chunk_left` is the number of payload bytes still owed by the chunk being streamed; `Some(0)` means the payload
/// is complete and only its terminating CRLF is outstanding.
pub struct BodyReader<R> {
    r: R,
    buf: BytesMut,
    framing: Framing,
    chunk_left: Option<usize>,
    done: bool,
}

impl<R: AsyncRead + Unpin> BodyReader<R> {
    pub fn new(r: R, leftover: BytesMut, framing: Framing) -> Self {
        Self {
            r,
            buf: leftover,
            framing,
            chunk_left: None,
            done: false,
        }
    }

    /// Gives the source back once the body has ended, so a caller that owns a pooled connection can return it.
    /// Calling this earlier hands back a stream still positioned inside the body.
    pub fn into_inner(self) -> R {
        self.r
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, TransportError> {
        if self.done {
            return Ok(None);
        }
        match &mut self.framing {
            Framing::ContentLength(remaining) => {
                if *remaining == 0 {
                    self.done = true;
                    return Ok(None);
                }
                if self.buf.is_empty() && !refill(&mut self.r, &mut self.buf).await? {
                    return Err(TransportError::Response(
                        "connection closed inside body".into(),
                    ));
                }
                let take = (self.buf.len() as u64).min(*remaining) as usize;
                *remaining -= take as u64;
                Ok(Some(self.buf.split_to(take).freeze()))
            }
            Framing::UntilClose => {
                if self.buf.is_empty() && !refill(&mut self.r, &mut self.buf).await? {
                    self.done = true;
                    return Ok(None);
                }
                Ok(Some(self.buf.split().freeze()))
            }
            Framing::Chunked => self.next_chunked().await,
        }
    }

    async fn fill(&mut self) -> Result<(), TransportError> {
        if !refill(&mut self.r, &mut self.buf).await? {
            return Err(TransportError::Response(
                "connection closed inside chunked body".into(),
            ));
        }
        Ok(())
    }

    /// Yields whatever payload of the current chunk has arrived. Waiting for the whole chunk would hold back the
    /// first SSE event of a long response and would let an upstream-declared chunk size dictate the buffer size.
    async fn next_chunked(&mut self) -> Result<Option<Bytes>, TransportError> {
        loop {
            if let Some(left) = self.chunk_left {
                if left == 0 {
                    if self.buf.len() < 2 {
                        self.fill().await?;
                        continue;
                    }
                    if &self.buf[..2] != b"\r\n" {
                        return Err(TransportError::Response(
                            "chunk not terminated by CRLF".into(),
                        ));
                    }
                    self.buf.advance(2);
                    self.chunk_left = None;
                    continue;
                }
                if self.buf.is_empty() {
                    self.fill().await?;
                    continue;
                }
                let take = self.buf.len().min(left);
                self.chunk_left = Some(left - take);
                return Ok(Some(self.buf.split_to(take).freeze()));
            }
            let Some(nl) = self.buf.windows(2).position(|w| w == b"\r\n") else {
                if self.buf.len() > MAX_CHUNK_LINE {
                    return Err(TransportError::Response("chunk framing too long".into()));
                }
                self.fill().await?;
                continue;
            };
            let size_line = std::str::from_utf8(&self.buf[..nl])
                .map_err(|e| TransportError::Response(e.to_string()))?;
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| TransportError::Response("bad chunk size".into()))?;
            self.buf.advance(nl + 2);
            if size == 0 {
                // Trailers, if any, end with an empty line.
                let mut trailer_bytes = 0usize;
                loop {
                    if let Some(p) = self.buf.windows(2).position(|w| w == b"\r\n") {
                        trailer_bytes += p + 2;
                        if trailer_bytes > MAX_TRAILERS {
                            return Err(TransportError::Response("chunk framing too long".into()));
                        }
                        self.buf.advance(p + 2);
                        if p == 0 {
                            break;
                        }
                    } else if self.buf.len() > MAX_CHUNK_LINE
                        || trailer_bytes + self.buf.len() > MAX_TRAILERS
                    {
                        return Err(TransportError::Response("chunk framing too long".into()));
                    } else {
                        self.fill().await?;
                    }
                }
                self.done = true;
                return Ok(None);
            }
            self.chunk_left = Some(size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncWriteExt};

    #[tokio::test]
    async fn interim_response_is_skipped_for_the_real_head() {
        let (mut tx, mut rx) = duplex(256);
        tokio::spawn(async move {
            tx.write_all(
                b"HTTP/1.1 103 Early Hints\r\nLink: </style.css>; rel=preload\r\n\r\n\
                  HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello",
            )
            .await
            .unwrap();
        });
        let mut buf = BytesMut::new();
        let head = read_head(&mut rx, &mut buf).await.unwrap();
        assert_eq!(head.status, 200);
        let mut body = BodyReader::new(rx, buf, framing(&head));
        let mut all = Vec::new();
        while let Some(c) = body.next_chunk().await.unwrap() {
            all.extend_from_slice(&c);
        }
        assert_eq!(all, b"hello");
    }

    #[tokio::test]
    async fn no_content_response_ends_without_reading_further() {
        let (mut tx, mut rx) = duplex(64);
        tokio::spawn(async move {
            tx.write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .await
                .unwrap();
        });
        let mut buf = BytesMut::new();
        let head = read_head(&mut rx, &mut buf).await.unwrap();
        assert_eq!(head.status, 204);
        let f = framing(&head);
        assert!(matches!(f, Framing::ContentLength(0)));
        let mut body = BodyReader::new(rx, buf, f);
        assert!(body.next_chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn too_many_interim_responses_is_rejected() {
        let (mut tx, mut rx) = duplex(4096);
        tokio::spawn(async move {
            let heads = "HTTP/1.1 103 Early Hints\r\n\r\n".repeat(9);
            tx.write_all(heads.as_bytes()).await.unwrap();
        });
        let mut buf = BytesMut::new();
        let result = read_head(&mut rx, &mut buf).await;
        assert!(matches!(result, Err(TransportError::Response(_))));
    }

    #[tokio::test]
    async fn a_chunk_size_line_without_an_end_is_refused_at_the_line_bound() {
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let feeder = tokio::spawn(async move {
            writer.write_all(b"1;").await.unwrap();
            for _ in 0..64 {
                if writer.write_all(&[b'x'; 4096]).await.is_err() {
                    break;
                }
            }
        });
        let mut body = BodyReader::new(reader, BytesMut::new(), Framing::Chunked);
        let error = body.next_chunk().await.unwrap_err();
        assert!(
            error.to_string().contains("chunk framing too long"),
            "{error}"
        );
        feeder.abort();
    }

    #[tokio::test]
    async fn trailers_past_the_bound_are_refused() {
        let (mut writer, reader) = tokio::io::duplex(128 * 1024);
        let feeder = tokio::spawn(async move {
            writer.write_all(b"0\r\nX-Trailer: ").await.unwrap();
            for _ in 0..16 {
                if writer.write_all(&[b'x'; 4096]).await.is_err() {
                    break;
                }
            }
        });
        let mut body = BodyReader::new(reader, BytesMut::new(), Framing::Chunked);
        let error = body.next_chunk().await.unwrap_err();
        assert!(
            error.to_string().contains("chunk framing too long"),
            "{error}"
        );
        feeder.abort();
    }

    /// One trailer line under `MAX_CHUNK_LINE`, repeated `count` times, so the per-line bound never trips and
    /// only the running total decides.
    fn trailer_wire(count: usize) -> Vec<u8> {
        let line = format!("X-Trailer: {}\r\n", "a".repeat(985));
        let mut wire = b"5\r\nhello\r\n0\r\n".to_vec();
        for _ in 0..count {
            wire.extend_from_slice(line.as_bytes());
        }
        wire.extend_from_slice(b"\r\n");
        wire
    }

    #[tokio::test]
    async fn trailer_lines_each_under_the_line_bound_whose_sum_crosses_the_trailer_bound_are_refused(
    ) {
        // 17 lines of 998 bytes each: none trips MAX_CHUNK_LINE (1024) alone, but their sum (16,966) passes
        // MAX_TRAILERS (16,384).
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let feeder = tokio::spawn(async move {
            let _ = writer.write_all(&trailer_wire(17)).await;
        });
        let mut body = BodyReader::new(reader, BytesMut::new(), Framing::Chunked);
        assert_eq!(body.next_chunk().await.unwrap().unwrap().as_ref(), b"hello");
        let error = body.next_chunk().await.unwrap_err();
        assert!(
            error.to_string().contains("chunk framing too long"),
            "{error}"
        );
        feeder.abort();
    }

    #[tokio::test]
    async fn the_same_trailer_lines_summing_just_under_the_trailer_bound_are_accepted() {
        // 16 lines of 998 bytes: a sum of 15,968, under MAX_TRAILERS.
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        writer.write_all(&trailer_wire(16)).await.unwrap();
        drop(writer);
        let mut body = BodyReader::new(reader, BytesMut::new(), Framing::Chunked);
        assert_eq!(body.next_chunk().await.unwrap().unwrap().as_ref(), b"hello");
        assert!(
            body.next_chunk().await.unwrap().is_none(),
            "the body ends cleanly once the trailers are read"
        );
    }
}
