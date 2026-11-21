#!/usr/bin/env bash
# The launch script stands in a fenced block of the claude.ai features page and in no file of its own, so this test is
# what runs it. It takes the script from the page, which stays the one source, and runs it under sh with the real curl
# against loopback stubs, four of which speak TLS under a CA the test makes with openssl. A proxy's 502 to the CONNECT
# has a stop of its own, which the script reaches only where curl knows each --write-out name of the probe. The script
# runs in an empty environment under a throwaway home and reaches no host off loopback. Under PRIVATE_HOSTS=yes it meets
# stand-ins of sudo, unshare, mount and setpriv, so no run gets root or binds a file over the machine's hosts file. A
# claude on its PATH records its arguments, its environment and its umask and prints one line, which fails a case that
# expects a stop and ends a case of the sound road.
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
# the leaf from the first byte, and a sixth plays it too, on ::1 at the first listener's port. So a rule toward
# localhost at that port leads a probe over IPv4 to a peer that speaks no TLS and a probe over IPv6 to the router. A
# seventh plays it as well, on ::1 at the fifth's port, since a rule names one port, and a start that proves both
# families needs the router to answer at that port over each. Each TLS listener answers a request the way the router
# answers a call without a key. The stub prints the first listener's port, the download's SHA-256, the four other ports
# and yes or no for the two listeners on ::1 once it listens, yes where both stand, since a host without ::1, or another
# process on one of their ports there, leaves one or both out. A thread per connection and the read timeout keep a
# listener serving past a connection that sends nothing, so each TLS handshake runs in its connection's thread under
# that timeout and never in the thread that accepts.
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
# each TLS listener passes under it as curl's CA file.
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


class IPv6Server(socketserver.ThreadingTCPServer):
    address_family = socket.AF_INET6


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Stub)
refusing = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Refusing)
sound = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SoundProxy)
direct = socketserver.ThreadingTCPServer(("127.0.0.1", 0), FeatureListener)
listeners = [refusing, sound, direct]
try:
    listeners.append(IPv6Server(("::1", server.server_address[1]), FeatureListener))
    listeners.append(IPv6Server(("::1", direct.server_address[1]), FeatureListener))
    v6 = "yes"
except OSError:
    v6 = "no"
for listener in listeners:
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
    v6,
    flush=True,
)
server.serve_forever()
PY
mkfifo "$tmp/ready"
python3 -I "$tmp/stub.py" "$tmp/ca.pem" "$tmp/leaf.pem" "$tmp/leaf.key" "$tmp/downloads" > "$tmp/ready" &
stub_pid=$!
read -r port fingerprint refusing closed good direct v6 < "$tmp/ready" || fail "the stub listener did not start"

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

# The page's script decides where curl connects, so an edit there that takes the proxy off a probe or sends the run into
# the redirect's branch would send a request to the real feature hosts. Each curl call of the script opens with -q and
# so reads no .curlrc. This stand-in therefore runs the host's curl with the words of the rules file ahead of the call's
# own arguments and behind a leading -q, which curl reads in the first place alone, and each exit and each --write-out
# value stays the real tool's. curl takes the first rule that fits, so a case writes its own rules ahead of the last
# one, which sends a connection to the stub whatever host and port the URL names. The stand-in stops where the rules
# lack that last one, so a case that writes its rules wrong cannot dial a real host. A call through a proxy connects to
# the proxy's address, which each fill of PROXY_URL sets to a port of the stub.
real_curl=$(command -v curl) || fail "curl is missing"
rules() { printf -- '--connect-to %s\n' "$@" "::127.0.0.1:$port" > "$tmp/curl-rules"; }
# Both feature hosts lead to the stand-in of the feature listener, so a redirect's probe over IPv4 reaches the router.
standing_rules() { rules "api.anthropic.com:443:127.0.0.1:$direct" "mcp-proxy.anthropic.com:443:127.0.0.1:$direct"; }
standing_rules
cat > "$tmp/bin/curl" <<SH
#!/bin/sh
set -f
rules=\$(cat '$tmp/curl-rules')
case "\$rules" in
  *'--connect-to ::127.0.0.1:'*) ;;
  *) echo "curl stand-in: no rule of $tmp/curl-rules keeps each connection on loopback" >&2; exit 120 ;;
esac
if [ "\$1" = -q ]; then
  shift
  exec '$real_curl' -q \$rules "\$@"
fi
exec '$real_curl' \$rules "\$@"
SH
chmod +x "$tmp/bin/curl"

# A start under PRIVATE_HOSTS=yes calls tools that need root and binds a file over the machine's hosts file. The
# stand-in of sudo resets the environment to PATH and the caller's ids, as the tool resets it to a small set of its own,
# and joins 022 to the umask, as the tool does by default. It keeps PATH so the stand-ins stay found, and the root
# part's own env -i then empties the environment. unshare and setpriv run what follows their options, and mount copies
# the file it is asked to bind, or fails as a failed mount does where a case asks it to. Each records its call, and sudo
# its words. The case of the private file's lines needs the host's own unshare and mount, so their paths are read ahead
# of the stand-ins.
host_unshare=$(command -v unshare) || host_unshare=
host_mount=$(command -v mount) || host_mount=
cat > "$tmp/bin/sudo" <<SH
#!/bin/sh
echo sudo >> '$tmp/tools.called'
for word in "\$@"; do printf '[%s]\n' "\$word"; done > '$tmp/sudo-words'
umask "\$(printf '%o' "\$(( \$(umask) | 022 ))")"
exec env -i PATH="\$PATH" SUDO_UID='$(id -u)' SUDO_GID='$(id -g)' "\$@"
SH
for tool in setpriv unshare; do
  cat > "$tmp/bin/$tool" <<SH
#!/bin/sh
echo $tool >> '$tmp/tools.called'
while [ "\$1" != -- ]; do shift; done
shift
exec "\$@"
SH
done
cat > "$tmp/bin/mount" <<SH
#!/bin/sh
echo mount >> '$tmp/tools.called'
[ ! -e '$tmp/mount-fails' ] || exit 32
cp "\$2" '$tmp/hosts.bound'
SH
chmod +x "$tmp/bin/mount" "$tmp/bin/setpriv" "$tmp/bin/sudo" "$tmp/bin/unshare"
# Where the host's sudo asks for no password, a launch that found it in place of the stand-in would run the script's
# root part as root.
for tool in mount setpriv sudo unshare; do
  [[ "$(env -i PATH="$tmp/bin:$PATH" sh -c 'command -v "$1"' sh "$tool")" == "$tmp/bin/$tool" ]] \
    || fail "the PATH a launch hands the script does not find the stand-in of $tool first"
done

cat > "$tmp/bin/claude" <<SH
#!/bin/sh
env > '$tmp/client-env'
umask > '$tmp/client-umask'
for word in "\$@"; do printf '[%s]\n' "\$word"; done > '$tmp/client-words'
if [ "\${LAUNCH_MARK+x}" = x ]; then
  echo "the launch script started the client with LAUNCH_MARK=\$LAUNCH_MARK" >&2
else
  echo "the launch script started the client" >&2
fi
exit 1
SH
chmod +x "$tmp/bin/claude"

# A LAUNCH_MARK or a handed of the caller joins the script's environment, so a case can follow a variable of the calling
# shell into the script.
launch() {
  status=0
  out=$(env -i HOME="$tmp/home" PATH="$tmp/bin:$PATH" ${LAUNCH_MARK+"LAUNCH_MARK=$LAUNCH_MARK"} \
    ${handed+"handed=$handed"} sh "$tmp/launch.sh" "$@" 2>&1) || status=$?
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

# The fetch may run over plain HTTP, so the compare with the operator's fingerprint is the one check the CA's trust
# rests on. The fetched CA is the sound one, so a compare that passed it would go on to the client's start.
fill PROXY_URL "http://127.0.0.1:$good"
fill PHOEBIS_FEATURE_CA_SHA256 0000000000000000000000000000000000000000000000000000000000000000
launch
[[ "$out" == "phoebis: the fetched CA does not have the operator's fingerprint. Get the fingerprint from the router's operator" ]] \
  || fail "a CA of another fingerprint passed the compare (exit $status): $out"

# A client started with an empty token can fall back to a login of its own. A key check behind the fetch prints the same
# stop, so the download's record tells the two apart.
fill PHOEBIS_FEATURE_CA_SHA256 "$fingerprint"
fill ROUTER_KEY ''
: > "$tmp/downloads"
launch
[[ "$out" == "phoebis: ROUTER_KEY holds no key. Set it to the key the router's operator handed over" ]] \
  || fail "an empty ROUTER_KEY did not stop the start (exit $status): $out"
[[ ! -s "$tmp/downloads" ]] || fail "an empty ROUTER_KEY stopped the start behind the CA fetch: $(cat "$tmp/downloads")"

# The client reads no .curlrc, so a probe that followed the user's file would prove a road the client never takes, and
# the client would carry the router key along its own. Here the file leads both names to the sound stand-in of the
# feature listener, while the rules lead to the first listener alone, which speaks no TLS.
cat > "$tmp/home/.curlrc" <<EOF
connect-to = "api.anthropic.com:443:127.0.0.1:$direct"
connect-to = "mcp-proxy.anthropic.com:443:127.0.0.1:$direct"
EOF
rules
fill ROUTER_KEY test-key
fill SETUP redirect
launch
[[ "$out" == "phoebis: api.anthropic.com leads to another host than the router's feature listener over ipv4" ]] \
  || fail "a .curlrc in the user's home led a probe of the redirect (exit $status): $out"
rm "$tmp/home/.curlrc"
standing_rules
fill SETUP proxy

# Under PRIVATE_HOSTS=yes the script runs a second time below sudo, which resets the environment to a small set, and the
# root part's env -i empties it, so the client gets a variable of the calling shell only where the first run hands it
# over, and none of the variables sudo sets.
fill SETUP redirect
fill PRIVATE_HOSTS yes
rm -f "$tmp/tools.called" "$tmp/client-env"
LAUNCH_MARK=kept launch
[[ "$out" == "the launch script started the client with LAUNCH_MARK=kept" ]] \
  || fail "a variable of the calling shell did not reach the client under PRIVATE_HOSTS=yes (exit $status): $out"
grep -qx sudo "$tmp/tools.called" || fail "a start under PRIVATE_HOSTS=yes did not call sudo"
[[ -f "$tmp/client-env" ]] && ! grep -q '^SUDO_UID=' "$tmp/client-env" \
  || fail "the client got sudo's SUDO_UID under PRIVATE_HOSTS=yes: $(grep '^SUDO_' "$tmp/client-env" 2>&1)"

# A bind that failed would leave the second run under the machine's hosts file, so the root part stops there, and the
# first run names the private file and starts no client.
fill SETUP redirect
fill PRIVATE_HOSTS yes
touch "$tmp/mount-fails"
launch
rm "$tmp/mount-fails"
[[ "$out" == "phoebis: the private hosts file could not be put in place (exit 32)" ]] \
  || fail "a bind that failed did not stop the start ahead of the second run (exit $status): $out"

# A proxy setup reads no hosts file of its own, so a script that read PRIVATE_HOSTS there would ask its user for root to
# bind a file that nothing reads.
fill SETUP proxy
fill PROXY_URL "http://127.0.0.1:$good"
fill PRIVATE_HOSTS yes
rm -f "$tmp/tools.called"
launch
[[ ! -e "$tmp/tools.called" ]] || fail "PRIVATE_HOSTS=yes under SETUP=proxy called $(tr '\n' ' ' < "$tmp/tools.called")"
[[ "$out" == "the launch script started the client" ]] \
  || fail "PRIVATE_HOSTS=yes under SETUP=proxy did not reach the client's start (exit $status): $out"

# A redirect that started the client while one family reached the router would carry the router key to the other
# family's peer. localhost at the first listener's port leads IPv4 to a peer that speaks no TLS and IPv6 to the router.
if [[ "$v6" == yes ]]; then
  fill SETUP redirect
  fill PRIVATE_HOSTS no
  rules "api.anthropic.com:443:localhost:$port" "mcp-proxy.anthropic.com:443:localhost:$port"
  launch
  [[ "$out" == "phoebis: api.anthropic.com leads to another host than the router's feature listener over ipv4" ]] \
    || fail "a family that reaches another peer than the router did not stop the start (exit $status): $out"
  standing_rules
else
  echo "launch-script: no listener on ::1, the case of a family that reaches another peer did not run"
fi

# A private file that kept the machine's address for a feature host would get live updates refused on the road it exists
# for, and a filter that dropped a line whole would lose the router's own name. The script reads the machine's hosts
# file by its path, so the case binds one of its own there, inside a namespace the host grants.
if [[ -n "$host_unshare" && -n "$host_mount" ]] && "$host_unshare" -rm true 2>/dev/null; then
  cat > "$tmp/machine-hosts" <<'EOF'
127.0.0.1 localhost
192.0.2.10 router.example API.Anthropic.com # api.anthropic.com
192.0.2.10 mcp-proxy.anthropic.com
EOF
  cat > "$tmp/hosts.expected" <<'EOF'
127.0.0.1 localhost
192.0.2.10 router.example # api.anthropic.com
127.0.0.1 api.anthropic.com mcp-proxy.anthropic.com
::1 api.anthropic.com mcp-proxy.anthropic.com
EOF
  fill SETUP redirect
  fill PRIVATE_HOSTS yes
  rm -f "$tmp/hosts.bound"
  status=0
  out=$("$host_unshare" -rm sh -c '"$1" --bind "$2" /etc/hosts && shift 2 && exec "$@"' sh "$host_mount" \
    "$tmp/machine-hosts" env -i HOME="$tmp/home" PATH="$tmp/bin:$PATH" sh "$tmp/launch.sh" 2>&1) || status=$?
  [[ -f "$tmp/hosts.bound" ]] || fail "a start under PRIVATE_HOSTS=yes bound no hosts file (exit $status): $out"
  [[ "$(tr -s ' ' < "$tmp/hosts.bound")" == "$(cat "$tmp/hosts.expected")" ]] \
    || fail "the private hosts file kept a feature name or lost another line: $(cat "$tmp/hosts.bound")"
else
  echo "launch-script: no user namespace on this host, the case of the private file's lines did not run"
fi

# No second run comes outside the private road, so a script that took the marker word from its caller there would read
# and remove each file the next argument named.
fill SETUP proxy
fill PROXY_URL "http://127.0.0.1:$good"
fill PRIVATE_HOSTS no
echo 'export LAUNCH_MARK=planted' > "$tmp/planted"
launch phoebis-private-hosts "$tmp/planted"
[[ "$out" == "the launch script started the client" ]] \
  || fail "a caller's marker word made the script read a file under SETUP=proxy (exit $status): $out"
[[ -e "$tmp/planted" ]] || fail "a caller's marker word made the script remove a file under SETUP=proxy"

# sudo writes each command line it runs to the system's log, so the caller's arguments cross to the second run in the
# saved file, and each has to reach the client as typed: an option, two blanks in a row with a quote, an empty one and a
# line break.
cat > "$tmp/words.expected" <<'EOF'
[--verbose]
[say  it's]
[]
[line one
line two]
EOF
fill SETUP redirect
fill PRIVATE_HOSTS yes
rm -f "$tmp/client-words" "$tmp/sudo-words"
launch --verbose "say  it's" '' $'line one\nline two'
[[ "$(cat "$tmp/client-words" 2>/dev/null)" == "$(cat "$tmp/words.expected")" ]] \
  || fail "the caller's arguments did not reach the client as typed under PRIVATE_HOSTS=yes (exit $status): $out"
[[ -f "$tmp/sudo-words" ]] && ! grep -qxF -e '[--verbose]' -e "[say  it's]" -e '[]' -e '[line one' "$tmp/sudo-words" \
  || fail "a caller's argument rode sudo's command line: $(cat "$tmp/sudo-words" 2>&1)"

# sudo joins its own umask to the caller's, so a second run that took its umask for the shell's would start the client
# under sudo's, and each file of the session would be made under it.
fill SETUP redirect
fill PRIVATE_HOSTS yes
rm -f "$tmp/client-umask"
harness_umask=$(umask)
umask 0002
launch
umask "$harness_umask"
[[ "$(cat "$tmp/client-umask" 2>/dev/null)" == 0002 ]] \
  || fail "the client did not start under the caller's umask 0002 (exit $status): $(cat "$tmp/client-umask" 2>&1) $out"

# The second run reads the first run's exported variables, so a block that held the saved file's path in a variable
# would let a variable of that name in the calling shell pick the file that the second run removes.
fill SETUP redirect
fill PRIVATE_HOSTS yes
echo 'a file of the caller' > "$tmp/callers-file"
handed="$tmp/callers-file" launch
[[ -e "$tmp/callers-file" ]] || fail "a variable of the calling shell made the second run remove the file it names: $out"
[[ "$out" == "the launch script started the client" ]] \
  || fail "a start under the caller's handed did not end at the client's start alone (exit $status): $out"

# A family over which nothing answered leaves its hosts line unproved, and a road over it that comes up inside the
# session would carry the router key to the upstream, so a start that said nothing would read as one that proved both.
# Under the standing rules each probe over IPv4 reaches the router and each probe over IPv6 reaches nothing.
fill SETUP redirect
fill PRIVATE_HOSTS no
launch
grep -qx 'the launch script started the client' <<< "$out" \
  || fail "a start with a family that nothing answered over did not reach the client's start (exit $status): $out"
[[ "$(grep -c '^phoebis: .*ipv6' <<< "$out")" == 1 && "$out" != *ipv4* ]] \
  || fail "a start that proved ipv4 alone did not name ipv6 in one line and ipv4 nowhere (exit $status): $out"
if [[ "$v6" == yes ]]; then
  # The families turned, so an arm or a notice that knows one family's word alone fails one of the two sides. Rules
  # toward an IPv6 literal give each probe over IPv4 nothing to connect to and lead each probe over IPv6 to the router.
  rules "api.anthropic.com:443:[::1]:$direct" "mcp-proxy.anthropic.com:443:[::1]:$direct"
  launch
  grep -qx 'the launch script started the client' <<< "$out" \
    || fail "a start with a family that nothing answered over did not reach the client's start (exit $status): $out"
  [[ "$(grep -c '^phoebis: .*ipv4' <<< "$out")" == 1 && "$out" != *ipv6* ]] \
    || fail "a start that proved ipv6 alone did not name ipv4 in one line and ipv6 nowhere (exit $status): $out"
  # No other case starts the client with both families at the router, so a start that stopped or named a family there
  # would go unseen. localhost at the fifth listener's port leads each family to the router.
  rules "api.anthropic.com:443:localhost:$direct" "mcp-proxy.anthropic.com:443:localhost:$direct"
  launch
  [[ "$out" == "the launch script started the client" ]] \
    || fail "a start that proved both families did not end at the client's start alone (exit $status): $out"
  standing_rules
else
  echo "launch-script: no listener on ::1, the sides with the families turned and with both proved did not run"
fi

echo "launch-script: ok"
