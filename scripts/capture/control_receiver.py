# Answers and records the control calls a capture's client makes: its OAuth token grant and its profile, roles and
# usage reads. The client sends them to hosts it dials by name, which ANTHROPIC_BASE_URL cannot redirect, so the
# capture points HTTPS_PROXY here and NODE_EXTRA_CA_CERTS at the run's CA. Each connection gets a 200 for its CONNECT, a TLS
# handshake with the run's leaf, and a canned answer to the one request inside.
#
# Usage: control_receiver.py <port> <output-json> <seconds> <certs-dir> <responses-dir>
#
# The canned bodies come from <responses-dir>, which a capture points at crates/profile/fixtures/responses, the
# canned responses the fixture builder keeps beside the control fixtures. The receiver's answers and the fixtures
# then share one source and cannot drift apart. Any other request gets a 404 with {}.
#
# It listens on 127.0.0.1 alone and never connects anywhere. The token endpoint answers any grant, so no account is
# involved. Every header and body is recorded verbatim, the authorization header and the token grant's body
# included, which is safe only while the client holds the dummy credentials of scripts/capture/lib.sh and, after
# the grant, the canned token. A recording is raw capture output and stays out of the repository.
#
# <output-json> holds one entry per request, or one error entry for a connection that failed, each carrying the
# seq of its connection. A connection whose answer fails after its request was recorded keeps both entries under
# its seq. The file is rewritten after every entry and on SIGTERM, so a capture stopped early keeps what it saw.
import json, pathlib, signal, socket, ssl, sys, threading, time

if len(sys.argv) != 6:
    sys.exit(f"usage: {sys.argv[0]} <port> <output-json> <seconds> <certs-dir> <responses-dir>")
PORT = int(sys.argv[1])
OUT = sys.argv[2]
DUR = int(sys.argv[3])
CERTS = pathlib.Path(sys.argv[4])
RESPONSES = pathlib.Path(sys.argv[5])

# The query takes no part in the match: the usage read carries one, and a build that adds one to another read
# still gets its canned body while the recording keeps the query.
ANSWERS = {
    ("GET", "/api/oauth/claude_cli/roles"): "roles.json",
    ("GET", "/api/oauth/profile"): "profile.json",
    ("GET", "/api/oauth/usage"): "usage.json",
    ("POST", "/v1/oauth/token"): "token.json",
}
CANNED = {key: (RESPONSES / name).read_bytes() for key, name in ANSWERS.items()}

# The receiver's clients sit on loopback and send their requests at once, so a connection idle this long has
# stalled and is recorded as an error instead of holding its thread.
IDLE_TIMEOUT = 15

entries = []
lock = threading.RLock()


def dump(*_):
    with lock, open(OUT, "w") as f:
        json.dump(entries, f, indent=1)


def record(entry):
    with lock:
        entries.append(entry)
        dump()


signal.signal(signal.SIGTERM, lambda *_: (dump(), sys.exit(0)))

context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(CERTS / "leaf.pem", CERTS / "leaf.key")
# The receiver reads HTTP/1.1 alone, and a client that negotiated h2 would send frames it cannot parse.
context.set_alpn_protocols(["http/1.1"])


def read_head(sock):
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(65536)
        if not chunk:
            raise ConnectionError(f"closed after {len(buf)} bytes of an unfinished head")
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    return head.decode("latin1").split("\r\n"), rest


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
        tls = context.wrap_socket(conn, server_side=True)
        lines, body = read_head(tls)
        length = 0
        for line in lines[1:]:
            name, _, value = line.partition(":")
            if name.strip().lower() == "content-length":
                length = int(value.strip())
        while len(body) < length:
            chunk = tls.recv(65536)
            if not chunk:
                raise ConnectionError(f"closed after {len(body)} of {length} body bytes of {lines[0]}")
            body += chunk
        parts = lines[0].split(" ")
        path = parts[1].split("?", 1)[0] if len(parts) > 1 else ""
        canned = CANNED.get((parts[0], path))
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
