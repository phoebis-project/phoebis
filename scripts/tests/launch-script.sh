#!/usr/bin/env bash
# The launch script stands in a fenced block of the claude.ai features page and in no file of its own, so this test is
# what runs it. It takes the script from the page, which stays the one source, and runs it under sh with the real curl
# against loopback stubs. A proxy's 502 to the CONNECT has a stop of its own, which the script reaches only where curl
# knows each --write-out name of the probe. The script runs in an empty environment under a throwaway home and reaches
# no host off loopback, and a claude on its PATH fails a case where the script gets as far as the client's start.
set -euo pipefail

here=$(cd "$(dirname "$0")/../.." && pwd)
page="$here/docs/site/claude-ai-features.md"
tmp=$(mktemp -d)
stub_pid=
stop_stub() {
  [[ -n "$stub_pid" ]] || return 0
  kill "$stub_pid" 2>/dev/null || true
  wait "$stub_pid" 2>/dev/null || true
}
trap 'stop_stub; rm -rf "$tmp"' EXIT
fail() { echo "FAIL $1" >&2; exit 1; }

awk '/^```sh$/ { on = 1; next } on && /^```$/ { exit } on' "$page" > "$tmp/launch.sh"
[[ "$(head -n 1 "$tmp/launch.sh")" == '#!/bin/sh' ]] || fail "the page's first sh block is no launch script"

# One listener plays both peers of the script: the router's proxy listener for the CA download, and a proxy that answers
# each CONNECT with 502, as phoebis-proxy does where its upstream is down. A second listener plays a proxy that answers
# each CONNECT with 403. A third socket is bound and never listens, so its port refuses each connection, and no other
# process can take that port while the test runs. The stub prints the first listener's port, the download's SHA-256 and
# the two other ports once it listens. A thread per connection and the read timeout keep a listener serving past a
# connection that sends nothing.
cat > "$tmp/stub.py" <<'PY'
import hashlib
import http.server
import socket
import threading

# curl reads the CA file for the TLS handshake, and a refused CONNECT ends ahead of one, so the download needs to be
# no certificate.
CA = b"no certificate\n"


class Stub(http.server.BaseHTTPRequestHandler):
    timeout = 5
    connect_status = 502

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(CA)))
        self.end_headers()
        self.wfile.write(CA)

    def do_CONNECT(self):
        self.send_response(self.connect_status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


class Refusing(Stub):
    connect_status = 403


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Stub)
refusing = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Refusing)
threading.Thread(target=refusing.serve_forever, daemon=True).start()
closed = socket.socket()
closed.bind(("127.0.0.1", 0))
print(
    server.server_address[1],
    hashlib.sha256(CA).hexdigest(),
    refusing.server_address[1],
    closed.getsockname()[1],
    flush=True,
)
server.serve_forever()
PY
mkfifo "$tmp/ready"
python3 -I "$tmp/stub.py" > "$tmp/ready" &
stub_pid=$!
read -r port fingerprint refusing closed < "$tmp/ready" || fail "the stub listener did not start"

# A fill that matches no line would leave the page's own value in the script, and the page's SETUP or PROXY_URL could
# then send a probe off the stub.
fill() {
  grep -q "^$1='" "$tmp/launch.sh" || fail "the page's script has no input line for $1"
  sed "s|^$1='[^']*'|$1='$2'|" "$tmp/launch.sh" > "$tmp/filled"
  mv "$tmp/filled" "$tmp/launch.sh"
}
fill BASE_URL "http://127.0.0.1:$port"
fill ROUTER_KEY test-key
fill PHOEBIS_FEATURE_CA_SHA256 "$fingerprint"
fill SETUP proxy
fill PROXY_URL "http://127.0.0.1:$port"

# The page's script decides where curl connects, so an edit there that takes the proxy off a probe or sends the run
# into the redirect's branch would send a request to the real feature hosts. curl reads $HOME/.curlrc ahead of each
# call that does not open with -q, and this line sends a connection to the stub whatever host and port the URL names.
# A call through a proxy connects to the proxy's address, which each fill of PROXY_URL sets to a port of the stub.
mkdir "$tmp/home"
printf 'connect-to = "::127.0.0.1:%s"\n' "$port" > "$tmp/home/.curlrc"

mkdir "$tmp/bin"
cat > "$tmp/bin/claude" <<'SH'
#!/bin/sh
echo "the launch script started the client" >&2
exit 1
SH
chmod +x "$tmp/bin/claude"

launch() {
  status=0
  out=$(env -i HOME="$tmp/home" PATH="$tmp/bin:$PATH" sh "$tmp/launch.sh" 2>&1) || status=$?
}

launch
[[ "$out" != *"unknown --write-out variable"* ]] \
  || fail "curl does not know a --write-out name the script asks for: $out"
[[ "$out" == "phoebis: the proxy at http://127.0.0.1:$port does not reach the router's feature listener" ]] \
  || fail "a proxy's 502 to the CONNECT did not reach its own stop (exit $status): $out"

# curl 8.18.0 exits 7 for a CONNECT that a proxy refuses, which is curl's exit where nothing listens at the proxy's
# address. Arms that sent each exit 7 to the stop of the 502 would name a proxy that does not run as one whose upstream
# is down.
fill PROXY_URL "http://127.0.0.1:$closed"
launch
[[ "$out" == "phoebis: no proxy runs at http://127.0.0.1:$closed" ]] \
  || fail "a proxy port where nothing listens did not reach its own stop (exit $status): $out"

# A proxy's other status than 502 belongs to the stop that prints both status codes. Under curl 8.18.0 an arm that read
# exit 7 without the proxy's status would name this proxy as one that does not run. The stop's line carries curl's exit
# for the refused CONNECT, which is 56 under curl 8.14.1 and 7 under curl 8.18.0, so the case leaves it open.
fill PROXY_URL "http://127.0.0.1:$refusing"
launch
[[ "$out" == "phoebis: the check of the road to api.anthropic.com failed (curl exit "*", answer 000 403)" ]] \
  || fail "a proxy's 403 to the CONNECT did not reach the stop that prints it (exit $status): $out"

echo "launch-script: ok"
