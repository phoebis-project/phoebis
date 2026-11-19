# Answers and records the control calls a capture's client makes: its OAuth token grant and its profile, roles and usage
# reads. The client sends them to hosts it dials by name, which ANTHROPIC_BASE_URL cannot redirect, so the capture
# points HTTPS_PROXY here and NODE_EXTRA_CA_CERTS at the run's CA. Each connection gets a 200 for its CONNECT, a TLS
# handshake with the run's leaf, and a canned answer to the one request inside.
#
# Usage: control_receiver.py <port> <output-json> <seconds> <certs-dir> <responses-dir> [<hellos-json>]
#
# The canned bodies come from <responses-dir>, which a capture points at crates/profile/fixtures/responses, the
# canned responses the fixture builder keeps beside the control fixtures. The receiver's answers and the fixtures
# then share one source and cannot drift apart. Any other request gets a 404 with {}.
#
# With <hellos-json>, as scripts/capture-feature-hellos.sh starts it, the receiver also writes each connection's
# ClientHello there under the connection's seq, and answers the connector list and a connector's MCP initialize with
# the invented bodies below, so the client goes on to open a connector session. It answers an artifact's publish and
# its page read the same way, so the client goes on to arm the artifact's watch. The control capture leaves both out,
# so its client meets the receiver its fixtures were captured against.
#
# It listens on 127.0.0.1 alone and never connects anywhere. The token endpoint answers any grant, so no account is
# involved. Every header and body is recorded verbatim, the authorization header and the token grant's body
# included, which is safe only while the client holds the dummy credentials of scripts/capture/lib.sh and, after
# the grant, the canned token. A recording is raw capture output and stays out of the repository.
#
# <output-json> holds one entry per request, or one error entry for a connection that failed or sent a chunked body,
# each carrying the seq of its connection. A connection whose answer fails after its request was recorded keeps both
# entries under its seq. The file is rewritten after every entry and on SIGTERM, so a capture stopped early keeps what
# it saw.
import base64, json, pathlib, signal, socket, ssl, sys, threading, time

if len(sys.argv) not in (6, 7):
    sys.exit(f"usage: {sys.argv[0]} <port> <output-json> <seconds> <certs-dir> <responses-dir> [<hellos-json>]")
PORT = int(sys.argv[1])
OUT = sys.argv[2]
DUR = int(sys.argv[3])
CERTS = pathlib.Path(sys.argv[4])
RESPONSES = pathlib.Path(sys.argv[5])
HELLOS = sys.argv[6] if len(sys.argv) == 7 else None

# The query takes no part in the match: the usage read carries one, and a build that adds one to another read
# still gets its canned body while the recording keeps the query.
ANSWERS = {
    ("GET", "/api/oauth/claude_cli/roles"): "roles.json",
    ("GET", "/api/oauth/profile"): "profile.json",
    ("GET", "/api/oauth/usage"): "usage.json",
    ("POST", "/v1/oauth/token"): "token.json",
}
CANNED = {key: (RESPONSES / name).read_bytes() for key, name in ANSWERS.items()}

# Invented for the feature capture, of no account: one connector under a name of its own, and the result an MCP
# server gives an initialize request.
CONNECTOR_LIST = json.dumps({
    "data": [{
        "additional_install": False,
        "created_at": "2026-01-01T00:00:00Z",
        "directory_entry_id": None,
        "display_name": "Example Connector",
        "icon_url": None,
        "id": "srv_example",
        "tools": [],
        "url": "https://connector.example/mcp",
    }],
    "next_page": None,
}).encode()
INITIALIZE_RESULT = {
    "capabilities": {"tools": {}},
    "protocolVersion": "2026-07-28",
    "serverInfo": {"name": "example-connector", "version": "1.0.0"},
}


def sync_token():
    """A sync token of the shape the client checks before it dials the artifact watch's socket: the account UUID's text
    as base64url, the organization UUID, a role that ends in _agent, the issue and the end in Unix seconds, and 43
    letters where the signature stands. The two UUIDs are the canned profile's, so the token names the session's own
    account."""
    profile = json.loads(CANNED[("GET", "/api/oauth/profile")])
    account = base64.urlsafe_b64encode(profile["account"]["uuid"].encode()).decode()
    start = int(time.time())
    return ".".join([account, profile["organization"]["uuid"], "owner_agent", str(start), str(start + 900), "A" * 43])


# Invented too: the publish of one artifact and its page read. The client keeps the slug only where it reads as a
# lowercase UUID, and the page read's syncClient and syncToken send its watch to the socket on api.anthropic.com.
ARTIFACT_SLUG = "00000000-0000-4000-8000-000000000003"
ARTIFACT_ANSWERS = {
    ("GET", f"/api/frame/{ARTIFACT_SLUG}"): json.dumps({
        "assetToken": "capture-asset-token",
        "syncClient": True,
        "syncToken": sync_token(),
        "ver": "1",
    }).encode(),
    ("POST", "/api/frame/deploy/direct"): json.dumps({"slug": ARTIFACT_SLUG, "version": "1"}).encode(),
    ("POST", "/api/frame/deploy/prepare"): json.dumps({"slug": ARTIFACT_SLUG}).encode(),
}

# The receiver's clients sit on loopback and send their requests at once, so a connection idle this long has
# stalled and is recorded as an error instead of holding its thread.
IDLE_TIMEOUT = 15

entries = []
hellos = []
lock = threading.RLock()


def dump(*_):
    with lock:
        with open(OUT, "w") as f:
            json.dump(entries, f, indent=1)
        if HELLOS is not None:
            with open(HELLOS, "w") as f:
                json.dump(hellos, f, indent=1)


def record(entry):
    with lock:
        entries.append(entry)
        dump()


signal.signal(signal.SIGTERM, lambda *_: (dump(), sys.exit(0)))

context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(CERTS / "leaf.pem", CERTS / "leaf.key")
# The receiver reads HTTP/1.1 alone, and a client that negotiated h2 would send frames it cannot parse.
context.set_alpn_protocols(["http/1.1"])
if HELLOS is not None:
    # A client that resumes a session offers a pre-shared key its first hello lacks, so without a ticket each recorded
    # hello is the one a connection to a new host opens with.
    context.num_tickets = 0


def read_head(sock):
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(65536)
        if not chunk:
            raise ConnectionError(f"closed after {len(buf)} bytes of an unfinished head")
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    return head.decode("latin1").split("\r\n"), rest


def peek_hello(sock):
    """The client's first TLS record, left on the socket for the handshake to read. A record that is not whole within
    IDLE_TIMEOUT is a stalled connection, as the constant has it, and raises."""
    want = 5
    deadline = time.monotonic() + IDLE_TIMEOUT
    while True:
        record = sock.recv(want, socket.MSG_PEEK)
        if not record:
            raise ConnectionError("closed before its ClientHello")
        if len(record) >= 5:
            want = 5 + int.from_bytes(record[3:5], "big")
        if len(record) >= want:
            return record
        if time.monotonic() >= deadline:
            raise TimeoutError(f"{len(record)} of a {want}-byte first record after {IDLE_TIMEOUT} seconds")
        # A peek answers at once with the bytes already there, so a record still arriving is waited for here.
        time.sleep(0.01)


def feature_answer(method, path, body):
    """The invented body of a feature call the capture serves, None for any other call. An initialize answer carries
    the request's own id, which the client matches it by."""
    if (method, path) == ("GET", "/v1/mcp_servers"):
        return CONNECTOR_LIST
    if (method, path) in ARTIFACT_ANSWERS:
        return ARTIFACT_ANSWERS[(method, path)]
    if method != "POST" or not path.startswith("/v1/mcp/"):
        return None
    try:
        message = json.loads(body)
    except ValueError:
        return None
    if not isinstance(message, dict) or message.get("method") != "initialize":
        return None
    return json.dumps({"id": message.get("id"), "jsonrpc": "2.0", "result": INITIALIZE_RESULT}).encode()


def response(code, reason, body):
    head = f"HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n"
    return head.encode() + b"Connection: close\r\n\r\n" + body


def handle(conn, seq):
    target = "?"
    tls = None
    try:
        conn.settimeout(IDLE_TIMEOUT)
        lines, _ = read_head(conn)
        if not lines[0].startswith("CONNECT "):
            raise ValueError(f"not a CONNECT request: {lines[0]}")
        target = lines[0].split(" ")[1]
        conn.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
        if HELLOS is not None:
            hello = peek_hello(conn)
            with lock:
                hellos.append({"hello": hello.hex(), "seq": seq, "target": target})
                dump()
        tls = context.wrap_socket(conn, server_side=True)
        lines, body = read_head(tls)
        length = 0
        for line in lines[1:]:
            name, _, value = line.partition(":")
            if name.strip().lower() == "content-length":
                length = int(value.strip())
            # The body is framed by Content-Length alone, so a chunked one would be recorded cut short as if whole.
            elif name.strip().lower() == "transfer-encoding" and "chunked" in value.lower():
                raise ValueError(f"a chunked body, which the receiver does not read, on {lines[0]}")
        while len(body) < length:
            chunk = tls.recv(65536)
            if not chunk:
                raise ConnectionError(f"closed after {len(body)} of {length} body bytes of {lines[0]}")
            body += chunk
        parts = lines[0].split(" ")
        path = parts[1].split("?", 1)[0] if len(parts) > 1 else ""
        canned = CANNED.get((parts[0], path))
        if canned is None and HELLOS is not None:
            canned = feature_answer(parts[0], path, body)
        code, reason, payload = (200, "OK", canned) if canned is not None else (404, "Not Found", b"{}")
        # Recorded before the answer goes out, so the entry is on disk by the time the client acts on the answer.
        record({"seq": seq, "target": target, "request_line": lines[0], "headers": lines[1:],
                "body": body.decode("latin1"), "answered": code})
        tls.sendall(response(code, reason, payload))
    except Exception as e:
        record({"seq": seq, "target": target, "error": repr(e)})
    finally:
        # wrap_socket takes the plain socket over and leaves its close a no-op, and a failed handshake closes the
        # TLS socket itself before tls is assigned.
        conn.close()
        if tls is not None:
            tls.close()


server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", PORT))
server.listen(32)
# start_control_receiver in scripts/capture/lib.sh waits for this file as the sign the port is bound, and a bind
# failure exits before it is written.
dump()
server.settimeout(1)
end = time.time() + DUR
# Numbered at accept, so seq follows the order the client opened its connections even where a slow request is
# recorded after a later one.
seq = 0
while time.time() < end:
    try:
        conn, _ = server.accept()
    except socket.timeout:
        continue
    threading.Thread(target=handle, args=(conn, seq), daemon=True).start()
    seq += 1
dump()
