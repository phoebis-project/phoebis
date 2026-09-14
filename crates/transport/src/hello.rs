//! Test support: capture the ClientHello a connector emits and mask its random fields.

use boring::ssl::SslConnector;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

pub async fn capture_client_hello(connector: &SslConnector, sni: &str) -> Vec<u8> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let mut n = 0;
        while n < 5 {
            n += sock.read(&mut buf[n..]).await.unwrap();
        }
        let need = 5 + u16::from_be_bytes([buf[3], buf[4]]) as usize;
        while n < need {
            n += sock.read(&mut buf[n..]).await.unwrap();
        }
        buf.truncate(need);
        buf
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
    h[random..random + 32].fill(0);
    let sid_len = h[random + 32] as usize;
    let sid = random + 33;
    h[sid..sid + sid_len].fill(0);
    let mut p = sid + sid_len;
    let cipher_len = u16::from_be_bytes([h[p], h[p + 1]]) as usize;
    p += 2 + cipher_len;
    let comp_len = h[p] as usize;
    p += 1 + comp_len;
    let ext_total = u16::from_be_bytes([h[p], h[p + 1]]) as usize;
    p += 2;
    let end = p + ext_total;
    while p + 4 <= end {
        let ty = u16::from_be_bytes([h[p], h[p + 1]]);
        let len = u16::from_be_bytes([h[p + 2], h[p + 3]]) as usize;
        let data = p + 4;
        if ty == 51 {
            let mut q = data + 2;
            while q + 4 <= data + len {
                let klen = u16::from_be_bytes([h[q + 2], h[q + 3]]) as usize;
                h[q + 4..q + 4 + klen].fill(0);
                q += 4 + klen;
            }
        }
        p = data + len;
    }
    h
}
