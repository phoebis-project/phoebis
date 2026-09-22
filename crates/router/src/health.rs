//! The health probe behind the image's `HEALTHCHECK`: one `HEAD /api/hello` on the proxy listener over
//! `std::net`, so the runtime image needs no shell and no HTTP client.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

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
pub fn probe(listen: SocketAddr, timeout: Duration) -> Result<SocketAddr, String> {
    let addr = target(listen);
    let deadline = Instant::now() + timeout;
    let remaining = |deadline: Instant| -> Result<Duration, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!(
                "proxy on {addr} sent no status line within {timeout:?}"
            ));
        }
        Ok(left)
    };
    let mut stream = TcpStream::connect_timeout(&addr, remaining(deadline)?)
        .map_err(|error| format!("proxy does not answer on {addr}: {error}"))?;
    stream
        .set_write_timeout(Some(remaining(deadline)?))
        .map_err(|error| format!("proxy probe on {addr} could not be timed: {error}"))?;
    let request = format!("HEAD /api/hello HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("proxy on {addr} took no request: {error}"))?;
    let mut head = [0u8; 128];
    let mut read = 0;
    while read < head.len() && !head[..read].contains(&b'\n') {
        stream
            .set_read_timeout(Some(remaining(deadline)?))
            .map_err(|error| format!("proxy probe on {addr} could not be timed: {error}"))?;
        match stream.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(error) => return Err(format!("proxy on {addr} sent no status line: {error}")),
        }
    }
    let line = String::from_utf8_lossy(&head[..read]);
    match line.split_whitespace().nth(1) {
        Some("200") => Ok(addr),
        Some(status) => Err(format!("proxy on {addr} answered {status}, not 200")),
        None => Err(format!("proxy on {addr} sent no status line")),
    }
}
