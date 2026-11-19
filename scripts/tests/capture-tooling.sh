#!/usr/bin/env bash
# Each case pins a rule a real capture leans on, checked here without the client: the leaf a receiver presents
# chains to the run's own CA and names the control hosts, the CA carries the key usage a strict verifier asks for,
# mkcerts.sh refuses to start on a host without openssl, hello_lib.py's compare tells the two pinned hellos apart, finds
# one equal to itself and names the one extension a changed copy of it differs in, and control_receiver.py answers the
# control calls with the committed canned bodies, records each call, keeps serving after a client that drops its tunnel
# before the handshake, and records a chunked request as an error. The resign example's --write mode leaves a body
# stamped with the digits the signer computes over its own bytes. drive_login.py logs stub_client.py in by either
# login method with the authorize URL's state and reads /status and /usage, and gives up with its screen saved when the
# client stops answering or refuses the code. The stub's https calls reach the receiver alone, NO_PROXY or not, and the
# stub refuses a proxy off loopback. capture-control.sh seeds each login's config directory past the client's
# first-start screens, records the stub's login and refresh phases in both of its variants, with the refresh run
# starting from the credentials the login wrote and its inference post reaching the inference receiver past the proxy,
# moves on past a login the driver gives up on, stops in a phase whose control receiver cannot bind its port, and
# refuses a real token. lib.sh's runcli takes KEEP_CONFIG and ALLOW_NONESSENTIAL_TRAFFIC from a call's prefix and never
# from the caller's environment, and its start_receiver runs the receiver isolated. capture-feature-hellos.sh stops
# ahead of its hosts file's bind where no namespace of its own stands. Every case runs against a throwaway tree, never
# /tmp/cc-capture or ~/.claude, and reaches no host off loopback.
set -euo pipefail

here=$(cd "$(dirname "$0")/../.." && pwd)
capture="$here/scripts/capture"
tmp=$(mktemp -d)
receiver_pid=
# A case that fails exits with its receiver still listening, so the trap stops it before removing the tree the
# receiver writes into.
stop_receiver() {
  [[ -n "$receiver_pid" ]] || return 0
  kill "$receiver_pid" 2>/dev/null || true
  wait "$receiver_pid" 2>/dev/null || true
  receiver_pid=
}
trap 'stop_receiver; rm -rf "$tmp"' EXIT
fail() { echo "FAIL $1" >&2; exit 1; }

# --- mkcerts.sh ---

# The client trusts the CA through NODE_EXTRA_CA_CERTS and checks the leaf's names against the host it dialed, so
# a leaf that does not chain or misses one of the three hosts would fail the control capture's handshake.
out=$("$capture/mkcerts.sh" "$tmp/run" 2>&1) || fail "mkcerts.sh did not make the certificates: $out"
for file in ca.pem ca.key leaf.pem leaf.key; do
  [[ -s "$tmp/run/certs/$file" ]] || fail "mkcerts.sh wrote no certs/$file"
done
out=$(openssl verify -CAfile "$tmp/run/certs/ca.pem" "$tmp/run/certs/leaf.pem" 2>&1) \
  || fail "the leaf does not verify against the run's CA: $out"
echo "$out" | grep -q 'leaf.pem: OK$' || fail "openssl verify did not print leaf.pem: OK: $out"
sans=$(openssl x509 -in "$tmp/run/certs/leaf.pem" -noout -ext subjectAltName 2>&1) \
  || fail "the leaf carries no subjectAltName: $sans"
names=$(echo "$sans" | tr -s ', ' '\n')
for host in api.anthropic.com platform.claude.com claude.com; do
  echo "$names" | grep -qxF "DNS:$host" || fail "the leaf's subjectAltName lacks DNS:$host: $sans"
done
# Python 3.13's default TLS context verifies under strict rules that refuse a CA without keyUsage, so there the stub
# client's handshake with the receiver fails on a CA that curl still accepts. This check pins the extension on an
# older Python as well.
usage=$(openssl x509 -in "$tmp/run/certs/ca.pem" -noout -ext keyUsage 2>&1) || fail "the CA's keyUsage: $usage"
echo "$usage" | grep -qF 'Certificate Sign' || fail "the CA's keyUsage does not list Certificate Sign: $usage"

# On a host without openssl the script refuses before it writes anything, so a run never starts with a certs/
# directory half made. bash is named by its own path because the PATH under test holds nothing at all.
mkdir -p "$tmp/empty-bin"
status=0
out=$(PATH="$tmp/empty-bin" "$BASH" "$capture/mkcerts.sh" "$tmp/no-openssl" 2>&1) || status=$?
[[ "$status" -eq 1 ]] || fail "mkcerts.sh without openssl exited $status, want 1: $out"
[[ ! -e "$tmp/no-openssl" ]] || fail "mkcerts.sh without openssl still wrote $tmp/no-openssl"

# --- hello_lib.py's compare ---

# The feature hellos' capture stops on this compare's status, so one that masked more than the fields a connection or
# a host draws anew would answer "equal" for any two hellos and never stop. The two pinned hellos first differ in their
# cipher lists, ahead of each extension.
fixtures="$here/crates/profile/fixtures"
status=0
out=$(python3 -I "$capture/hello_lib.py" "$fixtures/hello_inference.hex" "$fixtures/hello_control.hex" 2>&1) \
  || status=$?
[[ "$status" -eq 3 ]] || fail "the compare of the two pinned hellos exited $status, want 3: $out"
[[ "$(tail -n 1 <<<"$out")" == "differs at the cipher list" ]] \
  || fail "the compare of the two pinned hellos printed: $out"
status=0
out=$(python3 -I "$capture/hello_lib.py" "$fixtures/hello_inference.hex" "$fixtures/hello_inference.hex" 2>&1) \
  || status=$?
[[ "$status" -eq 0 ]] || fail "the compare of a pinned hello with itself exited $status, want 0: $out"

# The pinned pair differs ahead of the extensions, so this pair differs in one alone: the inference hello's ALPN
# extension offering http/1.0 in place of http/1.1, its seventh extension.
alpn_11=0010000b000908687474702f312e31
alpn_10=0010000b000908687474702f312e30
inference=$(<"$fixtures/hello_inference.hex")
[[ "$inference" == *"$alpn_11"* ]] || fail "hello_inference.hex holds no ALPN extension offering http/1.1 alone"
printf '%s\n' "${inference/"$alpn_11"/"$alpn_10"}" >"$tmp/hello_alpn.hex"
status=0
out=$(python3 -I "$capture/hello_lib.py" "$fixtures/hello_inference.hex" "$tmp/hello_alpn.hex" 2>&1) || status=$?
[[ "$status" -eq 3 ]] || fail "the compare of hellos that differ in one extension exited $status, want 3: $out"
[[ "$(tail -n 1 <<<"$out")" == "differs at extension 16 (alpn) at place 7" ]] \
  || fail "the compare of hellos that differ in one extension printed: $out"

# --- control_receiver.py ---

# The control capture's client reaches the control hosts only through this receiver, so the receiver has to answer
# with exactly the committed bodies and record every call. Each call below names the real host inside the
# receiver's tunnel. -q skips a ~/.curlrc and --noproxy '' overrides a NO_PROXY in the environment, so no call can
# bypass the receiver and reach that host.
responses="$here/crates/profile/fixtures/responses"
recording="$tmp/control.json"
port=$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])') \
  || fail "found no free loopback port for control_receiver.py"
python3 -I "$capture/control_receiver.py" "$port" "$recording" 60 "$tmp/run/certs" "$responses" &
receiver_pid=$!
control_curl() {
  curl -q -s --max-time 10 --noproxy '' --proxy "http://127.0.0.1:$port" --cacert "$tmp/run/certs/ca.pem" "$@"
}
# One line per entry in seq order, so a whole recording compares as one string. An entry whose keys differ from
# the request shape or the error shape, or a request whose Host header does not name its tunnel's host, prints a
# line no expected string holds.
summarize() {
  python3 -I - "$1" <<'PY'
import json, sys

for entry in sorted(json.load(open(sys.argv[1])), key=lambda e: e["seq"]):
    keys = sorted(entry)
    if keys == ["error", "seq", "target"] and entry["error"]:
        print(entry["seq"], entry["target"], "error")
    elif keys == ["answered", "body", "headers", "request_line", "seq", "target"]:
        if "Host: " + entry["target"].rsplit(":", 1)[0] not in entry["headers"]:
            print(entry["seq"], "no Host header for", entry["target"], entry["headers"])
        line = f'{entry["seq"]} {entry["target"]} {entry["request_line"]} -> {entry["answered"]} {entry["body"]}'
        print(line.rstrip())
    else:
        print(entry["seq"], "unexpected entry", entry)
PY
}

# The first call also waits out the receiver's start: curl retries a refused connect, and the receiver accepts
# nothing before it listens, so the wait leaves no trace in the recording.
code=$(control_curl --retry 5 --retry-delay 1 --retry-connrefused -o "$tmp/profile.out" \
  -w '%{http_code} %{content_type} %header{connection}' https://api.anthropic.com/api/oauth/profile) \
  || fail "the profile call through control_receiver.py failed: $code"
[[ "$code" == "200 application/json close" ]] || fail "the profile call was answered $code"
cmp -s "$tmp/profile.out" "$responses/profile.json" || fail "the profile call's body is not responses/profile.json"
code=$(control_curl -o "$tmp/token.out" -w '%{http_code}' -H 'Content-Type: application/json' \
  --data-raw '{"grant_type":"refresh_token"}' https://platform.claude.com/v1/oauth/token) \
  || fail "the token call through control_receiver.py failed: $code"
[[ "$code" == "200" ]] || fail "the token call was answered $code"
cmp -s "$tmp/token.out" "$responses/token.json" || fail "the token call's body is not responses/token.json"
code=$(control_curl -o "$tmp/other.out" -w '%{http_code}' https://api.anthropic.com/api/oauth/other) \
  || fail "the call to a path with no canned body failed: $code"
[[ "$code" == "404" ]] || fail "a path with no canned body was answered $code, want 404"
[[ "$(<"$tmp/other.out")" == "{}" ]] || fail "a path with no canned body was answered $(<"$tmp/other.out"), want {}"

# Each entry reaches the disk before its answer goes out, so a capture killed outright keeps every call the client
# saw answered.
three='0 api.anthropic.com:443 GET /api/oauth/profile HTTP/1.1 -> 200
1 platform.claude.com:443 POST /v1/oauth/token HTTP/1.1 -> 200 {"grant_type":"refresh_token"}
2 api.anthropic.com:443 GET /api/oauth/other HTTP/1.1 -> 404'
got=$(summarize "$recording") || fail "the recording does not parse: $got"
[[ "$got" == "$three" ]] || fail "the recording after three calls reads: $got"

# A client that drops its tunnel between the CONNECT answer and the TLS handshake leaves one error entry, and the
# receiver goes on to answer the next call.
python3 -I - "$port" <<'PY' || fail "control_receiver.py did not answer CONNECT x:443 with a 200"
import socket, sys

with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=10) as s:
    s.sendall(b"CONNECT x:443\r\n\r\n")
    answer = b""
    while b"\r\n\r\n" not in answer:
        chunk = s.recv(4096)
        if not chunk:
            break
        answer += chunk
sys.exit(answer != b"HTTP/1.1 200 Connection established\r\n\r\n")
PY
code=$(control_curl -o "$tmp/profile-again.out" -w '%{http_code}' https://api.anthropic.com/api/oauth/profile) \
  || fail "the profile call after a dropped tunnel failed: $code"
[[ "$code" == "200" ]] || fail "the profile call after a dropped tunnel was answered $code"
cmp -s "$tmp/profile-again.out" "$responses/profile.json" \
  || fail "the profile call after a dropped tunnel is not responses/profile.json"

# The receiver frames a body by Content-Length alone, so it would record a chunked grant cut short. It records an
# error entry naming the chunked body in place of the request.
control_curl -o /dev/null -H 'Content-Type: application/json' -H 'Transfer-Encoding: chunked' \
  --data-raw '{"grant_type":"refresh_token"}' https://platform.claude.com/v1/oauth/token || true

# The recording is removed first, so only the SIGTERM handler's own dump can bring it back.
rm "$recording"
kill -TERM "$receiver_pid"
status=0
wait "$receiver_pid" || status=$?
receiver_pid=
[[ "$status" -eq 0 ]] || fail "control_receiver.py exited $status on SIGTERM, want 0"
[[ -s "$recording" ]] || fail "control_receiver.py wrote no recording on SIGTERM"
six="$three
3 x:443 error
4 api.anthropic.com:443 GET /api/oauth/profile HTTP/1.1 -> 200
5 platform.claude.com:443 error"
got=$(summarize "$recording") || fail "the recording written on SIGTERM does not parse: $got"
[[ "$got" == "$six" ]] || fail "the recording written on SIGTERM reads: $got"
python3 -I - "$recording" <<'PY' || fail "control_receiver.py's error entry for the chunked call: $(<"$recording")"
import json, sys

entry = next(e for e in json.load(open(sys.argv[1])) if e["seq"] == 5)
sys.exit("chunked" not in entry.get("error", ""))
PY

# --- resign --write ---

# scripts/build-fixtures.py redacts a fresh body's ids before it writes the fixture, so the digits the client signed
# no longer match the bytes, and the write mode stores the digits the signer computes over what the file now holds.
# The compare run that follows exits 0 only when the stored digits are the signer's.
resign() {
  CARGO_INCREMENTAL=0 cargo run -q --manifest-path "$here/Cargo.toml" -p phoebis-cch --example resign -- "$@"
}
placeholder='{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1; cch=00000;"}],"messages":[]}'
body="$tmp/body.json"
printf '%s' "$placeholder" >"$body"
out=$(resign --write "$body" 2>"$tmp/resign.err") || fail "resign --write failed: $out $(<"$tmp/resign.err")"
stamp=$(grep -oE 'cch=[0-9a-f]{5};' "$body") || fail "resign --write left no cch= stamp: $(<"$body")"
[[ "$stamp" != "cch=00000;" ]] || fail "resign --write left the placeholder cch=00000 in place"
[[ "$(<"$body")" == "${placeholder/cch=00000;/$stamp}" ]] \
  || fail "resign --write changed more than the cch= digits: $(<"$body")"
out=$(resign "$body" 2>&1) || fail "the compare run after resign --write reports drift: $out"
out=$(resign --write "$body" 2>"$tmp/resign.err") || fail "a second resign --write failed: $out $(<"$tmp/resign.err")"

# A body without a stamp is refused as the compare run refuses it, and the file is left as it was.
bare='{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1;"}],"messages":[]}'
printf '%s' "$bare" >"$tmp/bare.json"
status=0
out=$(resign --write "$tmp/bare.json" 2>"$tmp/resign.err") || status=$?
[[ "$status" -eq 1 ]] || fail "resign --write on a body without a stamp exited $status, want 1: $(<"$tmp/resign.err")"
grep -qxF "resign: $tmp/bare.json: the billing block carries no cch= stamp" "$tmp/resign.err" \
  || fail "resign --write on a body without a stamp printed: $(<"$tmp/resign.err")"
[[ "$(<"$tmp/bare.json")" == "$bare" ]] || fail "resign --write rewrote a body without a stamp: $(<"$tmp/bare.json")"

# --- drive_login.py and stub_client.py ---

# The control capture logs the client in through drive_login.py with no human at the terminal, so the driver has to
# get through the login menus, paste a code carrying the authorize URL's state, confirm the success screen, and open
# and close /status and /usage. stub_client.py plays the client against a receiver of its own, so the recording holds
# the stub's calls alone. Each run starts from an empty environment holding the variables a capture sets, so no proxy or
# config directory of the host's reaches the stub, and ANTHROPIC_BASE_URL names the receiver too, so a stray inference
# post would leave an entry. The receiver gets no readiness probe, since a probe would leave an entry of its own: the
# driver waits out three quiet screens before the stub's first call, seconds more than the receiver needs to listen.
driver="$capture/drive_login.py"
stub="$here/scripts/tests/stub_client.py"
login_recording="$tmp/login.json"
login_port=$(python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])') \
  || fail "found no free loopback port for the login receiver"
python3 -I "$capture/control_receiver.py" "$login_port" "$login_recording" 120 "$tmp/run/certs" "$responses" &
receiver_pid=$!
mkdir -p "$tmp/cwd"
stub_env=(env -i PATH="$PATH" HTTPS_PROXY="http://127.0.0.1:$login_port"
  NODE_EXTRA_CA_CERTS="$tmp/run/certs/ca.pem" ANTHROPIC_BASE_URL="http://127.0.0.1:$login_port")

# The seq the receiver gives its next connection, so a sub-case reads the entries its own run left.
next_seq() {
  python3 -I - "$login_recording" <<'PY'
import json, os, sys

entries = json.load(open(sys.argv[1])) if os.path.exists(sys.argv[1]) else []
print(max((e["seq"] for e in entries), default=-1) + 1)
PY
}

# A login leaves, from seq $1 on, the stub's code grant with the driver's dummy code and the state of the URL the
# driver wrote to $2, then the profile, roles and usage reads and no other call, each with the stub's header. The
# credentials file in $3 holds the canned token, as the client's own would after a login.
check_login() {
  python3 -I - "$login_recording" "$@" <<'PY'
import json, sys, time, urllib.parse

recording, first, url_file, credentials = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
url = open(url_file).read().strip()
state = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).get("state", [""])[0]
entries = sorted((e for e in json.load(open(recording)) if e["seq"] >= first), key=lambda e: e["seq"])
calls = [(e.get("target"), e.get("request_line")) for e in entries]
want = [
    ("platform.claude.com:443", "POST /v1/oauth/token HTTP/1.1"),
    ("api.anthropic.com:443", "GET /api/oauth/profile HTTP/1.1"),
    ("api.anthropic.com:443", "GET /api/oauth/claude_cli/roles HTTP/1.1"),
    ("api.anthropic.com:443", "GET /api/oauth/usage HTTP/1.1"),
]
if calls != want:
    sys.exit(f"the recording holds {calls}")
grant = json.loads(entries[0]["body"])
if grant.get("grant_type") != "authorization_code" or grant.get("code") != "DUMMYCODE":
    sys.exit(f"the code grant reads {grant}")
if not state or grant.get("state") != state:
    sys.exit(f"the code grant's state {grant.get('state')!r} is not the URL's {state!r}")
if not grant.get("code_verifier"):
    sys.exit(f"the code grant carries no code_verifier: {grant}")
for entry in entries:
    if "x-stub-nonessential: unset" not in entry["headers"]:
        sys.exit(f"{entry['request_line']} lacks x-stub-nonessential: unset: {entry['headers']}")
oauth = json.load(open(credentials)).get("claudeAiOauth", {})
tokens = (oauth.get("accessToken"), oauth.get("refreshToken"), oauth.get("scopes"))
if tokens != ("sk-ant-oat01-PLACEHOLDER", "sk-ant-ort01-PLACEHOLDER", ["user:profile"]):
    sys.exit(f"the stub's credentials read {oauth}")
if not isinstance(oauth.get("expiresAt"), int) or oauth["expiresAt"] <= time.time() * 1000:
    sys.exit(f"the stub's credentials expire at {oauth.get('expiresAt')!r}, want a time ahead")
PY
}

first=$(next_seq) || fail "the login recording does not parse: $first"
status=0
out=$("${stub_env[@]}" CLAUDE_CONFIG_DIR="$tmp/cfg-claude" python3 -I "$driver" --binary "$stub" --cwd "$tmp/cwd" \
  --out "$tmp/login" --method claude 2>&1) || status=$?
[[ "$status" -eq 0 ]] || fail "drive_login.py against the stub exited $status, want 0: $out"
[[ -s "$tmp/login/login-claude-url.txt" ]] || fail "drive_login.py wrote no login-claude-url.txt: $out"
grep -qxE 'https://claude\.com/cai/oauth/authorize\?code=true&client_id=stub&state=[A-Za-z0-9_-]{43}' \
  "$tmp/login/login-claude-url.txt" || fail "login-claude-url.txt holds $(<"$tmp/login/login-claude-url.txt")"
got=$(check_login "$first" "$tmp/login/login-claude-url.txt" "$tmp/cfg-claude/.credentials.json" 2>&1) \
  || fail "after the login run, $got"

# A client that stops answering after its first prompt, as on a screen the driver does not know, ends the run with
# exit 2 once the timeout passes, and the screen read so far is on disk to show where it stopped. The driver exits 2
# for a client that exited as well, so the timeout's own message is what tells the two apart.
status=0
out=$("${stub_env[@]}" CLAUDE_CONFIG_DIR="$tmp/cfg-hang" STUB_HANG=1 python3 -I "$driver" --binary "$stub" \
  --cwd "$tmp/cwd" --out "$tmp/hang" --method claude --timeout 3 2>"$tmp/hang.err") || status=$?
out+=$(<"$tmp/hang.err")
[[ "$status" -eq 2 ]] || fail "drive_login.py against a client that stopped answering exited $status, want 2: $out"
grep -qF 'drive_login.py: no sign of ' "$tmp/hang.err" \
  || fail "drive_login.py against a client that stopped answering printed no timeout message: $out"
[[ -s "$tmp/hang/login-claude-screen.txt" ]] || fail "drive_login.py left no screen of the client that stopped: $out"

# A pasted code the client refuses leaves the login on an error screen that waits for a key. The same dummy code would
# be refused again, so the driver cancels the dialog and exits 2 with a message of its own, which tells the refusal
# from a timeout, and the stub writes no credentials. --timeout bounds a driver that waits on the error screen.
status=0
out=$("${stub_env[@]}" CLAUDE_CONFIG_DIR="$tmp/cfg-reject" STUB_REJECT_CODE=1 python3 -I "$driver" --binary "$stub" \
  --cwd "$tmp/cwd" --out "$tmp/reject" --method claude --timeout 10 2>&1) || status=$?
[[ "$status" -eq 2 ]] || fail "drive_login.py against a refused code exited $status, want 2: $out"
grep -qF 'drive_login.py: the client answered the pasted code with an error' <<<"$out" \
  || fail "drive_login.py against a refused code printed: $out"
grep -qF 'Invalid code' "$tmp/reject/login-claude-screen.txt" \
  || fail "the screen of the refused code holds: $(<"$tmp/reject/login-claude-screen.txt")"
[[ ! -e "$tmp/cfg-reject/.credentials.json" ]] || fail "the stub wrote credentials after a refused code: $out"

# The Console method is the login menu's second entry, which opens a second menu whose first entry signs in with a
# Console account, so the stub's grant follows a down arrow and two returns and its authorize URL is the Console's.
first=$(next_seq) || fail "the login recording does not parse: $first"
status=0
out=$("${stub_env[@]}" CLAUDE_CONFIG_DIR="$tmp/cfg-console" python3 -I "$driver" --binary "$stub" --cwd "$tmp/cwd" \
  --out "$tmp/login" --method console 2>&1) || status=$?
[[ "$status" -eq 0 ]] || fail "drive_login.py --method console against the stub exited $status, want 0: $out"
grep -qxE 'https://platform\.claude\.com/oauth/authorize\?code=true&client_id=stub&state=[A-Za-z0-9_-]{43}' \
  "$tmp/login/login-console-url.txt" || fail "login-console-url.txt holds $(<"$tmp/login/login-console-url.txt")"
got=$(check_login "$first" "$tmp/login/login-console-url.txt" "$tmp/cfg-console/.credentials.json" 2>&1) \
  || fail "after the --method console run, $got"

# urllib's proxy handler honors an inherited NO_PROXY=* and would dial the real host, so the same login with it set
# has to leave the same calls in the recording.
first=$(next_seq) || fail "the login recording does not parse: $first"
status=0
out=$("${stub_env[@]}" NO_PROXY='*' CLAUDE_CONFIG_DIR="$tmp/cfg-no-proxy" python3 -I "$driver" --binary "$stub" \
  --cwd "$tmp/cwd" --out "$tmp/no-proxy" --method claude 2>&1) || status=$?
[[ "$status" -eq 0 ]] || fail "drive_login.py against the stub with NO_PROXY=* exited $status, want 0: $out"
got=$(check_login "$first" "$tmp/no-proxy/login-claude-url.txt" "$tmp/cfg-no-proxy/.credentials.json" 2>&1) \
  || fail "after the login run with NO_PROXY=*, $got"

# A proxy off loopback is refused before a socket opens, so neither 203.0.113.9 nor the receiver behind
# ANTHROPIC_BASE_URL sees a connection, though the expired token would otherwise start a refresh grant.
mkdir -p "$tmp/cfg-expired"
expired='{"claudeAiOauth":{"accessToken":"sk-ant-oat01-PLACEHOLDER","refreshToken":"sk-ant-ort01-PLACEHOLDER",'
expired+='"expiresAt":0,"scopes":["user:profile"]}}'
printf '%s' "$expired" >"$tmp/cfg-expired/.credentials.json"
cp "$login_recording" "$tmp/login-before.json"
status=0
env -i PATH="$PATH" HTTPS_PROXY=http://203.0.113.9:3128 NODE_EXTRA_CA_CERTS="$tmp/run/certs/ca.pem" \
  CLAUDE_CONFIG_DIR="$tmp/cfg-expired" ANTHROPIC_BASE_URL="http://127.0.0.1:$login_port" \
  python3 -I "$stub" -p hi --model x >"$tmp/stub.out" 2>"$tmp/stub.err" || status=$?
[[ "$status" -eq 3 ]] || fail "the stub behind a proxy off loopback exited $status, want 3: $(<"$tmp/stub.err")"
[[ "$(<"$tmp/stub.err")" == "stub_client: refusing a proxy off loopback: http://203.0.113.9:3128" ]] \
  || fail "the stub behind a proxy off loopback printed: $(<"$tmp/stub.err")"
cmp -s "$login_recording" "$tmp/login-before.json" \
  || fail "the stub behind a proxy off loopback left an entry: $(<"$login_recording")"
stop_receiver

# --- capture-control.sh ---

# The control capture logs the stub in through the driver, then runs it in print mode on a copy of the login's config
# directory, once with runcli's environment and once without CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC. The phase files
# are what the builder's --check --control reads, so a phase file holding another phase's calls would grade the wrong
# request. PHOEBIS_CAPTURE_NETNS=1 skips the namespace, which a CI runner may not allow, and PHOEBIS_CAPTURE_STATE
# keeps the client's directories in the throwaway tree. The run takes lib.sh's own ports, and the host's credentials
# and CAPTURE_BASELINE stay out of it.
export PHOEBIS_CAPTURE_NETNS=1 PHOEBIS_CAPTURE_STATE="$tmp/state"
capture_control() {
  env -u ANTHROPIC_API_KEY -u CAPTURE_BASELINE -u CLAUDE_CODE_OAUTH_TOKEN "$here/scripts/capture-control.sh" "$@"
}

# Each phase file under $1 holds its phase's calls alone, in order, each carrying x-stub-nonessential: $2 and
# x-stub-home: $3, since the receivers record token grants verbatim and every client run gets a throwaway HOME in
# place of the operator's. A terminal UI may not draw its menus without TERM and a locale, so the login phases' client
# sees both, and BROWSER=true, so its login shows the paste form at once on any host. The refresh run keeps runcli's
# print-mode environment, the one the committed fixtures were captured under. The refresh grant carries the refresh
# token the stub stored at its login, which only a config directory that outlived runcli's reset still holds. The
# stub sends its inference post through the proxy, as the client does, unless NO_PROXY names the base URL's host, so
# the refresh run's turn reaches the inference receiver beside the control receiver only through the NO_PROXY the
# capture sets, and a post sent to the proxy instead leaves an error entry in refresh.json.
check_variant() {
  python3 -I - "$@" <<'PY'
import json, pathlib, sys

variant, nonessential, home = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
token, profile = "POST /v1/oauth/token HTTP/1.1", "GET /api/oauth/profile HTTP/1.1"
login = [token, profile, "GET /api/oauth/claude_cli/roles HTTP/1.1", "GET /api/oauth/usage HTTP/1.1"]
phases = {"login-claude": login, "login-console": login, "refresh": [token, profile]}
terminal = ["x-stub-browser: true", "x-stub-lang: C.UTF-8", "x-stub-term: xterm-256color"]
print_mode = ["x-stub-browser: unset", "x-stub-lang: unset", "x-stub-term: unset"]
# Both checked ahead of the phases, where the refresh phase's request lines would fail first on an error entry and
# name neither cause.
failures = []
errors = [e for e in json.load(open(variant / "refresh.json")) if "error" in e]
if errors:
    failures.append(f"refresh.json holds the error entries {errors}")
turns = [e["request_line"] for e in json.load(open(variant / "inference" / "clean_modes.json"))]
if turns != ["POST /v1/messages HTTP/1.1"]:
    failures.append(f"the inference receiver recorded {turns}")
if failures:
    sys.exit(", and ".join(failures))
for phase, want in phases.items():
    entries = sorted(json.load(open(variant / f"{phase}.json")), key=lambda e: e["seq"])
    if [e.get("request_line") for e in entries] != want:
        sys.exit(f"{phase}.json holds {entries}")
    headers = [f"x-stub-home: {home}", f"x-stub-nonessential: {nonessential}"]
    headers += print_mode if phase == "refresh" else terminal
    for entry in entries:
        for header in headers:
            if header not in entry["headers"]:
                sys.exit(f"{phase}.json's {entry['request_line']} lacks {header}: {entry}")
    if phase == "refresh":
        grant = json.loads(entries[0]["body"])
        if grant.get("grant_type") != "refresh_token" or grant.get("refresh_token") != "sk-ant-ort01-PLACEHOLDER":
            sys.exit(f"the refresh grant reads {grant}")
PY
}

# Each login phase's config directory under the state directory $1 holds a .claude.json with no account in it: the
# client's own smallest config that skips its onboarding wizard, plus the trust record for the working directory keyed
# by the resolved path as the client keys it. The client then starts at its prompt, and /login starts signed out.
check_seed() {
  python3 -I - "$1" <<'PY'
import json, os, sys

state = sys.argv[1]
want = {
    "hasCompletedOnboarding": True,
    "autoUpdates": False,
    "bypassPermissionsModeAccepted": False,
    "projects": {os.path.realpath(os.path.join(state, "cwd")): {"hasTrustDialogAccepted": True}},
}
for method in ("claude", "console"):
    path = os.path.join(state, f"cfg-login-{method}", ".claude.json")
    if not os.path.exists(path):
        sys.exit(f"cfg-login-{method} holds no .claude.json")
    with open(path) as f:
        config = json.load(f)
    if config != want:
        sys.exit(f"cfg-login-{method}/.claude.json reads {config}")
PY
}

out_dir=$(realpath -m "$tmp/out")
status=0
out=$(capture_control "$stub" "$tmp/out" 2>&1) || status=$?
[[ "$status" -eq 0 ]] || fail "capture-control.sh against the stub exited $status, want 0: $out"
[[ -s "$out_dir/certs/ca.pem" ]] || fail "capture-control.sh wrote no certs/ca.pem: $out"
# A capture directory may be committed, and a private key in it trips secret scanners, while the CA's key has no use
# once the run is over.
for key in ca.key leaf.key; do
  [[ ! -e "$out_dir/certs/$key" ]] || fail "capture-control.sh left certs/$key behind: $out"
done
for variant in control control-unflagged; do
  for phase in login-claude login-console refresh; do
    [[ -s "$out_dir/$variant/$phase.json" ]] || fail "capture-control.sh wrote no $variant/$phase.json: $out"
  done
  want="  python3 -I scripts/build-fixtures.py --check --control $out_dir/$variant <baseline-dir>"
  grep -qxF -- "$want" <<<"$out" || fail "capture-control.sh did not print the check of $variant/: $out"
done
got=$(check_variant "$out_dir/control" 1 "$PHOEBIS_CAPTURE_STATE/home" 2>&1) || fail "in control/, $got"
got=$(check_variant "$out_dir/control-unflagged" unset "$PHOEBIS_CAPTURE_STATE/home" 2>&1) \
  || fail "in control-unflagged/, $got"
got=$(check_seed "$PHOEBIS_CAPTURE_STATE" 2>&1) || fail "after capture-control.sh, $got"
# The capture stops both receivers before it exits, so the next capture finds lib.sh's ports free.
python3 -I - <<'PY' || fail "capture-control.sh left a receiver listening on port 48442 or 48444"
import socket

for port in (48442, 48444):
    with socket.socket() as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", port))
PY

# The receivers record every header verbatim, so a real token in the environment is refused before anything is
# written.
status=0
out=$(env -u ANTHROPIC_API_KEY -u CAPTURE_BASELINE CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-REAL \
  "$here/scripts/capture-control.sh" "$stub" "$tmp/refused" 2>&1) || status=$?
[[ "$status" -eq 1 ]] || fail "capture-control.sh with a real CLAUDE_CODE_OAUTH_TOKEN exited $status, want 1: $out"
[[ "$out" == "capture-control.sh: refusing to run with a real CLAUDE_CODE_OAUTH_TOKEN set" ]] \
  || fail "capture-control.sh with a real CLAUDE_CODE_OAUTH_TOKEN printed: $out"
[[ ! -e "$tmp/refused" ]] || fail "capture-control.sh with a real CLAUDE_CODE_OAUTH_TOKEN still wrote $tmp/refused"

# A login whose driver gives up, here on a client that prints a screen the driver does not know and exits, is logged
# with its screen file, and the capture goes on to the next phase. The refresh phase then finds no credentials to
# expire and is skipped with a note. The client exits once the driver has saved its screen, so the terminal never
# closes on a screen the driver has not read, and it exits rather than hangs, since a hang would cost the driver's
# 90-second timeout per variant. It finds its variant's screen file by the non-essential-traffic flag, which the
# control variant sets and control-unflagged leaves out. The output directory holds an earlier run's refresh recording
# and authorize URL, which this run neither makes nor keeps, so the builder cannot grade a stale phase file and no URL
# sits beside a screen it did not come from.
gave_up=$(realpath -m "$tmp/gave-up")
wrapper="$tmp/unknown-screen-client"
cat >"$wrapper" <<EOF
#!/usr/bin/env bash
if [[ "\$CLAUDE_CONFIG_DIR" == */cfg-login-claude ]]; then
  echo "an unknown screen"
  variant=control-unflagged
  [[ -n "\${CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:-}" ]] && variant=control
  until grep -qF "an unknown screen" "$gave_up/\$variant/login-claude-screen.txt" 2>/dev/null; do
    sleep 0.1
  done
  exit 0
fi
exec python3 -I "$stub" "\$@"
EOF
chmod +x "$wrapper"
for variant in control control-unflagged; do
  mkdir -p "$gave_up/$variant"
  echo '[]' >"$gave_up/$variant/refresh.json"
  echo 'https://claude.com/cai/oauth/authorize?state=stale' >"$gave_up/$variant/login-claude-url.txt"
done
status=0
out=$(capture_control "$wrapper" "$tmp/gave-up" 2>&1) || status=$?
[[ "$status" -eq 0 ]] || fail "capture-control.sh past a login the driver gave up on exited $status, want 0: $out"
for variant in control control-unflagged; do
  screen="$gave_up/$variant/login-claude-screen.txt"
  grep -qxF "phase login-claude: drive_login.py exited 2, screen in $screen" <<<"$out" \
    || fail "capture-control.sh did not log the screen of $variant/'s login-claude: $out"
  grep -qF "an unknown screen" "$screen" || fail "$variant/login-claude-screen.txt holds: $(<"$screen")"
  grep -qF '"request_line": "GET /api/oauth/usage HTTP/1.1"' "$gave_up/$variant/login-console.json" \
    || fail "capture-control.sh ran no login-console in $variant/ after the driver gave up: $out"
  [[ ! -e "$gave_up/$variant/refresh.json" ]] \
    || fail "capture-control.sh left a refresh.json in $variant/ from a run without credentials: $out"
  [[ ! -e "$gave_up/$variant/login-claude-url.txt" ]] \
    || fail "capture-control.sh kept a stale login-claude-url.txt in $variant/: $out"
done
skipped=$(grep -cxF "phase refresh skipped: login-claude left no claudeAiOauth.expiresAt in .credentials.json" \
  <<<"$out") || true
[[ "$skipped" -eq 2 ]] || fail "capture-control.sh printed the refresh phase's skip note $skipped times, want 2: $out"

# A control receiver that cannot bind its port, here held by a listener the test opens first, exits before it writes
# its recording, and the capture stops in that phase rather than logging the client in against nothing. The holder
# closes each connection it accepts, so a client sent to it fails its calls at once instead of waiting them out. The
# capture's exit trap still stops the inference receiver.
held=$(realpath -m "$tmp/held")
python3 -I - "$tmp/holder-ready" <<'PY' &
import pathlib, socket, sys

with socket.socket() as s:
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 48444))
    s.listen(8)
    pathlib.Path(sys.argv[1]).touch()
    while True:
        conn, _ = s.accept()
        conn.close()
PY
receiver_pid=$!
for ((step = 0; step < 100; step++)); do
  [[ -e "$tmp/holder-ready" ]] && break
  sleep 0.1
done
[[ -e "$tmp/holder-ready" ]] || fail "the listener meant to hold port 48444 did not start"
status=0
out=$(capture_control "$stub" "$tmp/held" 2>&1) || status=$?
stop_receiver
[[ "$status" -ne 0 ]] || fail "capture-control.sh with the control receiver's port held exited 0: $out"
grep -qxF "capture-control.sh: control receiver failed in phase login-claude" <<<"$out" \
  || fail "capture-control.sh with the control receiver's port held named no failed phase: $out"
grep -qxE "control receiver exited [1-9][0-9]*" <<<"$out" \
  || fail "capture-control.sh with the control receiver's port held printed no receiver status: $out"
for file in "$held"/control/*.json "$held"/control-unflagged/*.json; do
  [[ ! -e "$file" ]] || fail "capture-control.sh with the control receiver's port held wrote $file: $out"
done
python3 -I - <<'PY' || fail "capture-control.sh left the inference receiver listening on port 48442 after the failure"
import socket

with socket.socket() as s:
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 48442))
PY

# --- lib.sh ---

# runcli resets its config directory and sets the non-essential-traffic flag unless the call's own prefix says
# otherwise, so a KEEP_CONFIG or ALLOW_NONESSENTIAL_TRAFFIC exported in the operator's shell must not change a run of
# the baseline or count script. start_receiver runs its receiver under python3 -I, so a PYTHONPATH or user site of
# the operator's loads nothing into it. The harness sources lib.sh the way those scripts do, from beside a capture/
# directory of its own, its client prints the flag it was started with, and its receiver prints its isolated flag.
harness="$tmp/harness"
mkdir -p "$harness/capture"
ln -s "$capture/lib.sh" "$harness/capture/lib.sh"
cat >"$harness/capture/probe.py" <<'EOF'
import sys

with open(sys.argv[2], "w") as f:
    print(f"receiver isolated {sys.flags.isolated}", file=f)
EOF
cat >"$harness/lib-check.sh" <<'EOF'
source "$(dirname "$0")/capture/lib.sh"
for call in exported prefixed; do
  mkdir -p "$STATE/cfg-$call"
  touch "$STATE/cfg-$call/marker"
done
runcli exported exported hi
KEEP_CONFIG=1 ALLOW_NONESSENTIAL_TRAFFIC=1 runcli prefixed prefixed hi
for call in exported prefixed; do
  if [[ -e "$STATE/cfg-$call/marker" ]]; then
    echo "$call: config kept"
  else
    echo "$call: config reset"
  fi
done
start_receiver probe.py 0 "$OUT/probe.txt" 1
wait "$RECEIVER_PID"
RECEIVER_PID=
cat "$OUT/probe.txt"
EOF
cat >"$harness/client" <<'EOF'
#!/usr/bin/env bash
echo "flag ${CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:-unset}"
EOF
chmod +x "$harness/client"
want='=== exported ===
flag 1
run exported: client exited 0
=== prefixed ===
flag unset
run prefixed: client exited 0
exported: config reset
prefixed: config kept
receiver isolated 1'
status=0
out=$(env -u ANTHROPIC_API_KEY -u CLAUDE_CODE_OAUTH_TOKEN KEEP_CONFIG=1 ALLOW_NONESSENTIAL_TRAFFIC=1 \
  PHOEBIS_CAPTURE_STATE="$tmp/harness-state" bash "$harness/lib-check.sh" "$harness/client" "$tmp/harness-out" 2>&1) \
  || status=$?
[[ "$status" -eq 0 ]] || fail "the lib.sh harness exited $status, want 0: $out"
[[ "$out" == "$want" ]] || fail "the lib.sh harness with both switches exported printed: $out"

# --- capture-feature-hellos.sh ---

# A caller's PHOEBIS_CAPTURE_NETNS=1 skips the namespace, and a bind made then would lie over the machine's own
# /etc/hosts under root. A process under a map of one id already stands in such a namespace and cannot show the stop.
mapfile -t id_map </proc/self/uid_map
read -r inside _ count <<<"${id_map[0]:-}"
if [[ "${#id_map[@]}" -eq 1 && "$inside" == 0 && "$count" == 1 ]]; then
  echo "capture-tooling: this process stands in a namespace of one id, the case of the bind did not run"
else
  bound="$tmp/no-namespace"
  mkdir -p "$bound/bin"
  printf '#!/usr/bin/env bash\necho "$*" >"%s"\nexit 1\n' "$bound/mount-args" >"$bound/bin/mount"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$bound/client"
  chmod +x "$bound/bin/mount" "$bound/client"
  status=0
  out=$(env -u ANTHROPIC_API_KEY -u CLAUDE_CODE_OAUTH_TOKEN PATH="$bound/bin:$PATH" PHOEBIS_CAPTURE_NETNS=1 \
    PHOEBIS_CAPTURE_STATE="$bound/state" \
    bash "$here/scripts/capture-feature-hellos.sh" "$bound/client" "$bound/out" 2>&1 >/dev/null) || status=$?
  [[ "$status" -eq 1 ]] || fail "capture-feature-hellos.sh without its namespace exited $status, want 1: $out"
  [[ ! -e "$bound/mount-args" ]] \
    || fail "capture-feature-hellos.sh without its namespace still bound: $(cat "$bound/mount-args")"
  [[ "$out" == *"capture-feature-hellos.sh: no namespace of this capture stands, so no hosts file is bound"* ]] \
    || fail "capture-feature-hellos.sh without its namespace printed: $out"
fi

echo "capture-tooling: ok"
