#!/usr/bin/env bash
# Each case pins a rule a real capture leans on, checked here without the client: the leaf a receiver presents
# chains to the run's own CA and names the control hosts, and mkcerts.sh refuses to start on a host without
# openssl. Every case runs against a throwaway tree, never /tmp/cc-capture or ~/.claude, and reaches no host off
# loopback.
set -euo pipefail

here=$(cd "$(dirname "$0")/../.." && pwd)
capture="$here/scripts/capture"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
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

echo "capture-tooling: ok"
