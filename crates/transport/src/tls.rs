//! The two TLS profiles: the SDK inference plane and the Axios control plane of the pinned client.

use boring::ssl::{SslConnector, SslConnectorBuilder, SslMethod, SslVerifyMode, SslVersion};
use phoebis_profile::Profile;
use std::net::SocketAddr;
use tokio::net::TcpStream;

use crate::TransportError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plane {
    Inference,
    Control,
}

pub fn connector_builder(
    profile: &'static Profile,
    plane: Plane,
) -> Result<SslConnectorBuilder, TransportError> {
    let mut b = SslConnector::builder(SslMethod::tls())?;
    b.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    b.set_curves_list(profile.tls.curves)?;
    // The native client offers no GREASE and no permuted extensions on either plane.
    b.set_grease_enabled(false);
    b.set_permute_extensions(false);
    match plane {
        Plane::Inference => {
            b.set_alpn_protos(b"\x08http/1.1")?;
            b.enable_ocsp_stapling();
            b.enable_signed_cert_timestamps();
        }
        Plane::Control => {
            b.set_cipher_list(profile.tls.control_cipher_list)?;
        }
    }
    b.set_verify(SslVerifyMode::PEER);
    Ok(b)
}

pub fn connector(profile: &'static Profile, plane: Plane) -> Result<SslConnector, TransportError> {
    Ok(connector_builder(profile, plane)?.build())
}

pub fn trust_extra_ca(builder: &mut SslConnectorBuilder, pem: &str) -> Result<(), TransportError> {
    let cert = boring::x509::X509::from_pem(pem.as_bytes())?;
    builder.cert_store_mut().add_cert(cert)?;
    Ok(())
}

pub async fn connect_tls(
    connector: &SslConnector,
    host: &str,
    addr: SocketAddr,
) -> Result<tokio_boring::SslStream<TcpStream>, TransportError> {
    let started = std::time::Instant::now();
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let dialed = std::time::Instant::now();
    let cfg = connector.configure()?;
    let stream = tokio_boring::connect(cfg, host, tcp)
        .await
        .map_err(|e| TransportError::Handshake(e.to_string()))?;
    // The one line per new connection: the host and two durations, nothing of the request or the credential.
    tracing::debug!(
        host,
        dial_ms = dialed.duration_since(started).as_millis() as u64,
        handshake_ms = dialed.elapsed().as_millis() as u64,
        "a new upstream connection"
    );
    Ok(stream)
}
