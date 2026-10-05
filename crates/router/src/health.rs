//! The health probe behind the image's `HEALTHCHECK`: one `HEAD /api/ready` on the proxy listener over
//! `std::net`, so the runtime image needs no shell and no HTTP client.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use boring::error::ErrorStack;
use boring::ssl::{SslConnector, SslMethod, SslStream, SslVerifyMode};

/// The whole probe's budget: connect, write, and the status line together, so a wedged listener fails the
/// check instead of stalling it.
pub const TIMEOUT: Duration = Duration::from_secs(3);

/// Where a listener bound on every interface is reached: the loopback of the same family, since an
/// unspecified address is a bind target and not a destination.
pub fn target(listen: SocketAddr) -> SocketAddr {
    match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => (Ipv4Addr::LOCALHOST, listen.port()).into(),
        IpAddr::V6(ip) if ip.is_unspecified() => (Ipv6Addr::LOCALHOST, listen.port()).into(),
        _ => listen,
    }
}

/// Answers the address that said `200`, or one line saying why the listener did not. The budget is one
/// absolute deadline: every blocking step checks it first and gets the remainder as its socket timeout, so a
/// listener that trickles a byte per read cannot stretch the probe past `timeout`.
///
/// `tls` says the proxy listener terminates TLS: the probe then wraps the socket in a connector that verifies
/// nothing, since it dials its own process on loopback and asks whether `/api/ready` answers 200, not who answers.
pub fn probe(listen: SocketAddr, tls: bool, timeout: Duration) -> Result<SocketAddr, String> {
    let addr = target(listen);
    let deadline = Instant::now() + timeout;
    let stream = TcpStream::connect_timeout(&addr, remaining(addr, deadline, timeout)?)
        .map_err(|error| format!("proxy does not answer on {addr}: {error}"))?;
    let timed =
        |error: std::io::Error| format!("proxy probe on {addr} could not be timed: {error}");
    // The clone shares the descriptor, so a timeout set on it governs the reads the TLS stream makes, and the TLS
    // arm never borrows its own stream twice.
    let socket = stream.try_clone().map_err(timed)?;
    let mut probe = if tls {
        let connector_error = |error: ErrorStack| {
            format!("proxy probe on {addr} could not build its TLS connector: {error}")
        };
        // Verification off on purpose: the probe dials its own process on loopback and asks whether `/api/ready`
        // answers 200, not who answers; the listener's certificate is `check-config`'s to judge.
        let mut builder = SslConnector::builder(SslMethod::tls()).map_err(connector_error)?;
        builder.set_verify(SslVerifyMode::NONE);
        builder
            .set_alpn_protos(b"\x08http/1.1")
            .map_err(connector_error)?;
        let mut config = builder.build().configure().map_err(connector_error)?;
        config.set_verify_hostname(false);
        stream
            .set_write_timeout(Some(remaining(addr, deadline, timeout)?))
            .map_err(timed)?;
        stream
            .set_read_timeout(Some(remaining(addr, deadline, timeout)?))
            .map_err(timed)?;
        Probe::Tls(
            config
                .connect("localhost", stream)
                .map_err(|error| format!("proxy on {addr} refused the TLS handshake: {error}"))?,
        )
    } else {
        Probe::Plain(stream)
    };
    exchange(&mut probe, &socket, addr, deadline, timeout).map(|()| addr)
}

/// The probe's connection: the socket itself, or a TLS stream over it.
enum Probe {
    Plain(TcpStream),
    Tls(SslStream<TcpStream>),
}

impl Read for Probe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Probe::Plain(stream) => stream.read(buf),
            Probe::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for Probe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Probe::Plain(stream) => stream.write(buf),
            Probe::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Probe::Plain(stream) => stream.flush(),
            Probe::Tls(stream) => stream.flush(),
        }
    }
}

/// What is left of the budget, or the line saying it ran out.
fn remaining(addr: SocketAddr, deadline: Instant, timeout: Duration) -> Result<Duration, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(format!(
            "proxy on {addr} sent no status line within {timeout:?}"
        ));
    }
    Ok(left)
}

/// `HEAD /api/ready` through `stream` and the status line it answers, with each step's timeout set on `socket`, the
/// descriptor `stream` reads and writes through.
fn exchange<S: Read + Write>(
    stream: &mut S,
    socket: &TcpStream,
    addr: SocketAddr,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), String> {
    socket
        .set_write_timeout(Some(remaining(addr, deadline, timeout)?))
        .map_err(|error| format!("proxy probe on {addr} could not be timed: {error}"))?;
    let request = format!("HEAD /api/ready HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("proxy on {addr} took no request: {error}"))?;
    let mut head = [0u8; 128];
    let mut read = 0;
    while read < head.len() && !head[..read].contains(&b'\n') {
        socket
            .set_read_timeout(Some(remaining(addr, deadline, timeout)?))
            .map_err(|error| format!("proxy probe on {addr} could not be timed: {error}"))?;
        match stream.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(error) => return Err(format!("proxy on {addr} sent no status line: {error}")),
        }
    }
    // The status line ends at the first newline and nowhere else: a buffer that filled or a peer that closed
    // before one arrived carries no status line, whatever tokens it happens to hold, and reading the status
    // out of the whole buffer would take the second word of a greeting, or of the line after the first.
    let Some(end) = head[..read].iter().position(|b| *b == b'\n') else {
        return Err(format!("proxy on {addr} sent no status line"));
    };
    let line = String::from_utf8_lossy(&head[..end]);
    let line = line.trim_end_matches('\r');
    let mut fields = line.split(' ');
    let version = fields.next().unwrap_or_default();
    let status = fields.next().unwrap_or_default();
    if !version.starts_with("HTTP/")
        || status.len() != 3
        || !status.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!("proxy on {addr} sent no status line"));
    }
    if status != "200" {
        return Err(format!("proxy on {addr} answered {status}, not 200"));
    }
    Ok(())
}
