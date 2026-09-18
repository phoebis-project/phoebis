#!/usr/bin/env bash
# Re-captures the wire shape the router hands its own upstream, from a real paired account, and diffs it
# against scripts/build-fixtures.py --check --through-router, in one command.
#
# Usage: PHOEBIS_CAPTURE_OAUTH_KEY=<router key> PHOEBIS_CAPTURE_API_KEY=<router key> \
#        ROUTER_ADMIN_SECRET=<secret> [ROUTER_VIEWER_SECRET=<secret>] [PHOEBIS_CAPTURE_BASELINE=<dir>] \
#        scripts/capture-through-router.sh <path-to-claude-binary> <output-dir> <config.yaml>
#
# Mirrors scripts/capture-baseline.sh: the same credential refusal, the same `unshare -rn` re-exec (so this
# capture instance cannot reach the real internet even if the interception below were bypassed), and the same
# five client runs under the same cfg names. It differs in these ways:
#
# - PHOEBIS_CAPTURE_OAUTH_KEY must name a router key whose pool holds exactly one paired oauth account, and
#   PHOEBIS_CAPTURE_API_KEY a key whose pool holds one api_key account toward api.anthropic.com. Both are router
#   keys minted through the admin API ahead of time, never Anthropic credentials, and both are required.
# - PHOEBIS_CAPTURE_BASELINE, if set, names a native capture directory (a fresh scripts/capture-baseline.sh run on
#   the same host and the same day) to grade the router's capture against; --check without --baseline grades
#   against the committed fixtures, which carry a different date in the client's own system reminder.
# - The router's own service must be stopped before this runs: the capture instance opens the same store
#   (storage.path in <config.yaml>), and a second writer on that SQLite file corrupts it. Run this as the
#   store's owner:
#     sudo -u phoebis env PHOEBIS_CAPTURE_OAUTH_KEY=... PHOEBIS_CAPTURE_API_KEY=... ROUTER_ADMIN_SECRET=... \
#       ROUTER_VIEWER_SECRET=... scripts/capture-through-router.sh <binary> <output-dir> <config.yaml>
# - Every connection the router makes toward api.anthropic.com is redirected, inside this network namespace, to
#   a throwaway TLS receiver on loopback: scripts/capture/rewrite_config.py writes $OUT/config.yaml from the
#   given <config.yaml>, moving the admin, proxy, and metrics listeners to loopback ports this capture owns and
#   pointing upstream.resolve_override and upstream.extra_ca_pem at the receiver and its CA. The router
#   authenticates with the real paired account's stored credential, but that credential never leaves loopback,
#   and scripts/capture/tls_raw.py redacts it before it ever reaches disk.
# - The CONNECT run capture-baseline.sh's step 6 performs has nothing to record here: the router answers
#   HEAD /api/hello itself rather than forwarding it, and the router's own ClientHello toward its upstream is
#   graded by crates/transport/tests/client_hello.rs, not by this capture.
#
# The script ends by running `python3 scripts/build-fixtures.py --check --through-router` (plus --baseline, when
# PHOEBIS_CAPTURE_BASELINE is set) against $OUT, and exits with its status.

set -euo pipefail

DUMMY_API_KEY="sk-ant-api03-capture-dummy"
DUMMY_OAUTH_TOKEN="sk-ant-oat01-DUMMYOAUTH"

# A stray real Anthropic credential in the environment would otherwise be sent, in full, to a receiver that
# writes every header it is handed to disk (tls_raw.py's redaction covers the router's own forwarded requests,
# not this ambient check): reject the run instead of trusting a caller to have unset it.
if [[ -n "${ANTHROPIC_API_KEY:-}" && "${ANTHROPIC_API_KEY}" != "$DUMMY_API_KEY" ]]; then
  echo "capture-through-router.sh: refusing to run with a real ANTHROPIC_API_KEY set" >&2
  exit 1
fi
if [[ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" && "${CLAUDE_CODE_OAUTH_TOKEN}" != "$DUMMY_OAUTH_TOKEN" ]]; then
  echo "capture-through-router.sh: refusing to run with a real CLAUDE_CODE_OAUTH_TOKEN set" >&2
  exit 1
fi

if [[ -z "${PHOEBIS_CAPTURE_OAUTH_KEY:-}" || -z "${PHOEBIS_CAPTURE_API_KEY:-}" ]]; then
  echo "capture-through-router.sh: PHOEBIS_CAPTURE_OAUTH_KEY and PHOEBIS_CAPTURE_API_KEY must both be set" >&2
  exit 1
fi
if [[ -z "${ROUTER_ADMIN_SECRET:-}" ]]; then
  echo "capture-through-router.sh: ROUTER_ADMIN_SECRET must be set; the capture instance loads the same config" >&2
  exit 1
fi

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <path-to-claude-binary> <output-dir> <config.yaml>" >&2
  exit 1
fi

SELF=$(realpath "${BASH_SOURCE[0]}")
CAPTURE_DIR="$(cd "$(dirname "$SELF")/capture" && pwd)"
ROOT="$(cd "$(dirname "$SELF")/.." && pwd)"
BINARY=$(realpath "$1")
OUT=$(realpath -m "$2")
CONFIG_IN=$(realpath "$3")

if [[ ! -x "$BINARY" ]]; then
  echo "capture-through-router.sh: $BINARY is not an executable file" >&2
  exit 1
fi

PHOEBIS_BIN=$(command -v phoebis || echo /usr/local/bin/phoebis)
if [[ ! -x "$PHOEBIS_BIN" ]]; then
  echo "capture-through-router.sh: no phoebis binary on PATH or at /usr/local/bin/phoebis" >&2
  exit 1
fi

if [[ "${PHOEBIS_CAPTURE_NETNS:-}" != "1" ]]; then
  if ! command -v unshare >/dev/null || ! command -v ip >/dev/null; then
    echo "capture-through-router.sh: unshare and ip are needed to build the loopback-only namespace" >&2
    exit 1
  fi
  if ! unshare -rn true 2>/dev/null; then
    echo "capture-through-router.sh: unshare -rn does not work here; refusing to capture with the internet reachable" >&2
    exit 1
  fi
  export PHOEBIS_CAPTURE_NETNS=1
  # shellcheck disable=SC2016 # $0 and $@ are expanded by the inner shell, not this one
  exec unshare -rn bash -c 'ip link set lo up && exec bash "$0" "$@"' "$SELF" "$@"
fi

STATE=/tmp/cc-capture-through-router
CLIENT_CWD="$STATE/cwd"
TLS_PORT=48444
ROUTER_BASE="http://127.0.0.1:48317"
CLEAN_DURATION=400
SHORT_DURATION=120
mkdir -p "$OUT/ca" "$CLIENT_CWD"

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=capture-ca \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign \
  -keyout "$OUT/ca/ca.key" -out "$OUT/ca/ca.pem" -days 1 >/dev/null 2>&1
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=api.anthropic.com \
  -addext subjectAltName=DNS:api.anthropic.com -keyout "$OUT/ca/leaf.key" -out "$OUT/ca/leaf.csr" >/dev/null 2>&1
openssl x509 -req -CA "$OUT/ca/ca.pem" -CAkey "$OUT/ca/ca.key" -copy_extensions copy \
  -in "$OUT/ca/leaf.csr" -out "$OUT/ca/leaf.pem" -days 1 -CAcreateserial >/dev/null 2>&1

python3 "$CAPTURE_DIR/rewrite_config.py" "$CONFIG_IN" "$OUT/config.yaml" "$TLS_PORT" "$OUT/ca/ca.pem"

RECEIVER_PID=
ROUTER_PID=

start_receiver() { # $1=output-dir $2=seconds
  python3 "$CAPTURE_DIR/tls_raw.py" "$TLS_PORT" "$1" "$2" "$OUT/ca/leaf.pem" "$OUT/ca/leaf.key" &
  RECEIVER_PID=$!
  sleep 1
}

stop_receiver() {
  [[ -n "$RECEIVER_PID" ]] || return 0
  # SIGTERM flushes the receiver's JSON, so a run stopped early keeps whatever it already captured.
  kill "$RECEIVER_PID" 2>/dev/null || true
  wait "$RECEIVER_PID" 2>/dev/null || true
  RECEIVER_PID=
}

stop_router() {
  [[ -n "$ROUTER_PID" ]] || return 0
  kill "$ROUTER_PID" 2>/dev/null || true
  wait "$ROUTER_PID" 2>/dev/null || true
  ROUTER_PID=
}

cleanup() {
  stop_receiver
  stop_router
}
trap cleanup EXIT

env ROUTER_ADMIN_SECRET="$ROUTER_ADMIN_SECRET" ROUTER_VIEWER_SECRET="${ROUTER_VIEWER_SECRET:-}" \
  "$PHOEBIS_BIN" --config "$OUT/config.yaml" &
ROUTER_PID=$!
sleep 1

RUN_COUNT=0
RUN_FAILURES=0

runcli() { # $1=label $2=config dir name $3=prompt; remaining args are VAR=value assignments for the client
  local name=$1 prompt=$3 status=0
  local cfg="$STATE/cfg-$2"
  shift 3
  rm -rf "$cfg"
  mkdir -p "$cfg"
  echo "=== $name ==="
  # The router answers 400 to every relayed run here (tls_raw.py's canned response), so the client is expected
  # to fail; capture the status explicitly (the `||` keeps `set -e` from aborting the whole script on that
  # failure).
  ( cd "$CLIENT_CWD" && env -i \
      PATH="$PATH" HOME="$HOME" \
      CLAUDE_CONFIG_DIR="$cfg" \
      DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 DISABLE_AUTOUPDATER=1 \
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_COST_WARNINGS=1 \
      "$@" \
      timeout 45 "$BINARY" -p "$prompt" --model claude-sonnet-4-5 2>&1 | tail -3 ) || status=$?
  echo "run $name: client exited $status"
  RUN_COUNT=$((RUN_COUNT + 1))
  if [[ "$status" -ne 0 ]]; then
    RUN_FAILURES=$((RUN_FAILURES + 1))
  fi
  return 0
}

start_receiver "$OUT" "$CLEAN_DURATION"
runcli oauth co hi \
  ANTHROPIC_BASE_URL="$ROUTER_BASE" CLAUDE_CODE_OAUTH_TOKEN="$PHOEBIS_CAPTURE_OAUTH_KEY"
runcli oauth-first-party cof hi \
  ANTHROPIC_BASE_URL="$ROUTER_BASE" CLAUDE_CODE_OAUTH_TOKEN="$PHOEBIS_CAPTURE_OAUTH_KEY" \
  _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
runcli apikey ca hi \
  ANTHROPIC_BASE_URL="$ROUTER_BASE" ANTHROPIC_API_KEY="$PHOEBIS_CAPTURE_API_KEY"
runcli oauth-first-party-again cog hi \
  ANTHROPIC_BASE_URL="$ROUTER_BASE" CLAUDE_CODE_OAUTH_TOKEN="$PHOEBIS_CAPTURE_OAUTH_KEY" \
  _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
stop_receiver

# Its request is evidence only, so it is recorded into $OUT/gzip/ and its modes file moved to
# $OUT/gzip_modes.json, out of clean_modes.json, exactly as capture-baseline.sh keeps it out of the four
# fixture-bound runs there.
LONG=$(python3 -c "print('word '*1400)")
start_receiver "$OUT/gzip" "$SHORT_DURATION"
runcli gzip-forced gz "$LONG" \
  ANTHROPIC_BASE_URL="$ROUTER_BASE" CLAUDE_CODE_OAUTH_TOKEN="$PHOEBIS_CAPTURE_OAUTH_KEY" \
  CLAUDE_CODE_GZIP_REQUEST_BODIES=1
stop_receiver
mv "$OUT/gzip/clean_modes.json" "$OUT/gzip_modes.json"

echo "ran $RUN_COUNT runs, $RUN_FAILURES exited non-zero"
echo "capture written under $OUT: clean_modes.json, clean_body_<i>.json, gzip_modes.json"

stop_router
trap - EXIT
cd "$ROOT"
exec python3 scripts/build-fixtures.py --check --through-router ${PHOEBIS_CAPTURE_BASELINE:+--baseline "$PHOEBIS_CAPTURE_BASELINE"} "$OUT"
