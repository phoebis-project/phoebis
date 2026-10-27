#!/usr/bin/env bash
# Checks both capture receivers record what scripts/build-fixtures.py reads, without running the client: a wrong
# output shape would otherwise only surface at the end of a five-minute capture.
#
# Usage: scripts/capture/smoke.sh
#
# Talks to 127.0.0.1 only, and the ClientHello it replays is the committed one, so nothing here needs a client
# binary, a credential, or a network.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

BODY='{"model":"smoke","messages":[{"role":"user","content":"hi"}]}'

HTTP_PORT=$(free_port)
python3 "$HERE/http_raw.py" "$HTTP_PORT" "$TMP/http" 30 &
HTTP_PID=$!
sleep 1
curl -sS -o /dev/null -X POST "http://127.0.0.1:$HTTP_PORT/v1/messages?beta=true" \
  -H 'content-type: application/json' --data "$BODY"
kill "$HTTP_PID" 2>/dev/null || true
wait "$HTTP_PID" 2>/dev/null || true
python3 - "$TMP/http" "$BODY" <<'PY'
import json, pathlib, sys
out, body = pathlib.Path(sys.argv[1]), sys.argv[2]
modes = json.loads((out / "clean_modes.json").read_text())
assert len(modes) == 1, f"clean_modes.json holds {len(modes)} entries, want 1"
entry = modes[0]
assert list(entry) == ["t", "request_line", "headers", "body"], f"wrong entry shape: {list(entry)}"
assert entry["request_line"] == "POST /v1/messages?beta=true HTTP/1.1", entry["request_line"]
assert any(h.lower().startswith("content-length:") for h in entry["headers"]), entry["headers"]
assert entry["body"] == body, entry["body"]
raw = (out / "clean_body_0.json").read_bytes()
assert raw == body.encode(), raw
print("smoke passed: http_raw.py wrote clean_modes.json and clean_body_0.json")
PY

CONNECT_PORT=$(free_port)
python3 "$HERE/connect_capture.py" "$CONNECT_PORT" "$TMP/connect.json" 30 &
CONNECT_PID=$!
sleep 1
python3 - "$CONNECT_PORT" "$ROOT/crates/profile/fixtures/hello_inference.hex" <<'PY'
import socket, sys
port, hello = int(sys.argv[1]), bytes.fromhex(open(sys.argv[2]).read().strip())
c = socket.create_connection(("127.0.0.1", port), 5)
c.settimeout(5)
c.sendall(b"CONNECT api.anthropic.com:443 HTTP/1.1\r\nHost: api.anthropic.com:443\r\n\r\n")
reply = c.recv(4096)
assert reply.startswith(b"HTTP/1.1 200"), reply
c.sendall(hello)
c.close()
PY
sleep 1
kill "$CONNECT_PID" 2>/dev/null || true
wait "$CONNECT_PID" 2>/dev/null || true
python3 - "$TMP/connect.json" "$ROOT/crates/profile/fixtures/hello_inference.hex" <<'PY'
import json, pathlib, sys
recorded = json.loads(pathlib.Path(sys.argv[1]).read_text())
hello_bytes = bytes.fromhex(pathlib.Path(sys.argv[2]).read_text().strip())
assert len(recorded) == 1, f"connect.json holds {len(recorded)} entries, want 1"
hello = recorded[0]
# connect_capture.py records the handshake's length, without the five-byte record header the hex fixture
# carries ahead of it.
assert hello.get("hello_len") == len(hello_bytes) - 5, hello.get("hello_len") or hello
assert hello.get("target") == "api.anthropic.com:443", hello.get("target")
sni = next(e.get("sni") for e in hello["extensions"] if e["type"] == 0)
assert sni == "api.anthropic.com", sni
print(f"smoke passed: connect_capture.py recorded a {hello['hello_len']}-byte ClientHello for api.anthropic.com")
PY

CA_DIR="$TMP/ca"
mkdir -p "$CA_DIR"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=capture-ca \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign \
  -keyout "$CA_DIR/ca.key" -out "$CA_DIR/ca.pem" -days 1 >/dev/null 2>&1
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=api.anthropic.com \
  -addext subjectAltName=DNS:api.anthropic.com -keyout "$CA_DIR/leaf.key" -out "$CA_DIR/leaf.csr" >/dev/null 2>&1
openssl x509 -req -CA "$CA_DIR/ca.pem" -CAkey "$CA_DIR/ca.key" -copy_extensions copy \
  -in "$CA_DIR/leaf.csr" -out "$CA_DIR/leaf.pem" -days 1 -CAcreateserial >/dev/null 2>&1

TLS_PORT=$(free_port)
python3 "$HERE/tls_raw.py" "$TLS_PORT" "$TMP/tls" 30 "$CA_DIR/leaf.pem" "$CA_DIR/leaf.key" &
TLS_PID=$!
sleep 1
TOKEN_BODY='{"grant_type":"refresh_token","refresh_token":"sk-ant-ort01-notarealsecret"}'
COUNT_BODY='{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}]}'
python3 - "$TLS_PORT" "$CA_DIR/ca.pem" "$BODY" "$TOKEN_BODY" "$COUNT_BODY" <<'PY'
import socket, ssl, sys
port, ca_pem, body, token_body, count_body = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
ctx = ssl.create_default_context(cafile=ca_pem)

def send(target, extra_body, declared=None):
    raw = socket.create_connection(("127.0.0.1", port), 5)
    c = ctx.wrap_socket(raw, server_hostname="api.anthropic.com")
    request = (
        f"{target} HTTP/1.1\r\n"
        "Host: api.anthropic.com\r\n"
        "Authorization: Bearer test\r\n"
        "content-type: application/json\r\n"
        f"Content-Length: {len(extra_body) if declared is None else declared}\r\n"
        "\r\n"
        f"{extra_body}"
    )
    c.sendall(request.encode())
    c.recv(4096)
    c.close()

# Each is a fresh connection: tls_raw.py answers one request per connection, as http_raw.py does.
send("POST /v1/messages?beta=true", body)
send("POST /v1/oauth/token", token_body)
# A path this tool does not claim to capture, and a prefix of one it does: the match has to be exact, not a
# prefix, or this line would be graded as a POST /v1/messages request and its body kept in the clear.
send("POST /v1/messages/count_tokens", count_body)
# Two mis-framed Messages requests. The builder regenerates Content-Length from the stored body and the router
# grading masks the field, so neither would show up downstream: the receiver has to refuse them here.
send("POST /v1/messages", body, declared=len(body) - 1)
send("POST /v1/messages", body, declared=len(body) + 1)
PY
kill "$TLS_PID" 2>/dev/null || true
wait "$TLS_PID" 2>/dev/null || true
python3 - "$TMP/tls" "$BODY" "$TOKEN_BODY" "$COUNT_BODY" <<'PY'
import json, pathlib, sys
out, body, token_body, count_body = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
modes = json.loads((out / "clean_modes.json").read_text())
assert len(modes) == 5, f"clean_modes.json holds {len(modes)} entries, want 5"
entry = modes[0]
assert entry["request_line"] == "POST /v1/messages?beta=true HTTP/1.1", entry["request_line"]
assert "Authorization: REDACTED" in entry["headers"], entry["headers"]
assert entry["body"] == body, entry["body"]
raw = (out / "clean_body_0.json").read_bytes()
assert raw == body.encode(), raw
# A request line other than POST /v1/messages redacts its body too: nothing here proves this receiver would
# not otherwise capture a live refresh token verbatim (see tls_raw.py's header comment).
token_entry = modes[1]
assert token_entry["request_line"] == "POST /v1/oauth/token HTTP/1.1", token_entry["request_line"]
assert token_entry["body"] == "REDACTED", token_entry["body"]
assert token_body not in token_entry["body"], "the refresh token reached the JSON copy"
raw = (out / "clean_body_1.json").read_bytes()
assert raw == b"REDACTED", raw
# A prefix match would treat this as a POST /v1/messages request and keep its body; the match must be exact.
count_entry = modes[2]
assert count_entry["request_line"] == "POST /v1/messages/count_tokens HTTP/1.1", count_entry["request_line"]
assert count_entry["body"] == "REDACTED", count_entry["body"]
raw = (out / "clean_body_2.json").read_bytes()
assert raw == b"REDACTED", raw
# A declared length the body overruns and one it never reaches: each is recorded as the framing error it is,
# with no body file, so scripts/build-fixtures.py fails on the entry instead of regenerating a plausible one.
extra, short = modes[3], modes[4]
assert extra["body"] == f"FRAMING-ERROR: extra body {len(body)} of {len(body) - 1}", extra["body"]
assert short["body"] == f"FRAMING-ERROR: short body {len(body)} of {len(body) + 1}", short["body"]
for idx in (3, 4):
    assert not (out / f"clean_body_{idx}.json").exists(), f"clean_body_{idx}.json was written for a mis-framed request"
connect = json.loads((out / "connect.json").read_text())
assert len(connect) == 5, f"connect.json holds {len(connect)} entries, want 5"
assert all(c.get("hello_len", 0) > 0 for c in connect), connect
print("smoke passed: tls_raw.py recorded the request behind TLS, redacted the credential, redacted a non-messages body, and refused both mis-framed requests")
PY
