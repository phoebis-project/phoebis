# The preamble scripts/capture-baseline.sh and scripts/capture-count-tokens.sh share, so one loopback-only
# namespace, one credential refusal, one set of client state directories, and one runcli serve both captures
# and a change to any of them cannot land in one script and not the other.
#
# Sourced, never run on its own: it reads the sourcing script's own arguments and re-executes that script
# inside the namespace.
#
# What a sourcing script may read from here, since a shell file declares no interface of its own: BINARY,
# CAPTURE_DIR, CLEAN_DURATION, CLIENT_CWD, CONNECT_PORT, DUMMY_API_KEY, DUMMY_OAUTH_TOKEN, HTTP_PORT, OUT,
# RECEIVER_PID, ROOT, RUN_COUNT, RUN_FAILURES, SCRIPT_NAME, SELF, SHORT_DURATION, STATE, and the functions
# runcli, start_receiver, and stop_receiver.

# shellcheck shell=bash
set -euo pipefail

# BASH_SOURCE[1] is the sourcing script, where BASH_SOURCE[0] would be this file. The re-exec below has to
# restart the caller inside the namespace, and the paths and the messages here name the caller too.
SELF=$(realpath "${BASH_SOURCE[1]}")
SCRIPT_NAME=$(basename "$SELF")

DUMMY_API_KEY="sk-ant-api03-capture-dummy"
DUMMY_OAUTH_TOKEN="sk-ant-oat01-DUMMYOAUTH"

# A stray real credential in the environment would otherwise be sent, in full, to a receiver that writes every
# header it is handed to disk: reject the run instead of trusting a caller to have unset it.
if [[ -n "${ANTHROPIC_API_KEY:-}" && "${ANTHROPIC_API_KEY}" != "$DUMMY_API_KEY" ]]; then
  echo "$SCRIPT_NAME: refusing to run with a real ANTHROPIC_API_KEY set" >&2
  exit 1
fi
if [[ -n "${CLAUDE_CODE_OAUTH_TOKEN:-}" && "${CLAUDE_CODE_OAUTH_TOKEN}" != "$DUMMY_OAUTH_TOKEN" ]]; then
  echo "$SCRIPT_NAME: refusing to run with a real CLAUDE_CODE_OAUTH_TOKEN set" >&2
  exit 1
fi

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <path-to-claude-binary> <output-dir>" >&2
  exit 1
fi

CAPTURE_DIR="$(cd "$(dirname "$SELF")/capture" && pwd)"
ROOT="$(cd "$(dirname "$SELF")/.." && pwd)"
BINARY=$(realpath "$1")
OUT=$(realpath -m "$2")

if [[ ! -x "$BINARY" ]]; then
  echo "$SCRIPT_NAME: $BINARY is not an executable file" >&2
  exit 1
fi

if [[ "${PHOEBIS_CAPTURE_NETNS:-}" != "1" ]]; then
  if ! command -v unshare >/dev/null || ! command -v ip >/dev/null; then
    echo "$SCRIPT_NAME: unshare and ip are needed to build the loopback-only namespace" >&2
    exit 1
  fi
  if ! unshare -rn true 2>/dev/null; then
    echo "$SCRIPT_NAME: unshare -rn does not work here; refusing to capture with the internet reachable" >&2
    exit 1
  fi
  export PHOEBIS_CAPTURE_NETNS=1
  # shellcheck disable=SC2016 # $0 and $@ are expanded by the inner shell, not this one
  exec unshare -rn bash -c 'ip link set lo up && exec bash "$0" "$@"' "$SELF" "$@"
fi

STATE=/tmp/cc-capture
CLIENT_CWD="$STATE/cwd"
HTTP_PORT=48444
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

# A caller that needs extra client flags sets CLI_ARGS; bash expands an array that was never declared to
# nothing even under set -u, so the baseline never has to declare an empty one.
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
      timeout 45 "$BINARY" -p "$prompt" "${CLI_ARGS[@]}" --model claude-sonnet-4-5 2>&1 | tail -3 ) || status=$?
  echo "run $name: client exited $status"
  RUN_COUNT=$((RUN_COUNT + 1))
  if [[ "$status" -ne 0 ]]; then
    RUN_FAILURES=$((RUN_FAILURES + 1))
  fi
  return 0
}
