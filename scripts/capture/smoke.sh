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
python3 - "$TMP/connect.json" <<'PY'
import json, pathlib, sys
recorded = json.loads(pathlib.Path(sys.argv[1]).read_text())
assert len(recorded) == 1, f"connect.json holds {len(recorded)} entries, want 1"
hello = recorded[0]
assert hello.get("hello_len") == 1499, hello.get("hello_len") or hello
assert hello.get("target") == "api.anthropic.com:443", hello.get("target")
sni = next(e.get("sni") for e in hello["extensions"] if e["type"] == 0)
assert sni == "api.anthropic.com", sni
print("smoke passed: connect_capture.py recorded a 1499-byte ClientHello for api.anthropic.com")
PY
