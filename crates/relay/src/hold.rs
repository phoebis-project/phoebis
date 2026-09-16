//! The pre-commit hold: events before `message_start` or the first `error` are kept back so a rate-limit error
//! delivered as a 200 stream can still fail over without a byte having reached the caller.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use phoebis_transport::decode::Decoder;
use phoebis_transport::response::BodyReader;
use phoebis_transport::TransportError;
use tokio::io::AsyncRead;

use crate::sse::{Overflow, SseEvent, SseParser};

pub const HOLD_BUFFER: usize = 64 * 1024;
pub const INSPECT_BUFFER: usize = 64 * 1024;

#[derive(Debug)]
pub enum Commit {
    EndOfStream,
    Error(Bytes),
    MessageStart(Bytes),
    Overflow,
    Timeout,
}

/// The decoded view of a stream whose raw bytes are forwarded untouched.
pub struct Inspect {
    pub decoder: Option<Box<dyn Decoder>>,
    pub parser: SseParser,
}

impl Inspect {
    pub fn new(decoder: Option<Box<dyn Decoder>>) -> Inspect {
        Inspect {
            decoder,
            parser: SseParser::new(INSPECT_BUFFER),
        }
    }

    pub fn feed_raw(&mut self, raw: &[u8]) -> Result<(), Overflow> {
        match self.decoder.as_mut() {
            None => self.parser.feed(raw),
            // A copy that will not decode is not one the relay can act on; the caller treats it as an overflow.
            Some(decoder) => match decoder.feed(raw) {
                Ok(out) => self.parser.feed(&out),
                Err(_) => Err(Overflow),
            },
        }
    }
}

pub struct Held {
    pub commit: Commit,
    pub bytes: Bytes,
    pub inspect: Inspect,
}

pub async fn hold<R: AsyncRead + Unpin>(
    reader: &mut BodyReader<R>,
    mut inspect: Inspect,
    timeout: Duration,
) -> Result<Held, TransportError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut raw = BytesMut::new();
    loop {
        // Events are taken in stream order, so the first committing one decides and a later one is post-commit.
        while let Some(event) = inspect.parser.next_event() {
            match event.name.as_deref() {
                Some(b"message_start") => {
                    return Ok(Held {
                        commit: Commit::MessageStart(Bytes::from(event.data)),
                        bytes: raw.freeze(),
                        inspect,
                    })
                }
                Some(b"error") => {
                    return Ok(Held {
                        commit: Commit::Error(Bytes::from(event.data)),
                        bytes: raw.freeze(),
                        inspect,
                    })
                }
                _ => {}
            }
        }
        if raw.len() > HOLD_BUFFER {
            return Ok(Held {
                commit: Commit::Overflow,
                bytes: raw.freeze(),
                inspect,
            });
        }
        let next = match tokio::time::timeout_at(deadline, reader.next_chunk()).await {
            Err(_) => {
                return Ok(Held {
                    commit: Commit::Timeout,
                    bytes: raw.freeze(),
                    inspect,
                })
            }
            Ok(next) => next?,
        };
        let Some(chunk) = next else {
            // A stream cut off after its last event never sent the empty line that would close it, so the
            // committing event can still be sitting in the parser's buffer.
            let commit = match inspect.parser.finish() {
                Some(SseEvent { name, data }) => match name.as_deref() {
                    Some(b"message_start") => Commit::MessageStart(Bytes::from(data)),
                    Some(b"error") => Commit::Error(Bytes::from(data)),
                    _ => Commit::EndOfStream,
                },
                None => Commit::EndOfStream,
            };
            return Ok(Held {
                commit,
                bytes: raw.freeze(),
                inspect,
            });
        };
        raw.extend_from_slice(&chunk);
        if inspect.feed_raw(&chunk).is_err() {
            return Ok(Held {
                commit: Commit::Overflow,
                bytes: raw.freeze(),
                inspect,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_transport::response::{BodyReader, Framing};
    use std::io::Write;
    use std::time::Duration;
    use tokio::io::{duplex, AsyncWriteExt};

    fn event(name: &str, data: &str) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }

    async fn held_from(chunks: Vec<Vec<u8>>, inspect: Inspect, timeout: Duration) -> Held {
        let (mut tx, rx) = duplex(1 << 16);
        tokio::spawn(async move {
            for chunk in chunks {
                tx.write_all(&chunk).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // The writer stays open past the last chunk so the hold has to decide on its own.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let mut reader = BodyReader::new(rx, Default::default(), Framing::UntilClose);
        hold(&mut reader, inspect, timeout).await.unwrap()
    }

    fn plain(chunks: &[&str]) -> Vec<Vec<u8>> {
        chunks.iter().map(|c| c.as_bytes().to_vec()).collect()
    }

    #[tokio::test]
    async fn a_ping_is_held_until_message_start_commits() {
        let held = held_from(
            plain(&[
                &event("ping", r#"{"type":"ping"}"#),
                &event("message_start", r#"{"type":"message_start"}"#),
                &event("content_block_delta", "{}"),
            ]),
            Inspect::new(None),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(held.commit, Commit::MessageStart(_)));
        assert!(
            std::str::from_utf8(&held.bytes)
                .unwrap()
                .starts_with("event: ping\n"),
            "the ping is first in the held bytes"
        );
        assert!(
            std::str::from_utf8(&held.bytes)
                .unwrap()
                .contains("event: message_start\n"),
            "the committing event is part of the held bytes"
        );
    }

    #[tokio::test]
    async fn an_error_before_message_start_commits_with_its_data_and_an_event_split_across_chunks_is_reassembled(
    ) {
        let error = event(
            "error",
            r#"{"type":"error","error":{"type":"rate_limit_error"}}"#,
        );
        let (head, tail) = error.split_at(20);
        let held = held_from(
            plain(&[&event("ping", "{}"), head, tail]),
            Inspect::new(None),
            Duration::from_secs(1),
        )
        .await;
        let Commit::Error(data) = held.commit else {
            panic!("expected an error commit")
        };
        assert_eq!(
            data.as_ref(),
            br#"{"type":"error","error":{"type":"rate_limit_error"}}"#
        );
    }

    #[tokio::test]
    async fn a_crlf_message_start_followed_by_an_lf_error_commits_on_message_start() {
        let chunks = plain(&["event: message_start\r\ndata: {}\r\n\r\n", "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}\n\n"]);
        let held = held_from(chunks, Inspect::new(None), Duration::from_secs(1)).await;
        assert!(
            matches!(held.commit, Commit::MessageStart(_)),
            "the first committing event wins; the later error is post-commit"
        );
        let crlf = plain(&[
            "event: ping\r\ndata: {}\r\n\r\nevent: message_start\r\n",
            "data: {}\r\n\r\n",
        ]);
        let held = held_from(crlf, Inspect::new(None), Duration::from_secs(1)).await;
        assert!(
            matches!(held.commit, Commit::MessageStart(_)),
            "a CRLF-only stream commits without waiting for EOF"
        );
    }

    #[tokio::test]
    async fn a_compressed_stream_is_inspected_decoded_and_held_raw() {
        let text = format!("{}{}", event("ping", "{}"), event("message_start", "{}"));
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).unwrap();
        let gz = encoder.finish().unwrap();
        let held = held_from(
            vec![gz.clone()],
            Inspect::new(Some(phoebis_transport::decode::decoder(
                phoebis_transport::decode::Encoding::Gzip,
            ))),
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(held.commit, Commit::MessageStart(_)));
        assert_eq!(
            held.bytes.as_ref(),
            gz.as_slice(),
            "the held bytes are the wire bytes"
        );
    }

    /// Unlike `held_from`, the writer is dropped after the last chunk, so the body ends instead of going quiet.
    async fn held_to_end_of_body(chunks: Vec<Vec<u8>>) -> Held {
        let (mut tx, rx) = duplex(1 << 16);
        tokio::spawn(async move {
            for chunk in chunks {
                tx.write_all(&chunk).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut reader = BodyReader::new(rx, Default::default(), Framing::UntilClose);
        hold(&mut reader, Inspect::new(None), Duration::from_secs(1))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_end_of_the_body_commits_on_a_last_event_missing_its_empty_line() {
        let ping = event("ping", "{}");
        let held = held_to_end_of_body(plain(&[&ping])).await;
        assert!(matches!(held.commit, Commit::EndOfStream));
        assert_eq!(held.bytes.as_ref(), ping.as_bytes());
        let held = held_to_end_of_body(plain(&[&ping, "event: message_start\ndata: {}"])).await;
        assert!(
            matches!(held.commit, Commit::MessageStart(_)),
            "a truncated stream still commits on the event it did deliver"
        );
    }

    #[tokio::test]
    async fn the_hold_gives_up_on_time_and_on_either_bound() {
        let held = held_from(
            plain(&[&event("ping", "{}")]),
            Inspect::new(None),
            Duration::from_millis(100),
        )
        .await;
        assert!(matches!(held.commit, Commit::Timeout));
        assert_eq!(held.bytes.as_ref(), event("ping", "{}").as_bytes());
        let big = event("weird", &"x".repeat(HOLD_BUFFER));
        let held = held_from(plain(&[&big]), Inspect::new(None), Duration::from_secs(1)).await;
        assert!(matches!(held.commit, Commit::Overflow));
        assert!(held.bytes.len() > HOLD_BUFFER);
        // 16 KiB on the wire that decode to far more than the inspection bound: the decoded copy overflows first.
        let text = format!("event: weird\ndata: {}\n", "y".repeat(4 * INSPECT_BUFFER));
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(text.as_bytes()).unwrap();
        let gz = encoder.finish().unwrap();
        assert!(gz.len() < HOLD_BUFFER);
        let held = held_from(
            vec![gz],
            Inspect::new(Some(phoebis_transport::decode::decoder(
                phoebis_transport::decode::Encoding::Gzip,
            ))),
            Duration::from_secs(1),
        )
        .await;
        assert!(
            matches!(held.commit, Commit::Overflow),
            "the decoded copy is bounded on its own"
        );
    }
}
