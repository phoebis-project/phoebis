use fake_upstream::{FakeUpstream, Respond};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PATH: &str = "/v1/messages";

fn json(body: &str) -> Respond {
    Respond::Json {
        status: 200,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

/// One request per connection, asking the fake to close after it, so the whole response can be read to the end
/// without guessing where it stops.
async fn request(addr: SocketAddr, credential: Option<&str>) -> String {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    let auth = match credential {
        Some(c) => format!("Authorization: Bearer {c}\r\n"),
        None => String::new(),
    };
    conn.write_all(
        format!("GET {PATH} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    conn.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

#[tokio::test]
async fn a_credential_rule_answers_that_credential_and_no_other() {
    let up = FakeUpstream::start_plain(vec![]).await;
    let addr = up.addr();
    up.add_credential_rule("token-a", PATH, json("standing-a"));
    up.add_credential_rule("token-b", PATH, json("standing-b"));
    up.push_credential_rule("token-a", PATH, json("queued-a"));
    // Due on token-b's second request, so a foreign request that bumped its counter would surface here.
    up.set_credential_rule_after("token-b", 1, PATH, json("switched-b"));

    let stranger = request(addr, Some("token-c")).await;
    assert!(
        stranger.contains("404") && stranger.contains("no rule matches /v1/messages"),
        "an unknown credential must not be served another account's rule: {stranger}"
    );
    let anonymous = request(addr, None).await;
    assert!(
        anonymous.contains("404") && anonymous.contains("no rule matches /v1/messages"),
        "a request with no credential must not be served a credential rule: {anonymous}"
    );

    assert!(
        request(addr, Some("token-a")).await.ends_with("queued-a"),
        "the unmatched requests must not have popped token-a's queue"
    );
    assert!(request(addr, Some("token-a")).await.ends_with("standing-a"));
    assert!(
        request(addr, Some("token-b")).await.ends_with("standing-b"),
        "the unmatched requests must not have bumped token-b's request count"
    );
    assert!(request(addr, Some("token-b")).await.ends_with("switched-b"));

    up.shutdown().await;
}
