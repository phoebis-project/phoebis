#!/usr/bin/env bash
# The launch script stands in a fenced block of the claude.ai features page and in no file of its own, so this test is
# what runs it. It takes the script from the page, which stays the one source, and runs it under sh with the real curl
# against a loopback listener. A proxy's 502 to the CONNECT has a stop of its own, which the script reaches only where
# curl knows each --write-out name of the probe. The script runs in an empty environment under a throwaway home and
# reaches no host off loopback, and a claude on its PATH fails the case where the script gets as far as the client's
# start.
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

# One listener plays both peers of the script: the router's proxy listener for the CA download, and a proxy that
# answers each CONNECT with 502, as phoebis-proxy does where its upstream is down. It prints its port and the
# download's SHA-256 once it listens. A thread per connection and the read timeout keep it serving past a connection
# that sends nothing.
cat > "$tmp/stub.py" <<'PY'
import hashlib
import http.server

# curl reads the CA file for the TLS handshake, and a refused CONNECT ends ahead of one, so the download needs to be
# no certificate.
CA = b"no certificate\n"


class Stub(http.server.BaseHTTPRequestHandler):
    timeout = 5

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", str(len(CA)))
        self.end_headers()
        self.wfile.write(CA)

    def do_CONNECT(self):
        self.send_response(502)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Stub)
print(server.server_address[1], hashlib.sha256(CA).hexdigest(), flush=True)
server.serve_forever()
PY
mkfifo "$tmp/ready"
python3 -I "$tmp/stub.py" > "$tmp/ready" &
stub_pid=$!
read -r port fingerprint < "$tmp/ready" || fail "the stub listener did not start"

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

mkdir "$tmp/bin"
cat > "$tmp/bin/claude" <<'SH'
#!/bin/sh
echo "the launch script started the client" >&2
exit 1
SH
chmod +x "$tmp/bin/claude"

status=0
out=$(env -i HOME="$tmp/home" PATH="$tmp/bin:$PATH" sh "$tmp/launch.sh" 2>&1) || status=$?
[[ "$out" != *"unknown --write-out variable"* ]] \
  || fail "curl does not know a --write-out name the script asks for: $out"
[[ "$out" == "phoebis: the proxy at http://127.0.0.1:$port does not reach the router's feature listener" ]] \
  || fail "a proxy's 502 to the CONNECT did not reach its own stop (exit $status): $out"

echo "launch-script: ok"
