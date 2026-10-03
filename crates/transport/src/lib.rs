//! TLS profiles, ordered HTTP/1.1 writer, response reader, keep-alive pool.

pub mod control;
pub mod decode;
pub mod encode;
pub mod hello;
pub mod pool;
pub mod request;
pub mod response;
mod tls;

pub use tls::{connect_tls, connector, connector_builder, trust_extra_ca, Plane};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("tls configuration: {0}")]
    Boring(#[from] boring::error::ErrorStack),
    #[error("tls handshake: {0}")]
    Handshake(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed response: {0}")]
    Response(String),
    #[error("response body exceeds {0} bytes")]
    TooLarge(usize),
    /// The request never left intact, so nothing upstream can have acted on it: no address for the host, a
    /// refused connection, a rejected handshake, or a partially written request. Callers that must not
    /// repeat a side effect rely on this distinction.
    #[error("upstream unreachable: {0}")]
    Unreachable(String),
}
