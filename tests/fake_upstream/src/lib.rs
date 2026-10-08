//! Loopback HTTP(S) upstream double: records every request verbatim and answers per configured rule, terminating
//! TLS for the `api.anthropic.com` / `platform.claude.com` SNI, for the SANs `start_tls_for` is given, or
//! speaking plain HTTP via `start_plain`.

mod capture;

pub use capture::capture_tracing;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boring::pkey::PKey;
use boring::ssl::{
    select_next_proto, AlpnError, ConnectConfiguration, SslAcceptor, SslConnector, SslMethod,
};
use boring::x509::X509;
use bytes::{Buf, BytesMut};
use rcgen::{
    date_time_ymd, BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch, Notify};
use tokio::task::JoinHandle;

/// Waits for something a background task of the router or the fake produces. The deadline is generous because
/// it is only ever reached when the thing never happens, and the poll is short so a passing test does not pay
/// for it.
pub async fn wait_for<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let give_up = Instant::now() + deadline;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            Instant::now() < give_up,
            "the expected state never arrived within {deadline:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The dual of `wait_for`, for a state that must not move. The window bounds how long a background write has
/// to land; it is never proof that none is still coming.
pub async fn stays_for(window: Duration, mut probe: impl FnMut() -> bool) {
    let until = Instant::now() + window;
    while Instant::now() < until {
        assert!(probe(), "the state moved within {window:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The async-probe twin of `wait_for`, for a check that itself needs to await (a ledger read on a blocking
/// worker, for instance, which a plain `FnMut` probe cannot call). Same deadline, same 5 ms poll, same message.
pub async fn wait_for_async<T, F, Fut>(deadline: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let give_up = Instant::now() + deadline;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            Instant::now() < give_up,
            "the expected state never arrived within {deadline:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The async-probe twin of `stays_for`.
pub async fn stays_for_async<F, Fut>(window: Duration, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let until = Instant::now() + window;
    while Instant::now() < until {
        assert!(probe().await, "the state moved within {window:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

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
    /// Writes the head and the events, then closes the connection without the chunked body's terminator: a stream
    /// cut mid-body, whatever its status.
    SseCut {
        status: u16,
        headers: Vec<(String, String)>,
        events: Vec<SseEvent>,
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
        Self::start_tls_for(rules, &["api.anthropic.com", "platform.claude.com"]).await
    }

    /// A leaf carrying the caller's own SANs, so a request to a foreign hostname passes verification.
    pub async fn start_tls_for(rules: Vec<Rule>, hosts: &[&str]) -> FakeUpstream {
        let tls = generate_tls(hosts);
        Self::spawn(rules, Wire::Tls(tls.acceptor), tls.ca_pem).await
    }

    /// Skips TLS for the foreign-host tests that never present the pinned SNI.
    pub async fn start_plain(rules: Vec<Rule>) -> FakeUpstream {
        Self::spawn(rules, Wire::Plain, String::new()).await
    }

    /// A listener the relay dials as a TLS host (its CA trusted as `start`'s is) that accepts and never answers the
    /// ClientHello, reading nothing, so the handshake is what the connect bound times out.
    pub async fn start_silent() -> FakeUpstream {
        let tls = generate_tls(&["api.anthropic.com", "platform.claude.com"]);
        Self::spawn(Vec::new(), Wire::Silent, tls.ca_pem).await
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
            held: Mutex::new(VecDeque::new()),
            released: AtomicBool::new(false),
            written: Mutex::new(HashMap::new()),
        });
        let wire = Arc::new(wire);
        let accept_shared = shared.clone();
        let task = tokio::spawn(async move {
            // Held by this task, so a silent listener's sockets stay open until `shutdown` aborts it.
            let mut silent = Vec::new();
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let conn_id = accept_shared.accept_connection();
                if matches!(*wire, Wire::Silent) {
                    silent.push(sock);
                    continue;
                }
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

    /// Lets the oldest responder still held at its gate continue and leaves the rest held, so a test can end one
    /// stream of several. Waits for a responder to reach its gate first: the client can hold a committed head
    /// a moment before the responder that wrote it has parked.
    pub async fn release_one(&self) {
        let give_up = Instant::now() + Duration::from_secs(5);
        loop {
            let next = self.shared.held.lock().unwrap().pop_front();
            match next {
                Some(open) => {
                    // A responder the latch already let go, or one whose connection ended, dropped its end.
                    if open.send(()).is_ok() {
                        return;
                    }
                }
                None => {
                    assert!(
                        Instant::now() < give_up,
                        "fake-upstream: no responder was held to release"
                    );
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
    }

    /// Resolves once the `n`th scripted response (zero-based, in arrival order) has been written out whole, so
    /// a test can assert on what a slow subscriber did after the stream ended rather than sleep past it.
    pub async fn written(&self, n: usize) {
        let mut done = self.shared.written_signal(n).subscribe();
        // The sender lives in `Shared` for the fake's lifetime, so the wait ends only on the write.
        let _ = done.wait_for(|whole| *whole).await;
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
            header: None,
            queue: VecDeque::new(),
        });
    }

    /// Answers only the requests that carry `header: value`, ahead of every prefix-only rule: for a gateway
    /// whose token rides in a name of its own rather than the credential slot `add_credential_rule` matches.
    pub fn add_header_rule(&self, header: &str, value: &str, path_prefix: &str, respond: Respond) {
        self.shared.rules.lock().unwrap().push(RuleState {
            path_prefix: path_prefix.into(),
            respond,
            request_count: 0,
            pending: None,
            credential: None,
            header: Some((header.to_ascii_lowercase(), value.into())),
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
                header: None,
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
    /// Accepts and reads nothing.
    Silent,
}

struct Shared {
    rules: Mutex<Vec<RuleState>>,
    requests: Mutex<Vec<RecordedRequest>>,
    connections: AtomicU64,
    conns: Mutex<HashMap<u64, ConnHandle>>,
    gate: Notify,
    /// One opener per responder parked at its gate, oldest first, for `release_one`.
    held: Mutex<VecDeque<oneshot::Sender<()>>>,
    released: AtomicBool,
    /// One latch per recorded request, by its index in `requests`, made by whichever of the responder and a
    /// `written` waiter comes first, so neither a waiter that comes late nor a second one can miss the write.
    written: Mutex<HashMap<usize, watch::Sender<bool>>>,
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
    /// `Some((name, value))` matches only requests carrying that header (case-insensitive name, exact value).
    header: Option<(String, String)>,
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
            header: None,
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
    /// The request's index in arrival order, which `written` names its response by.
    fn record(&self, req: RecordedRequest) -> usize {
        let mut requests = self.requests.lock().unwrap();
        requests.push(req);
        requests.len() - 1
    }

    fn written_signal(&self, n: usize) -> watch::Sender<bool> {
        self.written
            .lock()
            .unwrap()
            .entry(n)
            .or_insert_with(|| watch::channel(false).0)
            .clone()
    }

    fn mark_written(&self, n: usize) {
        self.written_signal(n).send_replace(true);
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

    /// Credential- and header-scoped rules are consulted before the prefix-only ones, so one fake can answer
    /// several accounts, or a gateway's own header check, on the same path without shadowing each other.
    fn resolve(&self, path: &str, headers: &[(String, String)]) -> Respond {
        let credential = credential_of(headers);
        let has_header = |name: &str, value: &str| {
            headers
                .iter()
                .any(|(n, v)| n.eq_ignore_ascii_case(name) && v == value)
        };
        let mut rules = self.rules.lock().unwrap();
        for scoped in [true, false] {
            for rule in rules.iter_mut() {
                // A scoped rule answers its own account or header and nothing else: a request carrying
                // another credential, another header value, or none, must fall through to the prefix-only
                // rules.
                let admitted = match (&rule.credential, &rule.header) {
                    (Some(c), None) => scoped && Some(c.as_str()) == credential,
                    (None, Some((name, value))) => scoped && has_header(name, value),
                    (Some(c), Some((name, value))) => {
                        scoped && Some(c.as_str()) == credential && has_header(name, value)
                    }
                    (None, None) => !scoped,
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

    /// Blocks until `release()` has latched the gate open or `release_one()` picked this responder. The
    /// `Notified` future is created before the flag check on every loop iteration, so a `release()` landing
    /// between the check and the `.await` is still observed (`Notify` captures that ordering); the flag itself
    /// covers a `release()` that already happened before this call started, which `notify_waiters()` alone would
    /// miss since it only wakes listeners that existed at the time it was called.
    async fn wait_for_release(&self) {
        let (open, opened) = oneshot::channel();
        self.held.lock().unwrap().push_back(open);
        tokio::pin!(opened);
        loop {
            let notified = self.gate.notified();
            if self.released.load(Ordering::SeqCst) {
                return;
            }
            tokio::select! {
                _ = notified => {}
                _ = &mut opened => return,
            }
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
        Wire::Silent => unreachable!("the accept task keeps a silent listener's sockets"),
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
        let index = shared.record(recorded);
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
                shared.mark_written(index);
            }
            // The close is the whole of what these script, so a waiter on it resolves rather than hangs.
            Respond::CloseImmediately | Respond::CloseAfterRequest => {
                shared.mark_written(index);
                return;
            }
            Respond::Gated(_) => unreachable!("the loop above unwraps every gate"),
            Respond::DropAfterHead => {
                if write_drop_head(&mut stream).await.is_ok() {
                    shared.mark_written(index);
                }
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
                shared.mark_written(index);
            }
            Respond::Sse {
                status,
                headers,
                events,
                delay_between,
            } => {
                if write_sse(&mut stream, status, headers, events, delay_between)
                    .await
                    .is_ok()
                {
                    shared.mark_written(index);
                }
                return;
            }
            Respond::SseCut {
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
                for e in &events {
                    if write_chunk(&mut stream, render_event(e).as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                shared.mark_written(index);
                return;
            }
            Respond::SseGated {
                status,
                headers,
                events,
            } => {
                if write_sse_gated(shared, &mut stream, status, headers, events)
                    .await
                    .is_ok()
                {
                    shared.mark_written(index);
                }
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
                if write_chunk_terminator(&mut stream).await.is_ok() {
                    shared.mark_written(index);
                }
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
    let (line, headers, content_length, chunked, head_len) = loop {
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
                    .and_then(|(_, v)| v.trim().parse::<usize>().ok());
                // A declared length wins, so a head carrying both is read the way the framing rules say.
                let chunked = content_length.is_none()
                    && headers.iter().any(|(n, v)| {
                        n.eq_ignore_ascii_case("transfer-encoding")
                            && v.to_ascii_lowercase().contains("chunked")
                    });
                break (line, headers, content_length, chunked, n);
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
    let (body, complete) = if chunked {
        read_chunked_body(stream, buf).await?
    } else {
        read_sized_body(stream, buf, content_length.unwrap_or(0)).await?
    };
    Ok(Some(RecordedRequest {
        connection,
        line,
        headers,
        body,
        complete,
    }))
}

/// `complete` is false when the peer stopped before the declared length arrived.
async fn read_sized_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut BytesMut,
    content_length: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut body = Vec::with_capacity(content_length);
    while body.len() < content_length {
        if buf.is_empty() {
            buf.reserve(4096);
            if stream.read_buf(buf).await? == 0 {
                return Ok((body, false));
            }
        }
        let take = buf.len().min(content_length - body.len());
        body.extend_from_slice(&buf.split_to(take));
    }
    Ok((body, true))
}

/// Reassembles a chunked body into the payload an upstream would have seen, so a test asserts what was sent
/// rather than how it was framed.
async fn read_chunked_body<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut BytesMut,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    loop {
        let Some(header) = read_line(stream, buf).await? else {
            return Ok((body, false));
        };
        // Everything after the semicolon is a chunk extension, which carries nothing the fake records.
        let digits = header.split(';').next().unwrap_or_default().trim();
        let Ok(size) = usize::from_str_radix(digits, 16) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("malformed chunk size {digits:?}"),
            ));
        };
        if size == 0 {
            // Any trailer lines, then the empty line that ends the body.
            return Ok(loop {
                match read_line(stream, buf).await? {
                    None => break (body, false),
                    Some(line) if line.is_empty() => break (body, true),
                    Some(_) => {}
                }
            });
        }
        while buf.len() < size {
            buf.reserve(4096);
            if stream.read_buf(buf).await? == 0 {
                body.extend_from_slice(buf);
                return Ok((body, false));
            }
        }
        body.extend_from_slice(&buf.split_to(size));
        if read_line(stream, buf).await?.is_none() {
            return Ok((body, false));
        }
    }
}

/// One CRLF-terminated line without its terminator; `None` when the peer closed before one arrived.
async fn read_line<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut BytesMut,
) -> std::io::Result<Option<String>> {
    loop {
        if let Some(at) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..at]).into_owned();
            buf.advance(at + 2);
            return Ok(Some(line));
        }
        if buf.len() > MAX_HEAD {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk line too large",
            ));
        }
        buf.reserve(4096);
        if stream.read_buf(buf).await? == 0 {
            return Ok(None);
        }
    }
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

fn generate_tls(hosts: &[&str]) -> GeneratedTls {
    let pair = generate_pair(hosts);
    GeneratedTls {
        acceptor: acceptor_for(&pair),
        ca_pem: pair.ca_pem,
    }
}

/// A CA and a leaf for `hosts`, generated here for a test that serves TLS itself: the PEMs for an operator's pair
/// written to files, the CA for the test's client, and an acceptor for a loop test. Never installed anywhere.
pub struct GeneratedPair {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

impl GeneratedPair {
    /// Writes the pair where an operator's would sit, `<name>.pem` and `<name>.key` under `dir`, and answers the
    /// two paths in that order.
    pub fn write_to(&self, dir: &Path, name: &str) -> (PathBuf, PathBuf) {
        let cert = dir.join(format!("{name}.pem"));
        let key = dir.join(format!("{name}.key"));
        std::fs::write(&cert, &self.cert_pem).expect("write the generated leaf");
        std::fs::write(&key, &self.key_pem).expect("write the generated key");
        (cert, key)
    }
}

pub fn generate_pair(hosts: &[&str]) -> GeneratedPair {
    signed_pair(leaf_params(hosts))
}

/// The same with the leaf's validity window set, for the date refusals' tests.
pub fn generate_pair_valid_between(
    hosts: &[&str],
    not_before: (i32, u8, u8),
    not_after: (i32, u8, u8),
) -> GeneratedPair {
    let mut params = leaf_params(hosts);
    params.not_before = date_time_ymd(not_before.0, not_before.1, not_before.2);
    params.not_after = date_time_ymd(not_after.0, not_after.1, not_after.2);
    signed_pair(params)
}

/// The same pair with the leaf's `not_after` at an exact instant, for a test that pins a day count.
pub fn generate_pair_valid_until(hosts: &[&str], not_after_unix_seconds: i64) -> GeneratedPair {
    let mut params = leaf_params(hosts);
    // rcgen's dates are `time`'s, which this crate does not name: the epoch from rcgen's own constructor, moved by a
    // std duration, is the same instant.
    let epoch = date_time_ymd(1970, 1, 1);
    let offset = Duration::from_secs(not_after_unix_seconds.unsigned_abs());
    params.not_after = if not_after_unix_seconds < 0 {
        epoch - offset
    } else {
        epoch + offset
    };
    signed_pair(params)
}

/// The acceptor shape the router's listeners build: the Mozilla intermediate profile with ALPN fixed to
/// `http/1.1`, so a loop test negotiates what a listener negotiates.
pub fn acceptor_for(pair: &GeneratedPair) -> SslAcceptor {
    let leaf = X509::from_pem(pair.cert_pem.as_bytes()).expect("parse the generated leaf");
    let key = PKey::private_key_from_pem(pair.key_pem.as_bytes()).expect("parse the generated key");
    let mut acceptor =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).expect("build TLS acceptor");
    acceptor
        .set_certificate(&leaf)
        .expect("set leaf certificate");
    acceptor.set_private_key(&key).expect("set leaf key");
    acceptor
        .check_private_key()
        .expect("leaf key matches certificate");
    acceptor.set_alpn_select_callback(|_, offered| {
        select_next_proto(b"\x08http/1.1", offered).ok_or(AlpnError::NOACK)
    });
    acceptor.build()
}

/// A client of a listener serving `pair`: it trusts the pair's CA and offers HTTP/1.1, as a listener's clients do.
pub fn connector_trusting(pair: &GeneratedPair) -> ConnectConfiguration {
    let mut builder = SslConnector::builder(SslMethod::tls()).expect("build TLS connector");
    let ca = X509::from_pem(pair.ca_pem.as_bytes()).expect("parse the generated CA");
    builder
        .cert_store_mut()
        .add_cert(ca)
        .expect("trust the generated CA");
    builder
        .set_alpn_protos(b"\x08http/1.1")
        .expect("offer http/1.1");
    builder
        .build()
        .configure()
        .expect("configure a TLS connection")
}

fn leaf_params(hosts: &[&str]) -> CertificateParams {
    CertificateParams::new(hosts.iter().map(|h| h.to_string()).collect::<Vec<_>>())
        .expect("leaf SAN params")
}

fn signed_pair(leaf_params: CertificateParams) -> GeneratedPair {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("empty-SAN CA params");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let mut ca_name = DistinguishedName::new();
    // Must differ from the leaf's default distinguished name, or BoringSSL reads the leaf's issuer field as
    // equal to its own subject and rejects the chain as self-signed.
    ca_name.push(DnType::CommonName, "fake-upstream-ca");
    ca_params.distinguished_name = ca_name;
    let ca_key = KeyPair::generate().expect("generate CA key");
    let ca_cert = ca_params.self_signed(&ca_key).expect("self-sign CA cert");

    let leaf_key = KeyPair::generate().expect("generate leaf key");
    let leaf_cert = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .expect("sign leaf cert");

    GeneratedPair {
        ca_pem: ca_cert.pem(),
        cert_pem: leaf_cert.pem(),
        key_pem: leaf_key.serialize_pem(),
    }
}
