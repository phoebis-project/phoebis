# The preamble scripts/capture-baseline.sh, scripts/capture-control.sh and scripts/capture-count-tokens.sh share, so
# one loopback-only namespace, one credential refusal, one set of client state directories, and one runcli serve
# every capture and a change to any of them cannot land in one script and not the others.
#
# Sourced, never run on its own: it reads the sourcing script's own arguments and re-executes that script
# inside the namespace.
#
# What a sourcing script may read from here, since a shell file declares no interface of its own: BINARY,
# CAPTURE_DIR, CLEAN_DURATION, CLIENT_CWD, CLIENT_ENV, CONNECT_PORT, CONTROL_RECEIVER_PID, CONTROL_RECEIVER_PORT,
# DUMMY_API_KEY, DUMMY_OAUTH_TOKEN, HTTP_PORT, OUT, RECEIVER_PID, ROOT, RUN_COUNT, RUN_FAILURES, SCRIPT_NAME, SELF,
# SHORT_DURATION, STATE, and the functions client_env, runcli, start_control_receiver, start_receiver,
# stop_control_receiver, and stop_receiver.

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

# A test points PHOEBIS_CAPTURE_STATE at a throwaway tree. A real capture leaves it unset: the system prompt embeds
# the paths of the working and config directories, and the committed fixtures were captured under /tmp/cc-capture.
STATE=${PHOEBIS_CAPTURE_STATE:-/tmp/cc-capture}
CLIENT_CWD="$STATE/cwd"
HTTP_PORT=48442
CONNECT_PORT=48443
CONTROL_RECEIVER_PORT=48444
CLEAN_DURATION=400
SHORT_DURATION=120
mkdir -p "$OUT" "$CLIENT_CWD"

RECEIVER_PID=
CONTROL_RECEIVER_PID=

start_receiver() { # $1=script $2=port $3=output $4=seconds, then any further arguments the receiver takes
  python3 "$CAPTURE_DIR/$1" "${@:2}" &
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

# The control capture starts this receiver per phase while the inference receiver of start_receiver serves the whole
# variant, so each keeps a pid of its own.
start_control_receiver() { # $1=output $2=seconds $3=certs dir $4=responses dir
  python3 -I "$CAPTURE_DIR/control_receiver.py" "$CONTROL_RECEIVER_PORT" "$@" &
  CONTROL_RECEIVER_PID=$!
  sleep 1
}

stop_control_receiver() {
  [[ -n "$CONTROL_RECEIVER_PID" ]] || return 0
  kill "$CONTROL_RECEIVER_PID" 2>/dev/null || true
  wait "$CONTROL_RECEIVER_PID" 2>/dev/null || true
  CONTROL_RECEIVER_PID=
}

trap 'stop_receiver; stop_control_receiver' EXIT

RUN_COUNT=0
RUN_FAILURES=0

# The environment a client run starts from, shared by runcli and the control capture's login phases so every run of
# a capture carries the same flags. ALLOW_NONESSENTIAL_TRAFFIC=1 for the call leaves
# CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC out, since an empty value appended later would leave it set.
client_env() { # $1=config dir, fills CLIENT_ENV
  CLIENT_ENV=(PATH="$PATH" HOME="$HOME" CLAUDE_CONFIG_DIR="$1"
    DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 DISABLE_AUTOUPDATER=1)
  if [[ "${ALLOW_NONESSENTIAL_TRAFFIC:-}" != 1 ]]; then
    CLIENT_ENV+=(CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1)
  fi
  CLIENT_ENV+=(DISABLE_COST_WARNINGS=1)
}

# A caller that needs extra client flags sets CLI_ARGS; bash expands an array that was never declared to
# nothing even under set -u, so the baseline never has to declare an empty one.
runcli() { # $1=label $2=config dir name $3=prompt; remaining args are VAR=value assignments for the client
  local name=$1 prompt=$3 status=0
  local cfg="$STATE/cfg-$2"
  shift 3
  # A refresh run needs the credentials its login phase wrote, so KEEP_CONFIG=1 for the call skips the reset.
  if [[ "${KEEP_CONFIG:-}" != 1 ]]; then
    rm -rf "$cfg"
    mkdir -p "$cfg"
  fi
  # ALLOW_NONESSENTIAL_TRAFFIC=1 for the call reaches client_env, for a run that shows the calls the flag suppresses.
  client_env "$cfg"
  echo "=== $name ==="
  # Every receiver answers with an error or drops the connection, so the client is expected to fail here;
  # capture the status explicitly (the `||` keeps `set -e` from aborting the whole script on that failure).
  ( cd "$CLIENT_CWD" && env -i "${CLIENT_ENV[@]}" "$@" \
      timeout 45 "$BINARY" -p "$prompt" "${CLI_ARGS[@]}" --model claude-sonnet-4-5 2>&1 | tail -3 ) || status=$?
  echo "run $name: client exited $status"
  RUN_COUNT=$((RUN_COUNT + 1))
  if [[ "$status" -ne 0 ]]; then
    RUN_FAILURES=$((RUN_FAILURES + 1))
  fi
  return 0
}
