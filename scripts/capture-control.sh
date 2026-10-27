#!/usr/bin/env bash
# Records the control plane of a Claude Code build (its OAuth token grants and its profile, roles and usage reads)
# and, given a baseline capture, diffs it against crates/profile/fixtures/ before re-pinning the profile.
#
# Usage: scripts/capture-control.sh <path-to-claude-binary> <output-dir>
#
# It sources scripts/capture/lib.sh, so it runs in the same loopback-only namespace as scripts/capture-baseline.sh,
# refuses to start with a real credential in the environment, and keeps the client's working and config directories
# under /tmp/cc-capture. The control calls go to hosts the client dials by name, which ANTHROPIC_BASE_URL cannot
# redirect, so every client run here gets HTTPS_PROXY pointed at scripts/capture/control_receiver.py on port 48444
# (CONTROL_RECEIVER_PORT) and NODE_EXTRA_CA_CERTS at the throwaway CA that scripts/capture/mkcerts.sh writes to
# <output-dir>/certs/ before the first run. scripts/capture/http_raw.py on port 48442 (HTTP_PORT) answers the refresh
# phase's inference post with its canned 400 and records it under <variant>/inference/.
#
# The capture runs in two variants: into <output-dir>/control/ with the environment runcli gives every capture, the
# one the committed fixtures were captured under, and into <output-dir>/control-unflagged/ with the same environment
# less CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC. A call missing from the first variant then shows up in the second
# when the flag suppresses it, and in neither when the build no longer makes it.
#
# The phases of each variant, in order, each recorded by a control receiver of its own into <variant>/<phase>.json:
#   1. login-claude: scripts/capture/drive_login.py logs the client in with a Claude account from a fresh config
#      directory and reads /status, and writes the authorize URL and the screen it read beside the recording.
#   2. login-console: the same with an Anthropic Console account.
#   3. refresh: the config directory login-claude left, copied with its token's expiresAt set to 0, so the client's
#      print-mode run starts with the refresh grant. The phase is skipped with a note when login-claude left no
#      expiresAt.
# A login whose driver gives up (exit 2, on a screen it does not know or a client that exited) is logged with its
# screen file and the next phase runs. Any other failure of the driver stops the capture.
#
# With CAPTURE_BASELINE naming a capture directory of scripts/capture-baseline.sh (one holding clean_modes.json), the
# script ends with `python3 -I scripts/build-fixtures.py --check --control <variant-dir> "$CAPTURE_BASELINE"` for each
# variant, writes each one's output to <variant>/check.txt, and exits with the first non-zero status of the two, or
# 0. Without it, the script prints the two commands to run later and exits 0.
#
# Nothing here talks to Anthropic. The receivers answer canned bodies on loopback and record every header and body
# verbatim, the authorization header and the token grants included, which is safe only while the client holds
# lib.sh's dummy credentials and the canned tokens the receiver hands out. The recordings, URLs and screens are raw
# capture output and stay out of the repository. scripts/build-fixtures.py puts placeholders in for the tokens and
# the code grant's state and code_verifier before it compares a recording or writes a fixture from one.

source "$(dirname "$0")/capture/lib.sh"

CA="$OUT/certs/ca.pem"
PROXY="http://127.0.0.1:$CONTROL_RECEIVER_PORT"
RESPONSES="$ROOT/crates/profile/fixtures/responses"

"$CAPTURE_DIR/mkcerts.sh" "$OUT"

login_phase() { # $1=variant dir $2=1 to leave the non-essential-traffic flag out $3=claude or console
  local dir=$1 allow=$2 method=$3 status=0
  local phase="login-$method"
  local cfg="$STATE/cfg-$phase"
  rm -rf "$cfg"
  mkdir -p "$cfg"
  # The driver writes the URL file only once it has read a URL, so an earlier run's would sit beside this run's screen.
  rm -f "$dir/$phase-url.txt" "$dir/$phase-screen.txt"
  ALLOW_NONESSENTIAL_TRAFFIC=$allow client_env "$cfg"
  echo "=== $phase ==="
  start_control_receiver "$dir/$phase.json" "$CLEAN_DURATION" "$OUT/certs" "$RESPONSES"
  # A terminal UI may not draw its menus without TERM and a locale. The print-mode runs keep runcli's list, the
  # environment the committed fixtures were captured under.
  env -i "${CLIENT_ENV[@]}" HTTPS_PROXY="$PROXY" LANG=C.UTF-8 NODE_EXTRA_CA_CERTS="$CA" TERM=xterm-256color \
    python3 -I "$CAPTURE_DIR/drive_login.py" --binary "$BINARY" --cwd "$CLIENT_CWD" --out "$dir" --method "$method" \
    || status=$?
  stop_control_receiver
  case $status in
    0) ;;
    2) echo "phase $phase: drive_login.py exited 2, screen in $dir/$phase-screen.txt" ;;
    *)
      echo "$SCRIPT_NAME: drive_login.py exited $status in phase $phase" >&2
      exit "$status"
      ;;
  esac
}

refresh_phase() { # $1=variant dir $2=1 to leave the non-essential-traffic flag out
  local dir=$1 allow=$2 status=0
  local cfg="$STATE/cfg-refresh"
  rm -rf "$cfg"
  cp -a "$STATE/cfg-login-claude" "$cfg"
  # The client refreshes only a token it holds as expired, and the login wrote one valid for hours. Exit 3 tells a
  # login that left no token from a failure of the edit itself.
  python3 -I - "$cfg/.credentials.json" <<'PY' || status=$?
import json, sys

path = sys.argv[1]
try:
    with open(path) as f:
        credentials = json.load(f)
except (OSError, ValueError):
    sys.exit(3)
oauth = credentials.get("claudeAiOauth") if isinstance(credentials, dict) else None
if not isinstance(oauth, dict) or "expiresAt" not in oauth:
    sys.exit(3)
oauth["expiresAt"] = 0
with open(path, "w") as f:
    json.dump(credentials, f)
PY
  if [[ "$status" -eq 3 ]]; then
    echo "phase refresh skipped: login-claude left no claudeAiOauth.expiresAt in .credentials.json"
    return 0
  fi
  [[ "$status" -eq 0 ]] || exit "$status"
  start_control_receiver "$dir/refresh.json" "$CLEAN_DURATION" "$OUT/certs" "$RESPONSES"
  ALLOW_NONESSENTIAL_TRAFFIC=$allow KEEP_CONFIG=1 runcli refresh refresh hi \
    HTTPS_PROXY="$PROXY" NODE_EXTRA_CA_CERTS="$CA" ANTHROPIC_BASE_URL="http://127.0.0.1:$HTTP_PORT"
  stop_control_receiver
}

capture_variant() { # $1=variant dir name $2=1 to leave the non-essential-traffic flag out
  local dir="$OUT/$1" allow=$2
  echo "--- $1 ---"
  mkdir -p "$dir"
  # A re-run into the same directory writes no phase file for a phase it skips or whose receiver fails to start, and
  # the builder would grade the earlier run's file in its place.
  rm -f "$dir/login-claude.json" "$dir/login-console.json" "$dir/refresh.json"
  start_receiver http_raw.py "$HTTP_PORT" "$dir/inference" "$CLEAN_DURATION"
  login_phase "$dir" "$allow" claude
  login_phase "$dir" "$allow" console
  refresh_phase "$dir" "$allow"
  stop_receiver
}

capture_variant control ""
capture_variant control-unflagged 1

echo "ran $RUN_COUNT refresh runs, $RUN_FAILURES exited non-zero"
echo "phase files written:"
for variant in control control-unflagged; do
  for file in "$OUT/$variant"/*.json; do
    if [[ -e "$file" ]]; then
      echo "  $file"
    fi
  done
done

if [[ -z "${CAPTURE_BASELINE:-}" ]]; then
  echo "to compare against a baseline capture directory (one holding clean_modes.json), run from $ROOT:"
  for variant in control control-unflagged; do
    echo "  python3 -I scripts/build-fixtures.py --check --control $OUT/$variant <baseline-dir>"
  done
  exit 0
fi

# Resolved before the cd below, so a relative CAPTURE_BASELINE still names the directory the caller meant.
baseline=$(realpath -m "$CAPTURE_BASELINE")
cd "$ROOT" || exit 1
first=0
for variant in control control-unflagged; do
  status=0
  python3 -I scripts/build-fixtures.py --check --control "$OUT/$variant" "$baseline" >"$OUT/$variant/check.txt" 2>&1 \
    || status=$?
  echo "check of $variant exited $status, output in $OUT/$variant/check.txt"
  if [[ "$first" -eq 0 ]]; then
    first=$status
  fi
done
exit "$first"
