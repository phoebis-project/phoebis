//! Loopback HTTP(S) upstream double: records every request verbatim and answers per configured rule, terminating
//! TLS for the `api.anthropic.com` / `platform.claude.com` SNI or speaking plain HTTP via `start_plain`.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boring::ssl::{SslAcceptor, SslMethod};
use bytes::{Buf, BytesMut};
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

pub struct Rule {
    pub path_prefix: String,
    pub respond: Respond,
}

#[derive(Clone)]
pub enum Respond {
    /// A body in chunked framing, for the cases that need it without the SSE event shape.
    Chunked {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    CloseAfterRequest,
    CloseImmediately,
    DropAfterHead,
    /// Holds the request at the point where the response would start, until `release()`, and then answers
    /// with the wrapped rule. Lets a test act on a request the upstream has already read.
    Gated(Box<Respond>),
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
    /// Writes the head and then nothing until `release()`, which is what a stream that never commits looks like.
    Stall {
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

impl RecordedRequest {
    pub fn credential(&self) -> Option<&str> {
        credential_of(&self.headers)
    }
}

/// The bearer token or the `x-api-key` value, whichever the request carries.
fn credential_of(headers: &[(String, String)]) -> Option<&str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, v)| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .or_else(|| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("x-api-key"))
                .map(|(_, v)| v.as_str())
        })
        .map(str::trim)
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
            conns: Mutex::new(HashMap::new()),
            gate: Notify::new(),
            released: AtomicBool::new(false),
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

    /// Connections whose serve loop has not returned: a client that dropped its end shows up here once the
    /// fake's next write fails.
    pub fn open_connections(&self) -> usize {
        self.shared.conns.lock().unwrap().len()
    }

    /// Closes every connection that is sitting between requests, the way an upstream reaps a socket it
    /// considers idle. A connection in the middle of a request is left to finish it. The signal is
    /// asynchronous, so a caller that needs the client to observe the end of stream wants
    /// `close_idle_and_wait` instead.
    pub fn close_idle(&self) {
        self.reap_idle();
    }

    /// `close_idle` that returns only once the reaped sockets are actually shut, so the next thing the client
    /// reads from one of them is the end of stream rather than nothing at all.
    pub async fn close_idle_and_wait(&self) {
        let reaped = self.reap_idle();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self
            .shared
            .conns
            .lock()
            .unwrap()
            .keys()
            .any(|id| reaped.contains(id))
        {
            assert!(
                Instant::now() < deadline,
                "fake-upstream: a reaped connection never shut down",
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // The serve task has dropped its socket, so the FIN is on the wire; this settle keeps its loopback
        // delivery from racing whatever the client does next with its end.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    /// Signals every connection between requests to close and names the ones it signalled.
    fn reap_idle(&self) -> Vec<u64> {
        let mut reaped = Vec::new();
        for (id, handle) in self.shared.conns.lock().unwrap().iter() {
            if !handle.in_request.load(Ordering::SeqCst) {
                let _ = handle.close.send(true);
                reaped.push(*id);
            }
        }
        reaped
    }

    /// Lets every gated responder continue: a `Gated` one past the request it is holding, an `SseGated` one past
    /// its first event, including ones that gate after this call. The gate latches open: once released, it stays
    /// open for the lifetime of this `FakeUpstream`, so a second `release()` is never required.
    pub fn release(&self) {
        // Set before notifying: a responder woken by `notify_waiters()` re-checks this flag, and a responder
        // that gates afterward checks it before ever waiting, so neither ordering loses the release.
        self.shared.released.store(true, Ordering::SeqCst);
        self.shared.gate.notify_waiters();
    }

    /// Answers only the requests that carry `credential`, ahead of every prefix-only rule, so several accounts
    /// can be scripted independently against one listener.
    pub fn add_credential_rule(&self, credential: &str, path_prefix: &str, respond: Respond) {
        self.shared.rules.lock().unwrap().push(RuleState {
            path_prefix: path_prefix.into(),
            respond,
            request_count: 0,
            pending: None,
            credential: Some(credential.into()),
            queue: VecDeque::new(),
        });
    }

    /// One response per request, in order, ahead of the standing rule; the rule is created when it does not exist.
    pub fn push_credential_rule(&self, credential: &str, path_prefix: &str, respond: Respond) {
        let mut rules = self.shared.rules.lock().unwrap();
        match rules
            .iter_mut()
            .find(|r| r.path_prefix == path_prefix && r.credential.as_deref() == Some(credential))
        {
            Some(rule) => rule.queue.push_back(respond),
            None => rules.push(RuleState {
                path_prefix: path_prefix.into(),
                respond: Respond::CloseAfterRequest,
                request_count: 0,
                pending: None,
                credential: Some(credential.into()),
                queue: VecDeque::from([respond]),
            }),
        }
    }

    pub fn set_credential_rule_after(
        &self,
        credential: &str,
        n_requests: usize,
        path_prefix: &str,
        respond: Respond,
    ) {
        let mut rules = self.shared.rules.lock().unwrap();
        let rule = rules
            .iter_mut()
            .find(|r| r.path_prefix == path_prefix && r.credential.as_deref() == Some(credential))
            .unwrap_or_else(|| panic!("fake-upstream: no credential rule at {path_prefix:?}"));
        rule.pending = Some(PendingSwitch {
            kind: SwitchKind::Requests,
            threshold: n_requests as u64,
            new_respond: respond,
        });
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
    conns: Mutex<HashMap<u64, ConnHandle>>,
    gate: Notify,
    released: AtomicBool,
}

/// What `close_idle` needs about a live connection: whether it is busy, and a way to wake its serve loop.
struct ConnHandle {
    close: watch::Sender<bool>,
    in_request: Arc<AtomicBool>,
}

struct RuleState {
    path_prefix: String,
    respond: Respond,
    request_count: u64,
    pending: Option<PendingSwitch>,
    /// `None` matches on the path prefix alone; `Some` matches only the requests carrying that credential.
    credential: Option<String>,
    /// Scripted responses served one per request before the standing `respond`.
    queue: VecDeque<Respond>,
}

impl From<Rule> for RuleState {
    fn from(r: Rule) -> Self {
        RuleState {
            path_prefix: r.path_prefix,
            respond: r.respond,
            request_count: 0,
            pending: None,
            credential: None,
            queue: VecDeque::new(),
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

    fn register_connection(&self, conn_id: u64) -> (watch::Receiver<bool>, Arc<AtomicBool>) {
        let (close, closed) = watch::channel(false);
        let in_request = Arc::new(AtomicBool::new(false));
        self.conns.lock().unwrap().insert(
            conn_id,
            ConnHandle {
                close,
                in_request: in_request.clone(),
            },
        );
        (closed, in_request)
    }

    fn forget_connection(&self, conn_id: u64) {
        self.conns.lock().unwrap().remove(&conn_id);
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
    /// arrives, so before the path is even known. The credential is unknown just as early, so a
    /// `CloseImmediately` on a credential rule closes every connection, not that account's. Tests that use it
    /// configure exactly one active rule.
    fn should_close_immediately(&self) -> bool {
        self.rules
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r.respond, Respond::CloseImmediately))
    }

    /// Credential-scoped rules are consulted before the prefix-only ones, so one fake can answer several
    /// accounts on the same path without their rules shadowing each other.
    fn resolve(&self, path: &str, headers: &[(String, String)]) -> Respond {
        let credential = credential_of(headers);
        let mut rules = self.rules.lock().unwrap();
        for scoped in [true, false] {
            for rule in rules.iter_mut() {
                let admitted = match &rule.credential {
                    // A credential rule answers that account and nothing else: a request carrying another
                    // credential, or none, must fall through to the prefix-only rules.
                    Some(c) => scoped && Some(c.as_str()) == credential,
                    None => !scoped,
                };
                if !admitted {
                    continue;
                }
                if !path.starts_with(rule.path_prefix.as_str()) {
                    continue;
                }
                return take_response(rule);
            }
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

    /// Blocks until `release()` has latched the gate open. The `Notified` future is created before the flag
    /// check on every loop iteration, so a `release()` landing between the check and the `.await` is still
    /// observed (`Notify` captures that ordering); the flag itself covers a `release()` that already happened
    /// before this call started, which `notify_waiters()` alone would miss since it only wakes listeners that
    /// existed at the time it was called.
    async fn wait_for_release(&self) {
        loop {
            let notified = self.gate.notified();
            if self.released.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// Applies whatever request-count switch is due, then serves the queue's front if the rule has one scripted.
fn take_response(rule: &mut RuleState) -> Respond {
    let due = matches!(&rule.pending, Some(p) if p.kind == SwitchKind::Requests && rule.request_count >= p.threshold);
    if due {
        rule.respond = rule.pending.take().unwrap().new_respond;
    }
    rule.request_count += 1;
    rule.queue
        .pop_front()
        .unwrap_or_else(|| rule.respond.clone())
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

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(shared: Arc<Shared>, stream: S, conn_id: u64) {
    let (closed, in_request) = shared.register_connection(conn_id);
    serve_requests(&shared, stream, conn_id, closed, &in_request).await;
    shared.forget_connection(conn_id);
}

async fn serve_requests<S: AsyncRead + AsyncWrite + Unpin>(
    shared: &Arc<Shared>,
    mut stream: S,
    conn_id: u64,
    mut closed: watch::Receiver<bool>,
    in_request: &AtomicBool,
) {
    let mut buf = BytesMut::new();
    loop {
        // Biased so a connection asked to close never answers one more request first, which is the whole
        // point of reaping it: the client must find the socket gone when it writes into it.
        let read = tokio::select! {
            biased;
            _ = closed.changed() => return,
            read = read_request(&mut stream, &mut buf, conn_id) => read,
        };
        let recorded = match read {
            Ok(Some(r)) => r,
            Ok(None) => return, // the peer closed cleanly between requests
            Err(_) => return,   // a malformed or truncated head leaves nothing to answer
        };
        in_request.store(true, Ordering::SeqCst);
        let path = path_of(&recorded.line).to_string();
        let close_after = wants_close(&recorded.headers);
        // Read before the record takes ownership: the credential decides which rule answers.
        let headers = recorded.headers.clone();
        shared.record(recorded);
        // Unwrapped before the match rather than inside it, so the gate costs the responders nothing and no
        // arm has to be written twice.
        let mut respond = shared.resolve(&path, &headers);
        while let Respond::Gated(inner) = respond {
            shared.wait_for_release().await;
            respond = *inner;
        }
        match respond {
            Respond::Chunked {
                status,
                headers,
                body,
            } => {
                if write_chunked(&mut stream, status, headers, body)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Respond::CloseImmediately | Respond::CloseAfterRequest => return,
            Respond::Gated(_) => unreachable!("the loop above unwraps every gate"),
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
                let _ = write_sse_gated(shared, &mut stream, status, headers, events).await;
                return;
            }
            Respond::Stall {
                status,
                headers,
                events,
            } => {
                if write_sse_head(&mut stream, status, event_stream_headers(headers))
                    .await
                    .is_err()
                {
                    return;
                }
                shared.wait_for_release().await;
                for e in &events {
                    if write_chunk(&mut stream, render_event(e).as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = write_chunk_terminator(&mut stream).await;
                return;
            }
        }
        in_request.store(false, Ordering::SeqCst);
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

/// The head writer is shared with the SSE responders: all it adds is the chunked framing they have in common.
async fn write_chunked<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> std::io::Result<()> {
    write_sse_head(stream, status, headers).await?;
    write_chunk(stream, &body).await?;
    write_chunk_terminator(stream).await
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

/// The SSE responders label the stream the way the real upstream does when the test did not say otherwise.
/// `write_chunked` deliberately does not, so a chunked JSON rejection keeps the caller's own content type.
fn event_stream_headers(mut headers: Vec<(String, String)>) -> Vec<(String, String)> {
    if !headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("Content-Type".to_string(), "text/event-stream".to_string()));
    }
    headers
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
    write_sse_head(stream, status, event_stream_headers(headers)).await?;
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
    write_sse_head(stream, status, event_stream_headers(headers)).await?;
    let mut remaining = events.iter();
    if let Some(first) = remaining.next() {
        write_chunk(stream, render_event(first).as_bytes()).await?;
    }
    shared.wait_for_release().await;
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
