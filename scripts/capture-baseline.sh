#!/usr/bin/env bash
# Re-captures the inference-plane wire shape of a Claude Code build for a diff against
# crates/profile/fixtures/ before re-pinning the profile.
#
# Usage: scripts/capture-baseline.sh <path-to-claude-binary> <output-dir>
#
# Runs the binary in `-p` print mode under a scrubbed environment (env -i, plus PATH, HOME, and the
# variables set below) against two loopback receivers in scripts/capture/: tls_only.py answers the
# TLS ClientHello when ANTHROPIC_BASE_URL points at https, http_raw.py answers the HTTP/1.1 request
# when it points at http. Each variant runs once per credential mode (API key, OAuth token, OAuth
# token with the first-party override) plus the HTTP default and gzip-forced modes, writing raw JSON
# captures to <output-dir>/tls_only.json and <output-dir>/http_raw.json.
#
# The control plane (the OAuth token, profile, usage, and roles calls) is not captured here: those
# calls hit hardcoded hosts that ANTHROPIC_BASE_URL cannot redirect, so recording them needs a live
# `/login` and `/status` session run by hand behind a CONNECT tunnel that trusts a throwaway CA. Point
# HTTPS_PROXY at scripts/capture/mitm/mitm5.py (paired with scripts/capture/connect_capture.py for the
# ClientHello alone) and NODE_EXTRA_CA_CERTS at that CA's certificate; mitm5.py answers the token,
# profile, usage, and roles endpoints with canned responses and records what it received. Generate the
# throwaway CA and leaf certificate yourself; none is checked in.
#
# Nothing here talks to Anthropic. Every receiver answers with a canned 400 or the canned control-plane
# bodies above, and the script refuses to start if the caller's environment holds a real credential.

set -euo pipefail

DUMMY_API_KEY="sk-ant-api03-capture-dummy"
DUMMY_OAUTH_TOKEN="sk-ant-oat01-DUMMYOAUTH"

# A stray real credential in the environment would otherwise be sent, in full, to a receiver whose
# only reply is a canned 400 body: reject the run instead of trusting a caller to have unset it.
if [[ -n "${ANTHROPIC_API_KEY:-}" && "${ANTHROPIC_API_KEY}" != "$DUMMY_API_KEY" ]]; then
  echo "capture-baseline.sh: refusing to run with a real ANTHROPIC_API_KEY set" >&2
  exit 1
fi
if [[ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" && "${CLAUDE_CODE_OAUTH_TOKEN}" != "$DUMMY_OAUTH_TOKEN" ]]; then
  echo "capture-baseline.sh: refusing to run with a real CLAUDE_CODE_OAUTH_TOKEN set" >&2
  exit 1
fi

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <path-to-claude-binary> <output-dir>" >&2
  exit 1
fi
BINARY=$(realpath "$1")
OUT=$(realpath -m "$2")
CAPTURE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/capture" && pwd)"

if [[ ! -x "$BINARY" ]]; then
  echo "capture-baseline.sh: $BINARY is not an executable file" >&2
  exit 1
fi

mkdir -p "$OUT"
TLS_PORT=48441
HTTP_PORT=48442
DURATION=400

python3 "$CAPTURE_DIR/tls_only.py" "$TLS_PORT" "$OUT/tls_only.json" "$DURATION" &
TLS_PID=$!
python3 "$CAPTURE_DIR/http_raw.py" "$HTTP_PORT" "$OUT/http_raw.json" "$DURATION" &
HTTP_PID=$!
# SIGTERM flushes each receiver's JSON, so a run stopped early keeps whatever it already captured.
trap 'kill "$TLS_PID" "$HTTP_PID" 2>/dev/null; wait "$TLS_PID" "$HTTP_PID" 2>/dev/null' EXIT
sleep 1

runcli() { # $1=label $2=base URL $3=prompt; remaining args are extra VAR=value assignments
  local label=$1 base=$2 prompt=$3; shift 3
  local cfg cwd
  cfg=$(mktemp -d) cwd=$(mktemp -d)
  echo "=== $label -> $base ==="
  ( cd "$cwd" && env -i \
      PATH="$PATH" HOME="$HOME" \
      CLAUDE_CONFIG_DIR="$cfg" ANTHROPIC_BASE_URL="$base" \
      DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 DISABLE_AUTOUPDATER=1 \
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_COST_WARNINGS=1 \
      "$@" \
      timeout 45 "$BINARY" -p "$prompt" --model claude-sonnet-4-5 2>&1 | tail -3 )
  rm -rf "$cfg" "$cwd"
}

runcli apikey            "https://127.0.0.1:$TLS_PORT" hi ANTHROPIC_API_KEY="$DUMMY_API_KEY"
runcli oauth              "https://127.0.0.1:$TLS_PORT" hi CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN"
runcli oauth-first-party  "https://127.0.0.1:$TLS_PORT" hi \
  CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN" _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
runcli http-default       "http://127.0.0.1:$HTTP_PORT" hi ANTHROPIC_API_KEY="$DUMMY_API_KEY"
LONG=$(python3 -c "print('word '*1400)")
runcli http-gzip-forced   "http://127.0.0.1:$HTTP_PORT" "$LONG" \
  ANTHROPIC_API_KEY="$DUMMY_API_KEY" CLAUDE_CODE_GZIP_REQUEST_BODIES=1

kill "$TLS_PID" "$HTTP_PID" 2>/dev/null
wait "$TLS_PID" "$HTTP_PID" 2>/dev/null
trap - EXIT
echo "capture written: $OUT/tls_only.json $OUT/http_raw.json"
