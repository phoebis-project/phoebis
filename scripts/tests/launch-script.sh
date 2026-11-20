#!/usr/bin/env bash
# The launch script stands in a fenced block of the claude.ai features page and in no file of its own, so this test is
# what runs it. It takes the script from the page, which stays the one source, and runs it under sh with the real curl
# against loopback stubs, two of which speak TLS under a CA the test makes with openssl. A proxy's 502 to the CONNECT
# has a stop of its own, which the script reaches only where curl knows each --write-out name of the probe. The script
# runs in an empty environment under a throwaway home and reaches no host off loopback. A claude on its PATH prints one
# line, which fails a case that expects a stop and ends a case of the sound road.
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

# The CA carries the name constraint of the router's feature CA, so a curl that refuses such a CA under --cacert fails
# the sound road here as it would fail it at a user's start.
command -v openssl > /dev/null || fail "openssl is missing"
{
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 2 -subj '/CN=Phoebis feature CA' \
    -keyout "$tmp/ca.key" -out "$tmp/ca.pem" \
    -addext 'basicConstraints=critical,CA:TRUE,pathlen:0' -addext 'keyUsage=critical,keyCertSign' \
    -addext 'nameConstraints=critical,permitted;DNS:api.anthropic.com,permitted;DNS:mcp-proxy.anthropic.com,excluded;IP:0.0.0.0/0.0.0.0,excluded;IP:0:0:0:0:0:0:0:0/0:0:0:0:0:0:0:0' &&
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -subj '/CN=api.anthropic.com' \
    -keyout "$tmp/leaf.key" -out "$tmp/leaf.csr" &&
  printf 'subjectAltName=DNS:api.anthropic.com,DNS:mcp-proxy.anthropic.com\n' > "$tmp/leaf.ext" &&
  openssl x509 -req -in "$tmp/leaf.csr" -CA "$tmp/ca.pem" -CAkey "$tmp/ca.key" -CAcreateserial -days 2 \
    -extfile "$tmp/leaf.ext" -out "$tmp/leaf.pem"
} 2> "$tmp/openssl.log" || fail "openssl did not make the test's CA and leaf: $(cat "$tmp/openssl.log")"

# One listener plays both peers of the script: the router's proxy listener for the CA download, and a proxy that answers
# each CONNECT with 502, as phoebis-proxy does where its upstream is down. A second listener plays a proxy that answers
# each CONNECT with 403. A third socket is bound and never listens, so its port refuses each connection, and no other
# process can take that port while the test runs. A fourth listener plays a sound proxy: it answers the CONNECT with 200
# and speaks TLS under the test's leaf on that connection. A fifth plays the router's feature listener, with TLS under
# the leaf from the first byte. Both answer a request the way the router answers a call without a key. The stub prints
# the first listener's port, the download's SHA-256 and the four other ports once it listens. A thread per connection
# and the read timeout keep a listener serving past a connection that sends nothing, so each TLS handshake runs in its
# connection's thread under that timeout and never in the thread that accepts.
cat > "$tmp/stub.py" <<'PY'
import hashlib
import http.server
import socket
import socketserver
import ssl
import sys
import threading

ca_file, leaf_file, key_file, downloads = sys.argv[1:]

# The download is the test CA's certificate, so the fingerprint the test fills is that file's, and the handshake with
# either TLS listener passes under it as curl's CA file.
with open(ca_file, "rb") as f:
    CA = f.read()

TLS = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
TLS.load_cert_chain(leaf_file, key_file)

UNAUTHORIZED = (
    b"HTTP/1.1 401 Unauthorized\r\n"
    b"x-phoebis-error-code: auth_missing\r\n"
    b"Content-Length: 0\r\n"
    b"Connection: close\r\n"
    b"\r\n"
)


def answer_without_key(connection):
    try:
        with TLS.wrap_socket(connection, server_side=True) as tls:
            head = b""
            while b"\r\n\r\n" not in head:
                piece = tls.recv(4096)
                if not piece:
                    return
                head += piece
            tls.sendall(UNAUTHORIZED)
    except OSError:
        pass


class Stub(http.server.BaseHTTPRequestHandler):
    timeout = 5
    connect_status = 502

    def do_GET(self):
        # The line is in the record and the file closed ahead of the answer, so a read of the record once the script
        # ended holds each fetch the script made.
        with open(downloads, "a") as record:
            record.write(self.requestline + "\n")
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


class SoundProxy(Stub):
    def do_CONNECT(self):
        self.send_response(200)
        self.end_headers()
        self.close_connection = True
        answer_without_key(self.connection)


class FeatureListener(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(Stub.timeout)
        answer_without_key(self.request)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Stub)
refusing = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Refusing)
sound = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SoundProxy)
direct = socketserver.ThreadingTCPServer(("127.0.0.1", 0), FeatureListener)
for listener in (refusing, sound, direct):
    threading.Thread(target=listener.serve_forever, daemon=True).start()
closed = socket.socket()
closed.bind(("127.0.0.1", 0))
print(
    server.server_address[1],
    hashlib.sha256(CA).hexdigest(),
    refusing.server_address[1],
    closed.getsockname()[1],
    sound.server_address[1],
    direct.server_address[1],
    flush=True,
)
server.serve_forever()
PY
mkfifo "$tmp/ready"
python3 -I "$tmp/stub.py" "$tmp/ca.pem" "$tmp/leaf.pem" "$tmp/leaf.key" "$tmp/downloads" > "$tmp/ready" &
stub_pid=$!
read -r port fingerprint refusing closed good direct < "$tmp/ready" || fail "the stub listener did not start"

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

mkdir "$tmp/home" "$tmp/bin"

# The page's script decides where curl connects, so an edit there that takes the proxy off a probe or sends the run
# into the redirect's branch would send a request to the real feature hosts. Each curl call of the script opens with
# -q and so reads no .curlrc. This stand-in therefore runs the host's curl with the words of the rules file ahead of
# the call's own arguments and behind a leading -q, which curl reads in the first place alone, and each exit and each
# --write-out value stays the real tool's. curl takes the first rule that fits, so a case writes its own rules ahead
# of the last one, which sends a connection to the stub whatever host and port the URL names. A call through a proxy
# connects to the proxy's address, which each fill of PROXY_URL sets to a port of the stub.
real_curl=$(command -v curl) || fail "curl is missing"
printf -- '--connect-to ::127.0.0.1:%s\n' "$port" > "$tmp/curl-rules"
cat > "$tmp/bin/curl" <<SH
#!/bin/sh
set -f
rules=\$(cat '$tmp/curl-rules')
if [ "\$1" = -q ]; then
  shift
  exec '$real_curl' -q \$rules "\$@"
fi
exec '$real_curl' \$rules "\$@"
SH
chmod +x "$tmp/bin/curl"

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

# No other case gets as far as the client's start, so a success arm that never matches, or a header read that finds no
# code, would stop each sound setup unseen.
fill PROXY_URL "http://127.0.0.1:$good"
launch
[[ "$out" == "the launch script started the client" ]] \
  || fail "a sound setup behind a proxy did not reach the client's start (exit $status): $out"

# The fetch may run over plain HTTP, so the compare with the operator's fingerprint is the one check the CA's trust
# rests on. The fetched CA is the sound one, so a compare that passed it would go on to the client's start.
fill PHOEBIS_FEATURE_CA_SHA256 0000000000000000000000000000000000000000000000000000000000000000
launch
[[ "$out" == "phoebis: the fetched CA does not have the operator's fingerprint. Get the fingerprint from the router's operator" ]] \
  || fail "a CA of another fingerprint passed the compare (exit $status): $out"

# A client started with an empty token can fall back to a login of its own. A key check behind the fetch prints the
# same stop, so the download's record tells the two apart.
fill PHOEBIS_FEATURE_CA_SHA256 "$fingerprint"
fill ROUTER_KEY ''
: > "$tmp/downloads"
launch
[[ "$out" == "phoebis: ROUTER_KEY holds no key. Set it to the key the router's operator handed over" ]] \
  || fail "an empty ROUTER_KEY did not stop the start (exit $status): $out"
[[ ! -s "$tmp/downloads" ]] || fail "an empty ROUTER_KEY stopped the start behind the CA fetch: $(cat "$tmp/downloads")"

# The client reads no .curlrc, so a probe that followed the user's file would prove a road the client never takes, and
# the client would carry the router key along its own. Here the file leads both names to the sound stand-in of the
# feature listener, while the rules lead to the first listener, which speaks no TLS.
cat > "$tmp/home/.curlrc" <<EOF
connect-to = "api.anthropic.com:443:127.0.0.1:$direct"
connect-to = "mcp-proxy.anthropic.com:443:127.0.0.1:$direct"
EOF
fill ROUTER_KEY test-key
fill SETUP redirect
launch
[[ "$out" == "phoebis: api.anthropic.com leads to another host than the router's feature listener over ipv4" ]] \
  || fail "a .curlrc in the user's home led a probe of the redirect (exit $status): $out"
rm "$tmp/home/.curlrc"
fill SETUP proxy

echo "launch-script: ok"
