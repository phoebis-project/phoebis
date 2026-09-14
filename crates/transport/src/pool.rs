//! Keep-alive pool for the inference plane: one idle list per (account, host), bounded, with an idle TTL.

use boring::ssl::SslConnector;
use std::collections::HashMap;
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

    pub async fn checkout(
        &self,
        key: PoolKey,
        host: &str,
        addr: SocketAddr,
    ) -> Result<Conn, TransportError> {
        let reused = {
            let mut idle = self.idle.lock().unwrap();
            idle.get_mut(&key).and_then(|v| {
                while let Some((c, at)) = v.pop() {
                    if at.elapsed() < self.ttl {
                        return Some(c);
                    }
                }
                None
            })
        };
        if let Some(c) = reused {
            return Ok(c);
        }
        connect_tls(&self.connector, host, addr).await
    }

    pub fn checkin(&self, key: PoolKey, conn: Conn) {
        let mut idle = self.idle.lock().unwrap();
        let v = idle.entry(key).or_default();
        if v.len() < self.max_idle {
            v.push((conn, Instant::now()));
        }
    }
}
