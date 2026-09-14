//! Upstream response head parsing and body framing, streaming payload bytes as they arrive.

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::TransportError;

const MAX_HEAD: usize = 64 * 1024;

pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub async fn read_head<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut BytesMut,
) -> Result<ResponseHead, TransportError> {
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
                    continue;
                }
                return Ok(head);
            }
            httparse::Status::Partial => {
                if buf.len() > MAX_HEAD {
                    return Err(TransportError::Response("head too large".into()));
                }
                let n = r.read_buf(buf).await?;
                if n == 0 {
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

/// `chunk_left` is the number of payload bytes still owed by the chunk being streamed; `Some(0)` means the payload
/// is complete and only its terminating CRLF is outstanding.
pub struct BodyReader<R> {
    r: R,
    buf: BytesMut,
    framing: Framing,
    remaining: u64,
    chunk_left: Option<usize>,
    done: bool,
}

impl<R: AsyncRead + Unpin> BodyReader<R> {
    pub fn new(r: R, leftover: BytesMut, framing: Framing) -> Self {
        let remaining = if let Framing::ContentLength(n) = framing {
            n
        } else {
            0
        };
        Self {
            r,
            buf: leftover,
            framing,
            remaining,
            chunk_left: None,
            done: false,
        }
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, TransportError> {
        if self.done {
            return Ok(None);
        }
        match self.framing {
            Framing::ContentLength(_) => {
                if self.remaining == 0 {
                    self.done = true;
                    return Ok(None);
                }
                if self.buf.is_empty() {
                    // A prior split_to may have handed the buffer's capacity to the returned Bytes.
                    self.buf.reserve(8 * 1024);
                    if self.r.read_buf(&mut self.buf).await? == 0 {
                        return Err(TransportError::Response(
                            "connection closed inside body".into(),
                        ));
                    }
                }
                let take = (self.buf.len() as u64).min(self.remaining) as usize;
                self.remaining -= take as u64;
                Ok(Some(self.buf.split_to(take).freeze()))
            }
            Framing::UntilClose => {
                if self.buf.is_empty() {
                    // A prior split may have handed the buffer's capacity to the returned Bytes.
                    self.buf.reserve(8 * 1024);
                    if self.r.read_buf(&mut self.buf).await? == 0 {
                        self.done = true;
                        return Ok(None);
                    }
                }
                Ok(Some(self.buf.split().freeze()))
            }
            Framing::Chunked => self.next_chunked().await,
        }
    }

    async fn fill(&mut self) -> Result<(), TransportError> {
        // A prior split_to may have handed the buffer's capacity to a returned Bytes.
        self.buf.reserve(8 * 1024);
        if self.r.read_buf(&mut self.buf).await? == 0 {
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
                loop {
                    if let Some(p) = self.buf.windows(2).position(|w| w == b"\r\n") {
                        self.buf.advance(p + 2);
                        if p == 0 {
                            break;
                        }
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
}
