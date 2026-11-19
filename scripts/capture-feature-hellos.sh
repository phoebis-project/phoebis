#!/usr/bin/env bash
# Records the TLS ClientHello a Claude Code build sends on each connection toward the feature hosts, and compares the
# one toward mcp-proxy.anthropic.com with the one its fetch path sends toward api.anthropic.com. The router sends both
# hosts' pooled feature calls through its inference connector, which holds only while the two hellos are equal apart
# from what follows the host or the connection.
#
# Usage: scripts/capture-feature-hellos.sh <path-to-claude-binary> <output-dir>
#
# It sources scripts/capture/lib.sh, so it runs in the loopback-only namespace of the other captures, refuses a real
# credential in the environment, and gives the client lib.sh's dummy OAuth token. One print-mode run of the client
# carries the variables of a feature setup: HTTPS_PROXY at scripts/capture/control_receiver.py on port 48444, which
# presents the leaf scripts/capture/mkcerts.sh makes for the feature hosts, NODE_EXTRA_CA_CERTS at that run's CA, and
# ANTHROPIC_BASE_URL at scripts/capture/count_receiver.py on port 48442, which answers the first model request with a
# call of the Artifact tool that publishes an invented page, and each later one with its canned 400.
# CLAUDE_CODE_ARTIFACT=1 opens the artifacts MCP session in print mode, and CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC,
# which closes that session, stays unset, so the session's POST /api/artifacts/mcp carries the fetch path's hello
# toward api.anthropic.com. The receiver answers the connector list with one invented connector, whose session then
# opens toward mcp-proxy.anthropic.com.
#
# The run sets CLAUDE_CODE_ENTRYPOINT=sdk-ts, the entry point of an Agent SDK session, since a plain print-mode run of
# 2.1.327 arms no artifact watch. The receiver answers the publish and the page read with invented bodies, and the
# watch then resolves api.anthropic.com itself ahead of its socket's dial, so the script binds a private hosts file
# over /etc/hosts that adds a 127.0.0.1 line for that name. The bind lands only in the capture's own mount namespace,
# and a run that stands in none, as under a caller's PHOEBIS_CAPTURE_NETNS=1, stops with one line ahead of it.
#
# Into <output-dir> it writes recording.json, the receiver's entry per request, hellos.json, the receiver's hello per
# connection, and feature_hellos.json, which joins the two: one entry per connection with the CONNECT authority, the
# hello as hex, and the first request's line and User-Agent. The four hellos the compares read go beside them as
# fetch.hex, axios.hex, connector.hex and sync.hex, the last from the connection whose first request is the artifact
# watch's socket, GET /api/frame/sync.
#
# The compares run through `python3 -I scripts/capture/hello_lib.py <first.hex> <second.hex>`, which masks the random,
# the session id, the server name and the key shares. The fetch path's hello against
# crates/profile/fixtures/hello_inference.hex and the axios path's against hello_control.hex are reported and decide
# nothing. The socket's hello is set against both pinned files, and the socket verdict line ahead of the last compare
# names the one it equals, and with it the connector the router's upstream leg of the socket takes, or says that it
# equals neither or was not recorded. It decides nothing for the exit either, so a build that dials no socket fails no
# caller. The last compare, the fetch path's hello against the one toward mcp-proxy.anthropic.com, gives the exit
# status: 0 where the two are equal under the masks, 3 where they differ, after the two masked hellos and the first part
# that differs, and 4 where one of the two was not recorded. Exit 1 is lib.sh's refusal, a run outside the capture's
# namespace or a step that failed. The log ends with one verdict line.
#
# Nothing here talks to Anthropic. The recordings hold each header the client sent, its dummy token among them, and
# stay out of the repository.

source "$(dirname "$0")/capture/lib.sh"

# PHOEBIS_CAPTURE_NETNS=1 in a caller's environment skips the namespace, so the variable proves none. unshare -r
# writes a map of one id, and only a mount namespace that such a user namespace owns takes the bind below.
mapfile -t id_map </proc/self/uid_map
read -r inside _ count <<<"${id_map[0]:-}"
if [[ "${#id_map[@]}" -ne 1 || "$inside" != 0 || "$count" != 1 ]]; then
  echo "$SCRIPT_NAME: no namespace of this capture stands, so no hosts file is bound" >&2
  exit 1
fi
# The client resolves api.anthropic.com itself ahead of its dial, under a proxy too, and loopback passes its guard.
hosts=$(mktemp)
{ cat /etc/hosts; echo "127.0.0.1 api.anthropic.com"; } >"$hosts"
# A refused bind ends the run, and its copy of the hosts file goes first.
if ! mount --bind "$hosts" /etc/hosts; then
  rm -f "$hosts"
  exit 1
fi
rm -f "$hosts"

CA="$OUT/certs/ca.pem"
FIXTURES="$ROOT/crates/profile/fixtures"
HELLO_LIB="$CAPTURE_DIR/hello_lib.py"
PAGE="$CLIENT_CWD/capture-page.html"
PROXY="http://127.0.0.1:$CONTROL_RECEIVER_PORT"

# As in scripts/capture-control.sh: the keys go once the receiver no longer presents the leaf, and the receiver's
# status is dropped, since a non-zero return inside the trap would skip the removal under set -e. The invented page
# goes too, since the other captures' clients run in the same working directory.
trap 'stop_receiver; stop_control_receiver || true; rm -f "$OUT"/certs/*.key "$PAGE"' EXIT

"$CAPTURE_DIR/mkcerts.sh" "$OUT"

# The receiver records every header, so the client gets an empty HOME in place of the operator's, and an earlier
# run's files in the same directory would read as this run's.
rm -rf "${STATE:?}/home"
mkdir -p "$STATE/home"
rm -f "$OUT"/{recording,hellos,feature_hellos}.json "$OUT"/{fetch,axios,connector,sync}.hex
# A print-mode run sends no Bash command, so the env file of a feature setup stays empty.
: >"$STATE/feature-env"
# The model receiver's canned tool call publishes this invented page. A print-mode run has no one to approve the
# publish, so --allowedTools lets the Artifact tool run.
printf '%s\n' '<!doctype html><title>Capture page</title><p>An invented page of the feature capture.</p>' >"$PAGE"
CLI_ARGS=(--allowedTools Artifact)

start_receiver count_receiver.py "$HTTP_PORT" "$OUT/inference" "$CLEAN_DURATION" "$PAGE" \
  Artifact "{\"file_path\": \"$PAGE\", \"icon\": \"page\"}"
start_control_receiver "$OUT/recording.json" "$CLEAN_DURATION" "$OUT/certs" "$FIXTURES/responses" "$OUT/hellos.json" \
  || { echo "$SCRIPT_NAME: the control receiver did not start" >&2; exit 1; }
ALLOW_NONESSENTIAL_TRAFFIC=1 runcli feature-hellos feature-hellos hi \
  ANTHROPIC_BASE_URL="http://127.0.0.1:$HTTP_PORT" CLAUDE_CODE_ARTIFACT=1 CLAUDE_CODE_ENTRYPOINT=sdk-ts \
  CLAUDE_CODE_GATEWAY_HINT_HEADERS=1 \
  CLAUDE_CODE_OAUTH_SCOPES="user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload" \
  CLAUDE_CODE_OAUTH_TOKEN="$DUMMY_OAUTH_TOKEN" CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1 \
  CLAUDE_CODE_SUBSCRIPTION_TYPE=max CLAUDE_ENV_FILE="$STATE/feature-env" DISABLE_TELEMETRY=1 HOME="$STATE/home" \
  HTTPS_PROXY="$PROXY" NODE_EXTRA_CA_CERTS="$CA" NO_PROXY=127.0.0.1,localhost,::1 \
  _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
stop_control_receiver || { echo "$SCRIPT_NAME: the control receiver failed" >&2; exit 1; }
stop_receiver

# Each connection's entry, in the order the client opened them, and the four hellos the compares read: the first
# connection of each kind, since the receiver sends no ticket a later one could resume.
python3 -I - "$OUT" <<'PY'
import json, pathlib, sys

out = pathlib.Path(sys.argv[1])
requests = {}
for entry in json.load(open(out / "recording.json")):
    if "request_line" in entry:
        requests.setdefault(entry["seq"], entry)
joined = []
for hello in sorted(json.load(open(out / "hellos.json")), key=lambda hello: hello["seq"]):
    request = requests.get(hello["seq"], {})
    lines = request.get("headers", [])
    agent = next((line.split(":", 1)[1].strip() for line in lines if line.lower().startswith("user-agent:")), None)
    joined.append({
        "authority": hello["target"],
        "hello": hello["hello"],
        "request_line": request.get("request_line"),
        "user_agent": agent,
    })
with open(out / "feature_hellos.json", "w") as f:
    json.dump(joined, f, indent=1)
for entry in joined:
    print(f"connection {entry['authority']}: {entry['request_line']}, User-Agent {entry['user_agent']}")


def first(authority, line):
    return next(
        (e for e in joined if e["authority"] == authority and (e["request_line"] or "").startswith(line)),
        None,
    )


picks = {
    "axios": first("api.anthropic.com:443", "GET /v1/mcp_servers"),
    "connector": first("mcp-proxy.anthropic.com:443", ""),
    "fetch": first("api.anthropic.com:443", "POST /api/artifacts/mcp "),
    "sync": first("api.anthropic.com:443", ("GET /api/frame/sync ", "GET /api/frame/sync?")),
}
for name, entry in picks.items():
    if entry is not None:
        (out / f"{name}.hex").write_text(entry["hello"] + "\n")
PY

# $1 names the compare, $2 and $3 are the two hex files. Prints the masked hellos of a difference and then the
# compare's own line, keeps that line in VERDICT and answers the compare's status.
compare() {
  local status=0 out
  out=$(python3 -I "$HELLO_LIB" "$2" "$3" 2>&1) || status=$?
  if [[ "$status" -ne 0 && "$status" -ne 3 ]]; then
    echo "$SCRIPT_NAME: the compare of $1 failed: $out" >&2
    exit 1
  fi
  sed '$d; s/^/  /' <<<"$out"
  VERDICT=$(tail -n 1 <<<"$out")
  echo "$1: $VERDICT"
  return "$status"
}

# $1 names the hello, $2 is its hex file and $3 the pinned fixture it is set against. Answers the compare's status,
# or 4 where the hello was not recorded.
against_pinned() {
  if [[ -e "$2" ]]; then
    compare "$1 with $3" "$2" "$FIXTURES/$3"
  else
    echo "$1 with $3: not recorded"
    return 4
  fi
}

against_pinned "the fetch path's hello" "$OUT/fetch.hex" hello_inference.hex || true
against_pinned "the axios path's hello" "$OUT/axios.hex" hello_control.hex || true
# The pinned file the socket's hello equals names the connector its upstream leg takes. The two pinned hellos differ
# at the cipher list, so at most one of them can be equal.
socket="equals neither pinned hello"
[[ -e "$OUT/sync.hex" ]] || socket="was not recorded"
against_pinned "the socket's hello" "$OUT/sync.hex" hello_control.hex \
  && socket="equals hello_control.hex under the masks"
against_pinned "the socket's hello" "$OUT/sync.hex" hello_inference.hex \
  && socket="equals hello_inference.hex under the masks"
echo "socket verdict: the socket's hello $socket"

if [[ ! -e "$OUT/fetch.hex" || ! -e "$OUT/connector.hex" ]]; then
  [[ -e "$OUT/fetch.hex" ]] || echo "not recorded: the fetch path's hello toward api.anthropic.com"
  [[ -e "$OUT/connector.hex" ]] || echo "not recorded: the hello toward mcp-proxy.anthropic.com"
  echo "verdict: one of the two hellos the compare reads was not recorded"
  exit 4
fi
status=0
compare "the fetch path's hello toward api.anthropic.com with the one toward mcp-proxy.anthropic.com" \
  "$OUT/fetch.hex" "$OUT/connector.hex" || status=$?
if [[ "$status" -eq 0 ]]; then
  echo "verdict: the hello toward mcp-proxy.anthropic.com equals the fetch path's toward api.anthropic.com" \
    "under the masks"
else
  echo "verdict: the fetch path's hello toward api.anthropic.com (first) and the one toward" \
    "mcp-proxy.anthropic.com (second) ${VERDICT/differs/differ}"
fi
exit "$status"
