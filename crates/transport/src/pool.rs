//! Keep-alive pool for the inference plane: one idle list per (account, host), bounded, with an idle TTL.

use boring::ssl::SslConnector;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

use crate::{connect_tls, TransportError};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct PoolKey {
    pub account_id: String,
    pub host: String,
}
pub type Conn = tokio_boring::SslStream<TcpStream>;

pub struct Pool {
    connector: SslConnector,
    idle: Mutex<HashMap<PoolKey, Vec<(Conn, Instant)>>>,
    max_idle: usize,
    ttl: Duration,
}

impl Pool {
    pub fn new(connector: SslConnector, max_idle_per_key: usize, idle_ttl: Duration) -> Self {
        Self {
            connector,
            idle: Mutex::new(HashMap::new()),
            max_idle: max_idle_per_key,
            ttl: idle_ttl,
        }
    }

    /// Hands out a connection for the key, and reports whether it came from the idle list. An idle entry is
    /// probed before it is handed out and dropped unless the socket is open and quiet, because a request
    /// written into a connection the upstream closed while it sat idle looks exactly like a lost response and
    /// so has to be failed rather than sent again.
    pub async fn checkout(
        &self,
        key: PoolKey,
        host: &str,
        addr: SocketAddr,
    ) -> Result<(Conn, bool), TransportError> {
        let reused = {
            let mut idle = self.idle.lock().unwrap();
            idle.get_mut(&key).and_then(|v| {
                while let Some((c, at)) = v.pop() {
                    if at.elapsed() < self.ttl && alive(&c) {
                        return Some(c);
                    }
                }
                None
            })
        };
        if let Some(c) = reused {
            return Ok((c, true));
        }
        connect_tls(&self.connector, host, addr)
            .await
            .map(|c| (c, false))
    }

    pub fn checkin(&self, key: PoolKey, conn: Conn) {
        let mut idle = self.idle.lock().unwrap();
        let v = idle.entry(key).or_default();
        if v.len() < self.max_idle {
            v.push((conn, Instant::now()));
        }
    }
}

/// Whether an idle connection can still carry a request. `WouldBlock` is the only answer an open and quiet
/// socket gives; an end of stream, a close_notify alert, or any other byte waiting on a connection nobody has
/// a request on all mean the same thing, that the next request must go somewhere else.
fn alive(conn: &Conn) -> bool {
    let mut probe = [0u8; 1];
    matches!(conn.get_ref().try_read(&mut probe), Err(error) if error.kind() == ErrorKind::WouldBlock)
}
