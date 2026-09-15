//! TLS profiles, ordered HTTP/1.1 writer, response reader, keep-alive pool.

pub mod control;
pub mod decode;
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
}
