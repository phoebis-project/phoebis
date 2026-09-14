//! Test support: capture the ClientHello a connector emits and mask its random fields.

use boring::ssl::SslConnector;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

pub async fn capture_client_hello(connector: &SslConnector, sni: &str) -> Vec<u8> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        // A peer that never completes the handshake must not hang the test; fail fast instead.
        tokio::time::timeout(Duration::from_secs(5), async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let mut n = 0;
            while n < 5 {
                let r = sock.read(&mut buf[n..]).await.unwrap();
                assert!(r > 0, "peer closed the connection after {n} bytes, need at least 5 for the record header");
                n += r;
            }
            let need = 5 + u16::from_be_bytes([buf[3], buf[4]]) as usize;
            while n < need {
                let r = sock.read(&mut buf[n..]).await.unwrap();
                assert!(r > 0, "peer closed the connection after {n} of {need} bytes");
                n += r;
            }
            buf.truncate(need);
            buf
        })
        .await
        .expect("no ClientHello within 5 s")
    });
    let mut cfg = connector.configure().unwrap();
    cfg.set_verify_hostname(false);
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    // The server never answers, so the handshake fails; the record is what matters.
    let _ = tokio_boring::connect(cfg, sni, tcp).await;
    server.await.unwrap()
}

/// Zeroes the client random, the legacy session id, and every key-share payload so two hellos compare structurally.
pub fn mask(hello: &[u8]) -> Vec<u8> {
    let mut h = hello.to_vec();
    // record(5) + handshake header(4) + version(2) = 11; random follows.
    let random = 11;
    assert!(
        h.len() >= random + 32,
        "ClientHello truncated before the client random ends at offset {}",
        random + 32
    );
    h[random..random + 32].fill(0);
    assert!(
        h.len() > random + 32,
        "ClientHello truncated at the session id length, offset {}",
        random + 32
    );
    let sid_len = h[random + 32] as usize;
    let sid = random + 33;
    assert!(
        h.len() >= sid + sid_len,
        "ClientHello truncated before the session id ends at offset {}",
        sid + sid_len
    );
    h[sid..sid + sid_len].fill(0);
    let mut p = sid + sid_len;
    assert!(
        h.len() >= p + 2,
        "ClientHello truncated at the cipher suite length, offset {p}"
    );
    let cipher_len = u16::from_be_bytes([h[p], h[p + 1]]) as usize;
    p += 2 + cipher_len;
    assert!(
        h.len() > p,
        "ClientHello truncated at the compression method length, offset {p}"
    );
    let comp_len = h[p] as usize;
    p += 1 + comp_len;
    assert!(
        h.len() >= p + 2,
        "ClientHello truncated at the extensions length, offset {p}"
    );
    let ext_total = u16::from_be_bytes([h[p], h[p + 1]]) as usize;
    p += 2;
    let end = p + ext_total;
    assert!(
        h.len() >= end,
        "ClientHello truncated before the extensions block ends at offset {end}"
    );
    while p + 4 <= end {
        let ty = u16::from_be_bytes([h[p], h[p + 1]]);
        let len = u16::from_be_bytes([h[p + 2], h[p + 3]]) as usize;
        let data = p + 4;
        assert!(
            h.len() >= data + len,
            "ClientHello truncated inside extension {ty}, offset {}",
            data + len
        );
        if ty == 51 {
            let mut q = data + 2;
            while q + 4 <= data + len {
                let klen = u16::from_be_bytes([h[q + 2], h[q + 3]]) as usize;
                assert!(
                    h.len() >= q + 4 + klen,
                    "ClientHello truncated inside a key share payload, offset {}",
                    q + 4 + klen
                );
                h[q + 4..q + 4 + klen].fill(0);
                q += 4 + klen;
            }
        }
        p = data + len;
    }
    h
}
