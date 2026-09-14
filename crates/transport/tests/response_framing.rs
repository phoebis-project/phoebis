use std::sync::Arc;

use bytes::BytesMut;
use phoebis_transport::response::{framing, read_head, BodyReader, Framing};
use tokio::io::duplex;
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;

async fn collect(raw: &'static [u8]) -> (u16, Vec<Vec<u8>>) {
    let (mut tx, mut rx) = duplex(64);
    tokio::spawn(async move {
        tx.write_all(raw).await.unwrap();
    });
    let mut buf = BytesMut::new();
    let head = read_head(&mut rx, &mut buf).await.unwrap();
    let f = framing(&head);
    let mut body = BodyReader::new(rx, buf, f);
    let mut chunks = Vec::new();
    while let Some(c) = body.next_chunk().await.unwrap() {
        chunks.push(c.to_vec());
    }
    (head.status, chunks)
}

#[tokio::test]
async fn content_length_body_is_delivered_completely() {
    let (status, chunks) =
        collect(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world").await;
    assert_eq!(status, 200);
    assert_eq!(chunks.concat(), b"hello world");
}

#[tokio::test]
async fn chunked_body_streams_payload_before_the_terminator_arrives() {
    let (mut tx, mut rx) = duplex(256);
    let gate = Arc::new(Notify::new());
    let writer_gate = gate.clone();
    tokio::spawn(async move {
        tx.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n")
            .await
            .unwrap();
        writer_gate.notified().await;
        tx.write_all(b"6\r\n world\r\n0\r\n\r\n").await.unwrap();
    });
    let mut buf = BytesMut::new();
    let head = read_head(&mut rx, &mut buf).await.unwrap();
    let mut body = BodyReader::new(rx, buf, framing(&head));
    let first = body
        .next_chunk()
        .await
        .unwrap()
        .expect("payload arrives before the terminator is written");
    assert!(!first.is_empty());
    gate.notify_one();
    let mut all = first.to_vec();
    while let Some(c) = body.next_chunk().await.unwrap() {
        all.extend_from_slice(&c);
    }
    assert_eq!(all, b"hello world");
}

#[tokio::test]
async fn sse_events_split_across_chunks_arrive_in_order() {
    // 0x16 is the 22 bytes of the first fragment, 0xd the 13 bytes of the second.
    let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n16\r\nevent: ping\ndata: {\"ty\r\nd\r\npe\":\"ping\"}\n\n\r\n0\r\n\r\n";
    let (_, chunks) = collect(raw).await;
    assert_eq!(
        chunks.concat(),
        b"event: ping\ndata: {\"type\":\"ping\"}\n\n"
    );
}

#[test]
fn framing_prefers_chunked_over_content_length() {
    let head = phoebis_transport::response::ResponseHead {
        status: 200,
        headers: vec![
            ("Content-Length".into(), "5".into()),
            ("Transfer-Encoding".into(), "chunked".into()),
        ],
    };
    assert!(matches!(framing(&head), Framing::Chunked));
}
