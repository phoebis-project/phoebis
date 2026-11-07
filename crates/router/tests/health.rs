//! `phoebis health` and the `PHOEBIS_CONFIG` fallback, against listeners this file owns on loopback ports, plain and
//! behind TLS.

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use phoebis_router::health::{Clock, SystemClock};

fn config_file(dir: &Path, name: &str, proxy: SocketAddr) -> PathBuf {
    let key = dir.join("storage.key");
    std::fs::write(
        &key,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .unwrap();
    let yaml = format!(
        "admin:\n  listen: 127.0.0.1:0\n  secret_env: TEST_HEALTH_ADMIN\nproxy:\n  listen: {proxy}\nstorage:\n  encryption_key_file: {}\n  path: {}\nusage:\n  refresh_concurrency: 2\n",
        key.display(),
        dir.join("router.sqlite").display()
    );
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(&path, yaml).unwrap();
    path
}

fn phoebis() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_phoebis"));
    command
        .env("TEST_HEALTH_ADMIN", "admin-secret-001")
        .env_remove("PHOEBIS_CONFIG");
    command
}

/// One connection, one answer: the status line the test chooses, after the request head is read in full.
fn answering(status: &'static str) -> (SocketAddr, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            request.push(byte[0]);
        }
        stream
            .write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        String::from_utf8(request).unwrap()
    });
    (addr, served)
}

/// `answering`'s twin behind TLS under a pair the test generates: the request head once the handshake finished, or
/// `None` when the handshake failed.
fn answering_tls(status: &'static str) -> (SocketAddr, std::thread::JoinHandle<Option<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = fake_upstream::acceptor_for(&fake_upstream::generate_pair(&["localhost"]));
    let served = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = tokio_boring::accept(&acceptor, stream).await.ok()?;
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).await.unwrap() == 1
                {
                    request.push(byte[0]);
                }
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
                Some(String::from_utf8(request).unwrap())
            })
    });
    (addr, served)
}

#[test]
fn a_listener_answering_200_passes_and_is_asked_for_ready_by_head() {
    let (addr, served) = answering("200 OK");
    assert_eq!(
        phoebis_router::health::probe(addr, false, phoebis_router::health::TIMEOUT, &SystemClock),
        Ok(addr)
    );
    let request = served.join().unwrap();
    assert!(
        request.starts_with("HEAD /api/ready HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request.contains(&format!("Host: {addr}\r\n")) && request.contains("Connection: close\r\n")
    );
}

#[test]
fn a_closed_port_fails_within_the_timeout() {
    let addr = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let started = Instant::now();
    let reason =
        phoebis_router::health::probe(addr, false, phoebis_router::health::TIMEOUT, &SystemClock)
            .unwrap_err();
    assert!(
        reason.starts_with(&format!(
            "proxy does not answer on {addr}: Connection refused"
        )),
        "{reason}"
    );
    assert!(started.elapsed() < phoebis_router::health::TIMEOUT + Duration::from_secs(1));
}

/// The budget the four stalled-listener tests hand to `probe`, plain and TLS: short, so the suite stays fast, while
/// the subcommand's constant stays at its three seconds.
const SHORT: Duration = Duration::from_secs(1);

#[test]
fn a_listener_that_accepts_and_never_writes_fails_within_the_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let held = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        // Held open until the probe has given up, so the read sees silence rather than a closed socket.
        std::thread::sleep(SHORT + Duration::from_secs(1));
        drop(stream);
    });
    let started = Instant::now();
    let reason = phoebis_router::health::probe(addr, false, SHORT, &SystemClock).unwrap_err();
    assert!(
        started.elapsed() < SHORT + Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    assert!(reason.contains("no status line"), "{reason}");
    held.join().unwrap();
}

/// The spent-budget case's clock, keyed on the probe's own reads rather than on the listener's accept, which the
/// kernel's connect races: the probe reads the clock twice before the connect (the deadline, then the remainder the
/// connect gets), so those two reads answer the start and every later read answers the budget spent.
struct SpentAfterConnect {
    start: Instant,
    budget: Duration,
    reads: Cell<u32>,
}

impl Clock for SpentAfterConnect {
    fn now(&self) -> Instant {
        let read = self.reads.get();
        self.reads.set(read + 1);
        if read < 2 {
            self.start
        } else {
            self.start + self.budget
        }
    }
}

#[test]
fn a_listener_that_accepts_and_whose_budget_is_spent_before_the_first_read_fails_naming_the_deadline(
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let budget = phoebis_router::health::TIMEOUT;
    let clock = SpentAfterConnect {
        start: Instant::now(),
        budget,
        reads: Cell::new(0),
    };
    let reason = phoebis_router::health::probe(addr, true, budget, &clock).unwrap_err();
    assert_eq!(
        reason,
        format!("proxy on {addr} sent no status line within {budget:?}")
    );
    // The probe has returned, so a connection it made waits in the backlog though the probe has closed it.
    assert!(
        listener.accept().is_ok(),
        "the probe connected before its budget ran out"
    );
}

#[test]
fn a_listener_that_trickles_the_status_line_fails_within_the_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let trickled = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // One byte every 400 ms: each read returns inside a per-read timeout, so only an absolute deadline
        // stops the probe before the line completes.
        for byte in b"HTTP/1.1 200 OK\r\n" {
            if stream.write_all(&[*byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    });
    let started = Instant::now();
    let reason = phoebis_router::health::probe(addr, false, SHORT, &SystemClock).unwrap_err();
    assert!(
        started.elapsed() < SHORT + Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    assert!(reason.contains("no status line"), "{reason}");
    trickled.join().unwrap();
}

#[test]
fn a_tls_listener_that_accepts_and_never_answers_the_handshake_fails_within_the_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let held = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        // Held open until the probe has given up, so the handshake sees silence rather than a closed socket.
        std::thread::sleep(SHORT + Duration::from_secs(1));
        drop(stream);
    });
    let started = Instant::now();
    let reason = phoebis_router::health::probe(addr, true, SHORT, &SystemClock).unwrap_err();
    assert!(
        started.elapsed() < SHORT + Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    // A listener that never answers ran out the budget; it refused nothing.
    assert!(
        reason.contains("no status line") && !reason.contains("refused"),
        "{reason}"
    );
    held.join().unwrap();
}

#[test]
fn a_tls_listener_that_trickles_its_handshake_fails_within_the_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let trickled = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // A handshake record's header announcing sixteen bytes, then the sixteen, one byte every 400 ms: BoringSSL
        // reads a record to its announced length one socket read at a time, so only a deadline on every read stops
        // the handshake before the record completes.
        let mut record = vec![0x16, 0x03, 0x03, 0x00, 0x10];
        record.extend([0u8; 16]);
        for byte in record {
            if stream.write_all(&[byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    });
    let started = Instant::now();
    let reason = phoebis_router::health::probe(addr, true, SHORT, &SystemClock).unwrap_err();
    assert!(
        started.elapsed() < SHORT + Duration::from_millis(500),
        "{:?}: {reason}",
        started.elapsed()
    );
    assert!(
        reason.contains("no status line") && !reason.contains("refused"),
        "{reason}"
    );
    trickled.join().unwrap();
}

#[test]
fn an_unspecified_listen_is_probed_on_the_loopback_of_its_family() {
    use phoebis_router::health::target;
    assert_eq!(
        target("0.0.0.0:8317".parse().unwrap()),
        "127.0.0.1:8317".parse().unwrap()
    );
    assert_eq!(
        target("[::]:8317".parse().unwrap()),
        "[::1]:8317".parse().unwrap()
    );
    assert_eq!(
        target("10.0.0.2:8317".parse().unwrap()),
        "10.0.0.2:8317".parse().unwrap()
    );
    let (addr, served) = answering("200 OK");
    let unspecified: SocketAddr = format!("0.0.0.0:{}", addr.port()).parse().unwrap();
    assert_eq!(
        phoebis_router::health::probe(
            unspecified,
            false,
            phoebis_router::health::TIMEOUT,
            &SystemClock
        ),
        Ok(addr)
    );
    served.join().unwrap();
}

#[test]
fn the_subcommand_exits_zero_on_200_and_one_on_a_closed_port() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, served) = answering("200 OK");
    let up = config_file(dir.path(), "up", addr);
    let out = phoebis()
        .args(["health", "--config"])
        .arg(&up)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("proxy answers on {addr}")
    );
    served.join().unwrap();
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let down = config_file(dir.path(), "down", closed);
    let out = phoebis()
        .args(["health", "--config"])
        .arg(&down)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);
}

/// A listener that answers is not a healthy one, so the container's probe fails on any other status and says
/// which it saw; the orchestrator reads that line out of the last health log.
#[test]
fn the_subcommand_exits_one_on_a_listener_answering_503() {
    let dir = tempfile::tempdir().unwrap();
    let (addr, served) = answering("503 Service Unavailable");
    let config = config_file(dir.path(), "unhealthy", addr);
    let out = phoebis()
        .args(["health", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("proxy on {addr} answered 503, not 200")
    );
    served.join().unwrap();
}

/// The probe sends no credential, so a container whose secret the service would refuse still reads as up or down by
/// its listener alone.
#[test]
fn the_subcommand_probes_under_a_placeholder_or_unset_admin_secret() {
    let dir = tempfile::tempdir().unwrap();
    for (name, secret) in [
        ("placeholder", Some("replace-with-a-long-random-string")),
        ("unset", None),
    ] {
        let (addr, served) = answering("200 OK");
        let config = config_file(dir.path(), name, addr);
        let mut command = phoebis();
        match secret {
            Some(value) => command.env("TEST_HEALTH_ADMIN", value),
            None => command.env_remove("TEST_HEALTH_ADMIN"),
        };
        let out = command
            .args(["health", "--config"])
            .arg(&config)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("proxy answers on {addr}")
        );
        served.join().unwrap();
    }
}

#[test]
fn phoebis_config_names_the_file_when_the_flag_is_absent_and_loses_to_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let proxy: SocketAddr = "127.0.0.1:8317".parse().unwrap();
    let from_env = config_file(dir.path(), "env", proxy);
    let from_flag = config_file(dir.path(), "flag", proxy);
    let out = phoebis()
        .arg("check-config")
        .env("PHOEBIS_CONFIG", &from_env)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout)
        .starts_with(&format!("{} is valid", from_env.display())));
    let out = phoebis()
        .args(["check-config", "--config"])
        .arg(&from_flag)
        .env("PHOEBIS_CONFIG", dir.path().join("missing.yaml"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout)
        .starts_with(&format!("{} is valid", from_flag.display())));
    let out = phoebis()
        .arg("check-config")
        .env("PHOEBIS_CONFIG", dir.path().join("missing.yaml"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

/// One connection, one answer written verbatim: the bytes a real listener would never send, so the probe's
/// own reading of the status line is what the test drives.
fn answering_raw(bytes: &'static [u8]) -> (SocketAddr, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
            request.push(byte[0]);
        }
        stream.write_all(bytes).unwrap();
    });
    (addr, served)
}

/// `phoebis health` against a listener answering `bytes`: its exit code and what it printed.
fn health_against(name: &str, bytes: &'static [u8]) -> (Option<i32>, String) {
    let dir = tempfile::tempdir().unwrap();
    let (addr, served) = answering_raw(bytes);
    let config = config_file(dir.path(), name, addr);
    let out = phoebis()
        .args(["health", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    served.join().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

#[test]
fn a_first_line_that_names_no_http_version_is_no_status_line() {
    let (code, reason) = health_against("not-http", b"NOT_HTTP 200 OK\r\n\r\n");
    assert_eq!(code, Some(1));
    assert!(reason.ends_with("sent no status line"), "{reason}");
}

#[test]
fn a_version_and_a_status_on_two_lines_are_no_status_line() {
    let (code, reason) = health_against("split", b"HTTP/1.1\r\n200 OK\r\n\r\n");
    assert_eq!(code, Some(1));
    assert!(reason.ends_with("sent no status line"), "{reason}");
}

#[test]
fn a_status_line_an_eof_cut_short_is_no_status_line() {
    let (code, reason) = health_against("eof", b"HTTP/1.1 200");
    assert_eq!(code, Some(1));
    assert!(reason.ends_with("sent no status line"), "{reason}");
}

#[test]
fn the_probe_speaks_tls_to_a_tls_listener() {
    let (addr, served) = answering_tls("200 OK");
    assert_eq!(
        phoebis_router::health::probe(addr, true, phoebis_router::health::TIMEOUT, &SystemClock),
        Ok(addr)
    );
    let request = served.join().unwrap().expect("the handshake finished");
    assert!(
        request.starts_with("HEAD /api/ready HTTP/1.1\r\n"),
        "{request}"
    );
}

#[test]
fn a_plain_probe_against_a_tls_listener_reads_no_status_line() {
    let (addr, served) = answering_tls("200 OK");
    let reason =
        phoebis_router::health::probe(addr, false, phoebis_router::health::TIMEOUT, &SystemClock)
            .unwrap_err();
    assert!(
        reason.contains("sent no status line") || reason.contains("took no request"),
        "{reason}"
    );
    assert_eq!(served.join().unwrap(), None);
}

#[test]
fn a_tls_probe_against_a_plain_listener_fails_the_handshake() {
    // A plain listener that answers whatever it first reads with a 200, so only the handshake can fail the probe.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let served = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut hello = [0u8; 512];
        let _ = stream.read(&mut hello).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    let reason =
        phoebis_router::health::probe(addr, true, phoebis_router::health::TIMEOUT, &SystemClock)
            .unwrap_err();
    assert!(reason.contains("refused the TLS handshake"), "{reason}");
    served.join().unwrap();
}
