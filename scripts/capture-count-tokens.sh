#!/usr/bin/env bash
# Records the client's own token count in each credential mode, past a receiver that answers the first inference
# request with a canned tool_use for Read on scripts/capture/count-sample.txt: the Read tool counts a file whose
# local estimate exceeds a quarter of its token limit before the turn runs, which is the one count a print-mode
# run can reach. Runs inside the same loopback-only namespace as scripts/capture-baseline.sh, with the same dummy
# credentials, working directory, and config directories; nothing here talks to Anthropic.
#
# Usage: scripts/capture-count-tokens.sh <path-to-claude-binary> <output-dir>
#
# The runs, in order, each into <output-dir>/count_<mode>/:
#   1. oauth: OAuth token
#   2. oauth-fp: OAuth token with the first-party override
#   3. apikey: API key
# then `python3 scripts/build-fixtures.py --count <output-dir> --check`, exiting with its status.

source "$(dirname "$0")/capture/lib.sh"

SAMPLE="$ROOT/scripts/capture/count-sample.txt"
PROMPT="Read the file $SAMPLE with the Read tool and tell me its first line."
CLI_ARGS=(--allowedTools Read)
HTTP_BASE="http://127.0.0.1:$HTTP_PORT"

start_count_receiver() { # $1=output
  python3 "$CAPTURE_DIR/count_receiver.py" "$HTTP_PORT" "$1" "$SHORT_DURATION" "$SAMPLE" &
  RECEIVER_PID=$!
  sleep 1
}

start_count_receiver "$OUT/count_oauth"
runcli oauth co "$PROMPT" \
  ANTHROPIC_BASE_URL="$HTTP_BASE" CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN"
stop_receiver
start_count_receiver "$OUT/count_oauth_fp"
runcli oauth-first-party cof "$PROMPT" \
  ANTHROPIC_BASE_URL="$HTTP_BASE" CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN" \
  _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
stop_receiver
start_count_receiver "$OUT/count_apikey"
runcli apikey ca "$PROMPT" \
  ANTHROPIC_BASE_URL="$HTTP_BASE" ANTHROPIC_API_KEY="$DUMMY_API_KEY"
stop_receiver

for mode in count_oauth count_oauth_fp count_apikey; do
  if ! grep -q '"request_line": "POST /v1/messages/count_tokens' "$OUT/$mode/clean_modes.json"; then
    echo "capture-count-tokens.sh: $mode recorded no count_tokens request" >&2
    exit 1
  fi
done
echo "ran $RUN_COUNT runs, $RUN_FAILURES exited non-zero"
trap - EXIT
cd "$ROOT"
exec python3 scripts/build-fixtures.py --count "$OUT" --check
