use fake_upstream::{FakeUpstream, Respond, Rule, SseEvent};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn events() -> Vec<SseEvent> {
    vec![
        SseEvent {
            event: "first".into(),
            data: "{}".into(),
        },
        SseEvent {
            event: "second".into(),
            data: "{}".into(),
        },
    ]
}

/// The exact bytes a chunk carries for one event, trailing CRLF included: reading up to and including this
/// guarantees the whole first chunk (not just part of its payload) has been consumed.
fn rendered_chunk(e: &SseEvent) -> String {
    format!("event: {}\ndata: {}\n\n\r\n", e.event, e.data)
}

async fn open_gated(addr: SocketAddr) -> TcpStream {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    conn.write_all(b"GET /x HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    conn
}

async fn read_until_contains(conn: &mut TcpStream, needle: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        if buf.windows(needle.len()).any(|w| w == needle) {
            return buf;
        }
        let n = conn.read(&mut chunk).await.unwrap();
        assert!(
            n > 0,
            "connection closed before {:?} was seen",
            String::from_utf8_lossy(needle)
        );
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn assert_no_more_data_yet(conn: &mut TcpStream) {
    let mut chunk = [0u8; 64];
    let raced = tokio::time::timeout(Duration::from_millis(200), conn.read(&mut chunk)).await;
    assert!(raced.is_err(), "more data arrived before release()");
}

async fn read_to_close(conn: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    conn.read_to_end(&mut buf).await.unwrap();
    buf
}

fn gated_rule() -> Rule {
    Rule {
        path_prefix: "/x".into(),
        respond: Respond::SseGated {
            status: 200,
            headers: vec![],
            events: events(),
        },
    }
}

#[tokio::test]
async fn one_release_call_wakes_every_gated_connection() {
    let up = FakeUpstream::start_plain(vec![gated_rule()]).await;
    let addr = up.addr();

    let mut a = open_gated(addr).await;
    let mut b = open_gated(addr).await;

    let first_chunk = rendered_chunk(&events()[0]);
    read_until_contains(&mut a, first_chunk.as_bytes()).await;
    read_until_contains(&mut b, first_chunk.as_bytes()).await;
    assert_no_more_data_yet(&mut a).await;
    assert_no_more_data_yet(&mut b).await;

    up.release();

    let rest_a = read_to_close(&mut a).await;
    let rest_b = read_to_close(&mut b).await;
    assert!(
        rest_a.ends_with(b"0\r\n\r\n"),
        "connection a did not reach the terminating chunk"
    );
    assert!(
        rest_b.ends_with(b"0\r\n\r\n"),
        "connection b did not reach the terminating chunk"
    );
    assert!(
        String::from_utf8_lossy(&rest_a).contains("event: second"),
        "connection a never received the event withheld by the gate"
    );
    assert!(
        String::from_utf8_lossy(&rest_b).contains("event: second"),
        "connection b never received the event withheld by the gate"
    );

    up.shutdown().await;
}

#[tokio::test]
async fn a_release_before_any_connection_still_opens_the_gate() {
    let up = FakeUpstream::start_plain(vec![gated_rule()]).await;
    let addr = up.addr();

    up.release();

    let mut conn = open_gated(addr).await;
    let all = read_to_close(&mut conn).await;
    assert!(
        all.ends_with(b"0\r\n\r\n"),
        "the response never completed after a release() that landed before the connection existed"
    );
    assert!(
        String::from_utf8_lossy(&all).contains("event: second"),
        "the withheld event never arrived, though release() had already been called once"
    );

    up.shutdown().await;
}

/// A waiter that comes before the request, one that comes after the write and a second one for the same response
/// all resolve, and none of them before the response is whole: the signal latches rather than waking only the
/// waiters that happen to be parked.
#[tokio::test]
async fn written_holds_until_the_response_is_whole_and_latches_for_every_later_waiter() {
    let up = FakeUpstream::start_plain(vec![gated_rule()]).await;
    {
        let early = up.written(0);
        tokio::pin!(early);
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut early)
                .await
                .is_err(),
            "no request has arrived"
        );

        let mut conn = open_gated(up.addr()).await;
        read_until_contains(&mut conn, rendered_chunk(&events()[0]).as_bytes()).await;
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut early)
                .await
                .is_err(),
            "the gate still holds the rest of the response"
        );

        up.release();
        read_to_close(&mut conn).await;
        tokio::time::timeout(Duration::from_secs(5), early)
            .await
            .expect("the waiter that came first sees the write");
    }
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(5), up.written(0))
            .await
            .expect("a waiter after the write resolves at once");
    }

    up.shutdown().await;
}
