#!/usr/bin/env python3
# Stands in for the client binary in scripts/tests/capture-tooling.sh, so the tests drive the login driver and the
# control capture without the real client. It makes the control calls a capture records. In print mode (-p among
# its arguments) it sends the refresh grant when the credentials file holds an expired token, then the profile read
# and the inference post, and exits 1 as the client does against the canned 400. In terminal mode it answers /login
# with a menu, the authorize URL, the code grant, the profile and roles reads and a credentials file holding the
# token the grant returned, /status with the usage read, and /exit by exiting 0.
#
# It reaches no host but loopback. Every https call goes through a CONNECT tunnel opened to HTTPS_PROXY, so the stub
# never resolves or dials a control host itself, and it reads no NO_PROXY, which urllib's proxy handler would honor
# and then dial the real host. Before any socket opens it refuses a proxy off loopback, a run without
# CLAUDE_CONFIG_DIR, where the client's default would be the user's own config directory, and in print mode an http
# base URL off loopback. The tokens it sends are the dummy ones the tests seed and the canned ones the receiver
# answers with.
#
# Every request carries x-stub-nonessential with the value of CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC, so a
# recording shows which environment its run had. STUB_HANG=1 makes terminal mode print its prompt and then nothing,
# as a client stuck on a screen the driver does not know.
import http.client, json, os, pathlib, secrets, signal, ssl, sys, time, urllib.parse

AUTHORIZE_URL = "https://claude.com/cai/oauth/authorize"
PROFILE_URL = "https://api.anthropic.com/api/oauth/profile"
REDIRECT_URI = "https://console.anthropic.com/oauth/code/callback"
ROLES_URL = "https://api.anthropic.com/api/oauth/claude_cli/roles"
TOKEN_URL = "https://platform.claude.com/v1/oauth/token"
USAGE_URL = "https://api.anthropic.com/api/oauth/usage"

LOOPBACK = {"127.0.0.1", "::1", "localhost"}
NONESSENTIAL = os.environ.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "unset")
TIMEOUT = 10


def refuse(message):
    print(f"stub_client: {message}", file=sys.stderr)
    sys.exit(3)


def split(url):
    """The URL's scheme, host and port, with a host of None where the URL does not parse."""
    try:
        parts = urllib.parse.urlsplit(url)
        return parts.scheme, parts.hostname, parts.port
    except ValueError:
        return None, None, None


def request(method, url, body=None, token=None):
    """Sends one request and returns its status and body. An https URL goes through the proxy's tunnel, and an http
    one, the base URL checked to sit on loopback, straight to its host."""
    parts = urllib.parse.urlsplit(url)
    if parts.scheme == "https":
        conn = http.client.HTTPSConnection(PROXY_HOST, PROXY_PORT, timeout=TIMEOUT, context=CONTEXT)
        conn.set_tunnel(parts.hostname, parts.port or 443)
    else:
        conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=TIMEOUT)
    headers = {"Accept": "application/json", "x-stub-nonessential": NONESSENTIAL}
    payload = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        payload = json.dumps(body, separators=(",", ":")).encode()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    path = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")
    try:
        conn.request(method, path, body=payload, headers=headers)
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def say(text, end="\n"):
    print(text, end=end, flush=True)


def stored_oauth():
    credentials = CONFIG / ".credentials.json"
    return json.loads(credentials.read_text()).get("claudeAiOauth", {}) if credentials.exists() else {}


def print_mode():
    base = os.environ.get("ANTHROPIC_BASE_URL") or "https://api.anthropic.com"
    scheme, host, _ = split(base)
    if scheme != "https" and not (scheme == "http" and host in LOOPBACK):
        refuse(f"refusing a base URL off loopback: {base}")
    oauth = stored_oauth()
    token = oauth.get("accessToken")
    expires = oauth.get("expiresAt")
    try:
        if isinstance(expires, (int, float)) and expires < time.time() * 1000:
            grant = {"grant_type": "refresh_token", "refresh_token": oauth.get("refreshToken"), "client_id": "stub"}
            status, data = request("POST", TOKEN_URL, grant)
            if status == 200:
                token = json.loads(data).get("access_token", token)
        request("GET", PROFILE_URL, token=token)
        status, _ = request("POST", base.rstrip("/") + "/v1/messages", {"model": "stub", "messages": []}, token)
    except (OSError, http.client.HTTPException) as e:
        print(f"stub_client: {e!r}", file=sys.stderr)
        sys.exit(1)
    say(f"API Error: {status}")
    sys.exit(1)


def login_command():
    say("> 1. Claude account with subscription")
    say("  2. Anthropic Console account")
    sys.stdin.readline()
    state = secrets.token_urlsafe(32)
    say(f"{AUTHORIZE_URL}?code=true&client_id=stub&state={state}")
    code, _, pasted_state = sys.stdin.readline().strip().partition("#")
    grant = {
        "grant_type": "authorization_code",
        "code": code,
        "state": pasted_state,
        "code_verifier": secrets.token_urlsafe(32),
        "redirect_uri": REDIRECT_URI,
        "client_id": "stub",
    }
    status, data = request("POST", TOKEN_URL, grant)
    answer = json.loads(data) if status == 200 else {}
    if "access_token" not in answer:
        say(f"Login failed: {status}")
        return
    request("GET", PROFILE_URL, token=answer["access_token"])
    request("GET", ROLES_URL, token=answer["access_token"])
    oauth = {
        "accessToken": answer["access_token"],
        "refreshToken": answer.get("refresh_token"),
        "expiresAt": int(time.time() * 1000) + 8 * 3600 * 1000,
        "scopes": ["user:profile"],
    }
    CONFIG.mkdir(parents=True, exist_ok=True)
    (CONFIG / ".credentials.json").write_text(json.dumps({"claudeAiOauth": oauth}, separators=(",", ":")))
    say("Logged in")


def status_command():
    request("GET", USAGE_URL, token=stored_oauth().get("accessToken"))
    say("Usage ok")


def terminal_mode():
    say("> ", end="")
    if os.environ.get("STUB_HANG") == "1":
        while True:
            signal.pause()
    commands = {"/login": login_command, "/status": status_command}
    while line := sys.stdin.readline():
        command = line.strip()
        if command == "/exit":
            sys.exit(0)
        try:
            commands.get(command, lambda: say(f"Unknown command: {command}"))()
        except (OSError, http.client.HTTPException) as e:
            say(f"Request failed: {e!r}")
        say("> ", end="")


_, PROXY_HOST, PROXY_PORT = split(os.environ.get("HTTPS_PROXY", ""))
if PROXY_HOST not in LOOPBACK:
    refuse(f"refusing a proxy off loopback: {os.environ.get('HTTPS_PROXY', '')}")
PROXY_PORT = PROXY_PORT or 80
if not os.environ.get("CLAUDE_CONFIG_DIR"):
    refuse("refusing to run without CLAUDE_CONFIG_DIR")
CONFIG = pathlib.Path(os.environ["CLAUDE_CONFIG_DIR"])
# Python's default context verifies under OpenSSL's strict flags and none is cleared here, so a handshake that passes
# shows the run's certificates well-formed for a strict client too.
CONTEXT = ssl.create_default_context(cafile=os.environ.get("NODE_EXTRA_CA_CERTS"))

if "-p" in sys.argv[1:]:
    print_mode()
terminal_mode()
