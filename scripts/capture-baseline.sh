#!/usr/bin/env bash
# Re-captures the inference-plane wire shape of a Claude Code build and diffs it against
# crates/profile/fixtures/, in one command, before re-pinning the profile.
#
# Usage: scripts/capture-baseline.sh <path-to-claude-binary> <output-dir>
#
# The whole capture runs in a loopback-only network namespace: the script re-executes itself under `unshare -rn`
# and brings `lo` up inside, so a client build that ignored ANTHROPIC_BASE_URL or HTTPS_PROXY could not reach the
# internet at all. Measured on this host: `unshare -rn` works for an unprivileged user, loopback connects succeed
# inside it, and an outbound connect fails with `Network is unreachable`. The script refuses to start where
# `unshare -rn` does not work, rather than trusting the caller's environment.
#
# Two loopback receivers in scripts/capture/ record the traffic. http_raw.py answers each HTTP/1.1 request with a
# canned 400 and writes <output-dir>/clean_modes.json plus one clean_body_<i>.json per request, which is the
# layout scripts/build-fixtures.py reads. connect_capture.py answers CONNECT with a 200, records the ClientHello,
# closes without ever connecting anywhere, and writes <output-dir>/connect.json.
#
# The runs, in order:
#   1. OAuth token
#   2. OAuth token with the first-party override
#   3. API key
#   4. OAuth token with the first-party override again, which is the second CCH vector
#   5. OAuth token with gzipped request bodies forced. Its request is evidence only, so it is recorded into
#      <output-dir>/gzip/ and its modes file moved to <output-dir>/gzip_modes.json, out of clean_modes.json.
#   6. one run with HTTPS_PROXY pointed at connect_capture.py and no ANTHROPIC_BASE_URL, so the client sends
#      CONNECT api.anthropic.com:443 and the recorded ClientHello carries the server name api.anthropic.com,
#      which is what makes it comparable to hello_inference.hex. This step depends on the client honoring
#      HTTPS_PROXY, measured on 2.1.269; a run that records no ClientHello fails the script.
#
# Runs 1 to 5 point ANTHROPIC_BASE_URL at http_raw.py. The client's working directory is fixed at
# /tmp/cc-capture/cwd and its config directory at /tmp/cc-capture/cfg-<name>, wiped before each run. The system
# prompt embeds both paths, in the working-directory line and in the auto-memory section, so a mktemp directory
# would show up as body drift on every capture. The four cfg names of the compared runs are pinned by the
# committed fixtures, which were captured under them; renaming one changes the body and its Content-Length.
#
# The script finishes by running `python3 scripts/build-fixtures.py --check <output-dir>` and exits with its
# status, so one command captures and compares.
#
# The control plane (the OAuth token, profile, usage, and roles calls) is not captured here: those calls hit
# hardcoded hosts that ANTHROPIC_BASE_URL cannot redirect, so recording them needs a live `/login` and `/status`
# session run by hand behind a CONNECT tunnel that trusts a throwaway CA. Point HTTPS_PROXY at
# scripts/capture/mitm/mitm5.py and NODE_EXTRA_CA_CERTS at that CA's certificate; mitm5.py answers the token,
# profile, usage, and roles endpoints with canned responses and records what it received. Generate the throwaway
# CA and leaf certificate yourself; none is checked in. Without that half, --check reports the control-plane
# fixtures as not captured and compares the rest.
#
# Nothing here talks to Anthropic. Every receiver answers with a canned 400 or a canned 200, and the script
# refuses to start if the caller's environment holds a real credential.

set -euo pipefail

DUMMY_API_KEY="sk-ant-api03-capture-dummy"
DUMMY_OAUTH_TOKEN="sk-ant-oat01-DUMMYOAUTH"

# A stray real credential in the environment would otherwise be sent, in full, to a receiver that writes every
# header it is handed to disk: reject the run instead of trusting a caller to have unset it.
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

SELF=$(realpath "${BASH_SOURCE[0]}")
CAPTURE_DIR="$(cd "$(dirname "$SELF")/capture" && pwd)"
ROOT="$(cd "$(dirname "$SELF")/.." && pwd)"
BINARY=$(realpath "$1")
OUT=$(realpath -m "$2")

if [[ ! -x "$BINARY" ]]; then
  echo "capture-baseline.sh: $BINARY is not an executable file" >&2
  exit 1
fi

if [[ "${PHOEBIS_CAPTURE_NETNS:-}" != "1" ]]; then
  if ! command -v unshare >/dev/null || ! command -v ip >/dev/null; then
    echo "capture-baseline.sh: unshare and ip are needed to build the loopback-only namespace" >&2
    exit 1
  fi
  if ! unshare -rn true 2>/dev/null; then
    echo "capture-baseline.sh: unshare -rn does not work here; refusing to capture with the internet reachable" >&2
    exit 1
  fi
  export PHOEBIS_CAPTURE_NETNS=1
  # shellcheck disable=SC2016 # $0 and $@ are expanded by the inner shell, not this one
  exec unshare -rn bash -c 'ip link set lo up && exec bash "$0" "$@"' "$SELF" "$@"
fi

STATE=/tmp/cc-capture
CLIENT_CWD="$STATE/cwd"
HTTP_PORT=48442
CONNECT_PORT=48443
CLEAN_DURATION=400
SHORT_DURATION=120
mkdir -p "$OUT" "$CLIENT_CWD"

RECEIVER_PID=

start_receiver() { # $1=script $2=port $3=output $4=seconds
  python3 "$CAPTURE_DIR/$1" "$2" "$3" "$4" &
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

trap stop_receiver EXIT

RUN_COUNT=0
RUN_FAILURES=0

runcli() { # $1=label $2=config dir name $3=prompt; remaining args are VAR=value assignments for the client
  local name=$1 prompt=$3 status=0
  local cfg="$STATE/cfg-$2"
  shift 3
  rm -rf "$cfg"
  mkdir -p "$cfg"
  echo "=== $name ==="
  # Every receiver answers with an error or drops the connection, so the client is expected to fail here;
  # capture the status explicitly (the `||` keeps `set -e` from aborting the whole script on that failure).
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

HTTP_BASE="http://127.0.0.1:$HTTP_PORT"

start_receiver http_raw.py "$HTTP_PORT" "$OUT" "$CLEAN_DURATION"
runcli oauth co hi \
  ANTHROPIC_BASE_URL="$HTTP_BASE" CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN"
runcli oauth-first-party cof hi \
  ANTHROPIC_BASE_URL="$HTTP_BASE" CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN" \
  _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
runcli apikey ca hi \
  ANTHROPIC_BASE_URL="$HTTP_BASE" ANTHROPIC_API_KEY="$DUMMY_API_KEY"
runcli oauth-first-party-again cog hi \
  ANTHROPIC_BASE_URL="$HTTP_BASE" CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN" \
  _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
stop_receiver

LONG=$(python3 -c "print('word '*1400)")
start_receiver http_raw.py "$HTTP_PORT" "$OUT/gzip" "$SHORT_DURATION"
runcli gzip-forced gz "$LONG" \
  ANTHROPIC_BASE_URL="$HTTP_BASE" CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN" \
  CLAUDE_CODE_GZIP_REQUEST_BODIES=1
stop_receiver
mv "$OUT/gzip/clean_modes.json" "$OUT/gzip_modes.json"

start_receiver connect_capture.py "$CONNECT_PORT" "$OUT/connect.json" "$SHORT_DURATION"
runcli hello hello hi \
  HTTPS_PROXY="http://127.0.0.1:$CONNECT_PORT" CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN"
stop_receiver

python3 - "$OUT/connect.json" <<'PY'
import json, pathlib, sys
recorded = json.loads(pathlib.Path(sys.argv[1]).read_text())
hellos = [h for h in recorded if h.get("hello_len")]
if not hellos:
    sys.exit(f"capture-baseline.sh: no ClientHello recorded, so the client did not honor HTTPS_PROXY: {recorded}")
print(f"recorded {len(hellos)} ClientHello(s), lengths {sorted({h['hello_len'] for h in hellos})}")
PY

echo "ran $RUN_COUNT runs, $RUN_FAILURES exited non-zero"
echo "capture written under $OUT: clean_modes.json, clean_body_<i>.json, gzip_modes.json, connect.json"

trap - EXIT
cd "$ROOT"
exec python3 scripts/build-fixtures.py --check "$OUT"
