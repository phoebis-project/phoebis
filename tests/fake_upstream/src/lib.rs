//! Loopback HTTP(S) upstream double: records every request verbatim and answers per configured rule, terminating
//! TLS for the `api.anthropic.com` / `platform.claude.com` SNI or speaking plain HTTP via `start_plain`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use boring::ssl::{SslAcceptor, SslMethod};
use bytes::{Buf, BytesMut};
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub struct Rule {
    pub path_prefix: String,
    pub respond: Respond,
}

#[derive(Clone)]
pub enum Respond {
    CloseAfterRequest,
    CloseImmediately,
    DropAfterHead,
    Json {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Sse {
        status: u16,
        headers: Vec<(String, String)>,
        events: Vec<SseEvent>,
        delay_between: Duration,
    },
    SseGated {
        status: u16,
        headers: Vec<(String, String)>,
        events: Vec<SseEvent>,
    },
}

#[derive(Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

#[derive(Clone)]
pub struct RecordedRequest {
    pub connection: u64,
    pub line: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub complete: bool,
}

pub struct FakeUpstream {
    addr: SocketAddr,
    ca_pem: String,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl FakeUpstream {
    pub async fn start(rules: Vec<Rule>) -> FakeUpstream {
        let tls = generate_tls();
        Self::spawn(rules, Wire::Tls(tls.acceptor), tls.ca_pem).await
    }

    /// Skips TLS for the foreign-host tests that never present the pinned SNI.
    pub async fn start_plain(rules: Vec<Rule>) -> FakeUpstream {
        Self::spawn(rules, Wire::Plain, String::new()).await
    }

    async fn spawn(rules: Vec<Rule>, wire: Wire, ca_pem: String) -> FakeUpstream {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
        let addr = listener.local_addr().expect("listener has a local address");
        let shared = Arc::new(Shared {
            rules: Mutex::new(rules.into_iter().map(RuleState::from).collect()),
            requests: Mutex::new(Vec::new()),
            connections: AtomicU64::new(0),
            gate: Notify::new(),
        });
        let wire = Arc::new(wire);
        let accept_shared = shared.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let conn_id = accept_shared.accept_connection();
                // Decided before any byte is read: the only information a `CloseImmediately` connection ever
                // gets to act on is its own ordinal among accepted connections.
                if accept_shared.should_close_immediately() {
                    drop(sock);
                    continue;
                }
                let conn_shared = accept_shared.clone();
                let conn_wire = wire.clone();
                tokio::spawn(async move {
                    handle_connection(conn_shared, sock, conn_id, conn_wire).await;
                });
            }
        });
        FakeUpstream {
            addr,
            ca_pem,
            shared,
            task,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared.requests.lock().unwrap().clone()
    }

    pub fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst) as usize
    }

    /// Lets every gated `SseGated` responder currently waiting continue past its first event.
    pub fn release(&self) {
        self.shared.gate.notify_one();
    }

    pub fn set_rule_after(&self, n_requests: usize, path_prefix: &str, respond: Respond) {
        self.shared.set_pending(
            path_prefix,
            SwitchKind::Requests,
            n_requests as u64,
            respond,
        );
    }

    pub fn set_rule_after_connections(&self, n: usize, path_prefix: &str, respond: Respond) {
        self.shared
            .set_pending(path_prefix, SwitchKind::Connections, n as u64, respond);
    }

    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

enum Wire {
    Tls(SslAcceptor),
    Plain,
}

struct Shared {
    rules: Mutex<Vec<RuleState>>,
    requests: Mutex<Vec<RecordedRequest>>,
    connections: AtomicU64,
    gate: Notify,
}

struct RuleState {
    path_prefix: String,
    respond: Respond,
    request_count: u64,
    pending: Option<PendingSwitch>,
}

impl From<Rule> for RuleState {
    fn from(r: Rule) -> Self {
        RuleState {
            path_prefix: r.path_prefix,
            respond: r.respond,
            request_count: 0,
            pending: None,
        }
    }
}

struct PendingSwitch {
    kind: SwitchKind,
    threshold: u64,
    new_respond: Respond,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SwitchKind {
    Connections,
    Requests,
}

impl Shared {
    fn record(&self, req: RecordedRequest) {
        self.requests.lock().unwrap().push(req);
    }

    /// The ordinal (0-based) among all accepted connections; also applies any due connection-count switch, since
    /// that decision has to be made before this connection's first request is read.
    fn accept_connection(&self) -> u64 {
        let ordinal = self.connections.fetch_add(1, Ordering::SeqCst);
        let mut rules = self.rules.lock().unwrap();
        for rule in rules.iter_mut() {
            let due = matches!(&rule.pending, Some(p) if p.kind == SwitchKind::Connections && ordinal >= p.threshold);
            if due {
                rule.respond = rule.pending.take().unwrap().new_respond;
            }
        }
        ordinal + 1
    }

    /// `CloseImmediately` cannot be scoped to a path prefix: the decision is made before any request byte
    /// arrives, so before the path is even known. Tests that use it configure exactly one active rule.
    fn should_close_immediately(&self) -> bool {
        self.rules
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r.respond, Respond::CloseImmediately))
    }

    fn resolve(&self, path: &str) -> Respond {
        let mut rules = self.rules.lock().unwrap();
        for rule in rules.iter_mut() {
            if !path.starts_with(rule.path_prefix.as_str()) {
                continue;
            }
            let due = matches!(&rule.pending, Some(p) if p.kind == SwitchKind::Requests && rule.request_count >= p.threshold);
            if due {
                rule.respond = rule.pending.take().unwrap().new_respond;
            }
            rule.request_count += 1;
            return rule.respond.clone();
        }
        Respond::Json {
            status: 404,
            headers: vec![],
            body: format!("fake-upstream: no rule matches {path}").into_bytes(),
        }
    }

    fn set_pending(
        &self,
        path_prefix: &str,
        kind: SwitchKind,
        threshold: u64,
        new_respond: Respond,
    ) {
        let mut rules = self.rules.lock().unwrap();
        let rule = rules
            .iter_mut()
            .find(|r| r.path_prefix == path_prefix)
            .unwrap_or_else(|| {
                panic!("fake-upstream: no rule configured for prefix {path_prefix:?}")
            });
        rule.pending = Some(PendingSwitch {
            kind,
            threshold,
            new_respond,
        });
    }
}

fn path_of(line: &str) -> &str {
    line.split(' ').nth(1).unwrap_or("")
}

fn wants_close(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close"))
}

async fn handle_connection(shared: Arc<Shared>, sock: TcpStream, conn_id: u64, wire: Arc<Wire>) {
    match wire.as_ref() {
        Wire::Tls(acceptor) => {
            if let Ok(tls) = tokio_boring::accept(acceptor, sock).await {
                serve(shared, tls, conn_id).await;
            }
        }
        Wire::Plain => serve(shared, sock, conn_id).await,
    }
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    shared: Arc<Shared>,
    mut stream: S,
    conn_id: u64,
) {
    let mut buf = BytesMut::new();
    loop {
        let recorded = match read_request(&mut stream, &mut buf, conn_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return, // the peer closed cleanly between requests
            Err(_) => return,   // a malformed or truncated head leaves nothing to answer
        };
        let path = path_of(&recorded.line).to_string();
        let close_after = wants_close(&recorded.headers);
        shared.record(recorded);
        match shared.resolve(&path) {
            Respond::CloseImmediately | Respond::CloseAfterRequest => return,
            Respond::DropAfterHead => {
                let _ = write_drop_head(&mut stream).await;
                return;
            }
            Respond::Json {
                status,
                headers,
                body,
            } => {
                if write_json(&mut stream, status, headers, body)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Respond::Sse {
                status,
                headers,
                events,
                delay_between,
            } => {
                let _ = write_sse(&mut stream, status, headers, events, delay_between).await;
                return;
            }
            Respond::SseGated {
                status,
                headers,
                events,
            } => {
                let _ = write_sse_gated(&shared, &mut stream, status, headers, events).await;
                return;
            }
        }
        if close_after {
            return;
        }
    }
}

const MAX_HEAD: usize = 64 * 1024;

/// Reimplemented rather than reusing `phoebis_transport::response`'s head-and-framing logic: the fake exists to check
/// the transport crate's behavior, so it must not share the code under test.
async fn read_request<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut BytesMut,
    connection: u64,
) -> std::io::Result<Option<RecordedRequest>> {
    let (line, headers, content_length, head_len) = loop {
        let mut raw_headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Request::new(&mut raw_headers);
        match parsed
            .parse(buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
        {
            httparse::Status::Complete(n) => {
                let method = parsed.method.unwrap_or_default();
                let path = parsed.path.unwrap_or_default();
                let version = parsed.version.unwrap_or(1);
                let line = format!("{method} {path} HTTP/1.{version}");
                let headers: Vec<(String, String)> = parsed
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_string(),
                            String::from_utf8_lossy(h.value).into_owned(),
                        )
                    })
                    .collect();
                let content_length = headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (line, headers, content_length, n);
            }
            httparse::Status::Partial => {
                if buf.len() > MAX_HEAD {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "request head too large",
                    ));
                }
                buf.reserve(4096);
                if stream.read_buf(buf).await? == 0 {
                    if buf.is_empty() {
                        return Ok(None);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "connection closed mid-head",
                    ));
                }
            }
        }
    };
    buf.advance(head_len);
    let mut body = Vec::with_capacity(content_length);
    let mut complete = true;
    while body.len() < content_length {
        if buf.is_empty() {
            buf.reserve(4096);
            if stream.read_buf(buf).await? == 0 {
                complete = false;
                break;
            }
        }
        let take = buf.len().min(content_length - body.len());
        body.extend_from_slice(&buf.split_to(take));
    }
    Ok(Some(RecordedRequest {
        connection,
        line,
        headers,
        body,
        complete,
    }))
}

fn status_line(status: u16) -> String {
    format!("HTTP/1.1 {status} status\r\n")
}

async fn write_json<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    mut headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> std::io::Result<()> {
    // A response that outlives one request (keep-alive reuse) needs Content-Length so the reader knows where the
    // body ends instead of waiting for a connection close that never comes.
    if !headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("content-length"))
    {
        headers.push(("Content-Length".to_string(), body.len().to_string()));
    }
    let mut out = status_line(status).into_bytes();
    for (n, v) in &headers {
        out.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(&body);
    stream.write_all(&out).await?;
    stream.flush().await
}

async fn write_drop_head<S: AsyncWrite + Unpin>(stream: &mut S) -> std::io::Result<()> {
    // Declares a body that never arrives, so a reader relying on Content-Length observes a truncation rather
    // than a clean zero-byte response.
    stream
        .write_all(b"HTTP/1.1 200 status\r\nContent-Length: 16\r\n\r\n")
        .await?;
    stream.flush().await
}

fn render_event(e: &SseEvent) -> String {
    format!("event: {}\ndata: {}\n\n", e.event, e.data)
}

async fn write_sse_head<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    mut headers: Vec<(String, String)>,
) -> std::io::Result<()> {
    if !headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("transfer-encoding"))
    {
        headers.push(("Transfer-Encoding".to_string(), "chunked".to_string()));
    }
    let mut out = status_line(status).into_bytes();
    for (n, v) in &headers {
        out.extend_from_slice(format!("{n}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    stream.write_all(&out).await?;
    stream.flush().await
}

async fn write_chunk<S: AsyncWrite + Unpin>(stream: &mut S, payload: &[u8]) -> std::io::Result<()> {
    let mut out = format!("{:x}\r\n", payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
    stream.write_all(&out).await?;
    stream.flush().await
}

async fn write_chunk_terminator<S: AsyncWrite + Unpin>(stream: &mut S) -> std::io::Result<()> {
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await
}

async fn write_sse<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    headers: Vec<(String, String)>,
    events: Vec<SseEvent>,
    delay_between: Duration,
) -> std::io::Result<()> {
    write_sse_head(stream, status, headers).await?;
    let mut first = true;
    for e in &events {
        if !first {
            tokio::time::sleep(delay_between).await;
        }
        first = false;
        write_chunk(stream, render_event(e).as_bytes()).await?;
    }
    write_chunk_terminator(stream).await
}

async fn write_sse_gated<S: AsyncWrite + Unpin>(
    shared: &Shared,
    stream: &mut S,
    status: u16,
    headers: Vec<(String, String)>,
    events: Vec<SseEvent>,
) -> std::io::Result<()> {
    write_sse_head(stream, status, headers).await?;
    let mut remaining = events.iter();
    if let Some(first) = remaining.next() {
        write_chunk(stream, render_event(first).as_bytes()).await?;
    }
    shared.gate.notified().await;
    for e in remaining {
        write_chunk(stream, render_event(e).as_bytes()).await?;
    }
    write_chunk_terminator(stream).await
}

struct GeneratedTls {
    ca_pem: String,
    acceptor: SslAcceptor,
}

fn generate_tls() -> GeneratedTls {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("empty-SAN CA params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let mut ca_name = DistinguishedName::new();
    // Must differ from the leaf's default distinguished name, or BoringSSL reads the leaf's issuer field as
    // equal to its own subject and rejects the chain as self-signed.
    ca_name.push(DnType::CommonName, "fake-upstream-ca");
    ca_params.distinguished_name = ca_name;
    let ca_key = KeyPair::generate().expect("generate CA key");
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign CA cert");

    let leaf_params = CertificateParams::new(vec![
        "api.anthropic.com".to_string(),
        "platform.claude.com".to_string(),
    ])
    .expect("leaf SAN params");
    let leaf_key = KeyPair::generate().expect("generate leaf key");
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .expect("sign leaf cert");

    let x509 = boring::x509::X509::from_der(leaf_cert.der()).expect("parse leaf certificate DER");
    let pkey = boring::pkey::PKey::private_key_from_der(&leaf_key.serialize_der())
        .expect("parse leaf key DER");

    let mut acceptor =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).expect("build TLS acceptor");
    acceptor
        .set_certificate(&x509)
        .expect("set leaf certificate");
    acceptor.set_private_key(&pkey).expect("set leaf key");
    acceptor
        .check_private_key()
        .expect("leaf key matches certificate");

    GeneratedTls {
        ca_pem: ca_cert.pem(),
        acceptor: acceptor.build(),
    }
}
