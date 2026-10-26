#!/usr/bin/env bash
# Each case pins a rule a real capture leans on, checked here without the client: the leaf a receiver presents
# chains to the run's own CA and names the control hosts, mkcerts.sh refuses to start on a host without openssl,
# and control_receiver.py answers the control calls with the committed canned bodies, records each call, and keeps
# serving after a client that drops its tunnel before the handshake. The resign example's --write mode leaves a body
# stamped with the digits the signer computes over its own bytes. Every case runs against a throwaway tree, never
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
subject=$(openssl x509 -in "$tmp/run/certs/ca.pem" -noout -subject -nameopt RFC2253)
[[ "$subject" == "subject=CN=phoebis capture CA" ]] || fail "the CA's subject is not CN=phoebis capture CA: $subject"
for cert in ca.pem leaf.pem; do
  text=$(openssl x509 -in "$tmp/run/certs/$cert" -noout -text)
  echo "$text" | grep -qF 'Public-Key: (2048 bit)' || fail "certs/$cert does not carry a 2048-bit RSA key"
done
openssl x509 -in "$tmp/run/certs/leaf.pem" -noout -checkend $((6 * 86400)) >/dev/null \
  || fail "the leaf expires within 6 days, want 7"
openssl x509 -in "$tmp/run/certs/leaf.pem" -noout -checkend $((8 * 86400)) >/dev/null \
  && fail "the leaf is still valid in 8 days, want 7"

# On a host without openssl the script refuses before it writes anything, so a run never starts with a certs/
# directory half made. bash is named by its own path because the PATH under test holds nothing at all.
mkdir -p "$tmp/empty-bin"
status=0
out=$(PATH="$tmp/empty-bin" "$BASH" "$capture/mkcerts.sh" "$tmp/no-openssl" 2>&1) || status=$?
[[ "$status" -eq 1 ]] || fail "mkcerts.sh without openssl exited $status, want 1: $out"
[[ "$out" == "mkcerts.sh: openssl is needed" ]] || fail "mkcerts.sh without openssl printed: $out"
[[ ! -e "$tmp/no-openssl" ]] || fail "mkcerts.sh without openssl still wrote $tmp/no-openssl"

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

# The recording is removed first, so only the SIGTERM handler's own dump can bring it back.
rm "$recording"
kill -TERM "$receiver_pid"
status=0
wait "$receiver_pid" || status=$?
receiver_pid=
[[ "$status" -eq 0 ]] || fail "control_receiver.py exited $status on SIGTERM, want 0"
[[ -s "$recording" ]] || fail "control_receiver.py wrote no recording on SIGTERM"
five="$three
3 x:443 error
4 api.anthropic.com:443 GET /api/oauth/profile HTTP/1.1 -> 200"
got=$(summarize "$recording") || fail "the recording written on SIGTERM does not parse: $got"
[[ "$got" == "$five" ]] || fail "the recording written on SIGTERM reads: $got"

# --- resign --write ---

# scripts/build-fixtures.py redacts a fresh body's ids before it writes the fixture, so the digits the client signed
# no longer match the bytes, and the write mode stores the digits the signer computes over what the file now holds.
# The compare run that follows exits 0 only when the stored digits are the signer's. A message is read from its own
# stream, so cargo's output while the first run builds the example stays out of the compared text.
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
[[ "$out" == "resign: $body: 00000 -> ${stamp:4:5}" ]] || fail "resign --write printed: $out"
out=$(resign "$body" 2>&1) || fail "the compare run after resign --write reports drift: $out"
out=$(resign --write "$body" 2>"$tmp/resign.err") || fail "a second resign --write failed: $out $(<"$tmp/resign.err")"
[[ "$out" == "resign: $body: unchanged" ]] || fail "a second resign --write printed: $out"

# A body without a stamp is refused as the compare run refuses it, and the file is left as it was.
bare='{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1;"}],"messages":[]}'
printf '%s' "$bare" >"$tmp/bare.json"
status=0
out=$(resign --write "$tmp/bare.json" 2>"$tmp/resign.err") || status=$?
[[ "$status" -eq 1 ]] || fail "resign --write on a body without a stamp exited $status, want 1: $(<"$tmp/resign.err")"
grep -qxF "resign: $tmp/bare.json: the billing block carries no cch= stamp" "$tmp/resign.err" \
  || fail "resign --write on a body without a stamp printed: $(<"$tmp/resign.err")"
[[ "$(<"$tmp/bare.json")" == "$bare" ]] || fail "resign --write rewrote a body without a stamp: $(<"$tmp/bare.json")"

echo "capture-tooling: ok"
