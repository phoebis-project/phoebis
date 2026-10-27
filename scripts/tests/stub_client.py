#!/usr/bin/env -S python3 -I
# Stands in for the client binary in scripts/tests/capture-tooling.sh, so the tests drive the login driver and the
# control capture without the real client. It makes the control calls a capture records. In print mode (-p among
# its arguments) it sends the refresh grant when the credentials file holds an expired token, then the profile read
# and the inference post, and exits 1 as the client does against the canned 400. In terminal mode it draws the
# client's screens as text. It answers /login with the method menu's three entries, the Console sign-in menu after a
# down arrow (its header repainted over the method menu's the way the client repaints a line), the paste form with
# the authorize URL, then the code grant, the profile and roles reads and the success screen, and after a return a
# credentials file holding the token the grant returned. It answers /status with the settings dialog and the usage
# read after Esc, and /exit by exiting 0.
#
# It reaches no host but loopback. Every https call goes through a CONNECT tunnel opened to HTTPS_PROXY whatever
# NO_PROXY says, so the stub never resolves or dials a control host itself, where urllib's proxy handler would honor
# NO_PROXY and dial the real host. Print mode's inference post to an http base URL goes to HTTPS_PROXY as an
# absolute-URI request, as the client sends every request, unless NO_PROXY names the base URL's host (an exact entry or
# *), and then straight to that host. That post is the one place the stub reads NO_PROXY, and since the proxy and the
# base URL's host are both checked to sit on loopback, it only picks between two loopback listeners. Before any socket
# opens it refuses a proxy off loopback, a run without CLAUDE_CONFIG_DIR, where the client's default would be the
# user's own config directory, and in print mode an http base URL off loopback. The tokens it sends are the dummy ones
# the tests seed and the canned ones the receiver answers with.
#
# Every request carries x-stub-browser, x-stub-home, x-stub-lang, x-stub-nonessential and x-stub-term with the values
# of BROWSER, HOME, LANG, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC and TERM, or "unset" for a variable the run lacks,
# so a recording shows which environment its run had. STUB_HANG=1 makes terminal mode print its prompt and then
# nothing, as a client stuck on a screen the driver does not know. STUB_REJECT_CODE=1 answers every pasted code with
# the invalid-code screen the client shows for a code without its state.
import http.client, json, os, pathlib, secrets, signal, ssl, sys, termios, time, tty, urllib.parse

AUTHORIZE_URL = "https://claude.com/cai/oauth/authorize"
CONSOLE_AUTHORIZE_URL = "https://platform.claude.com/oauth/authorize"
PROFILE_URL = "https://api.anthropic.com/api/oauth/profile"
REDIRECT_URI = "https://console.anthropic.com/oauth/code/callback"
ROLES_URL = "https://api.anthropic.com/api/oauth/claude_cli/roles"
TOKEN_URL = "https://platform.claude.com/v1/oauth/token"
USAGE_URL = "https://api.anthropic.com/api/oauth/usage"

LOOPBACK = {"127.0.0.1", "::1", "localhost"}
RECORDED = {
    "x-stub-browser": os.environ.get("BROWSER", "unset"),
    "x-stub-home": os.environ.get("HOME", "unset"),
    "x-stub-lang": os.environ.get("LANG", "unset"),
    "x-stub-nonessential": os.environ.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "unset"),
    "x-stub-term": os.environ.get("TERM", "unset"),
}
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


def bypasses_proxy(host):
    """Whether NO_PROXY names `host` in one of its comma-separated entries, matched exactly, or holds *."""
    entries = {entry.strip() for entry in os.environ.get("NO_PROXY", "").split(",")}
    return "*" in entries or host in entries


def request(method, url, body=None, token=None):
    """Sends one request and returns its status and body. An https URL goes through the proxy's tunnel. An http one,
    the base URL checked to sit on loopback, goes to the proxy as an absolute-URI request, as the client sends it, or
    straight to its host where NO_PROXY names that host."""
    parts = urllib.parse.urlsplit(url)
    target = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")
    if parts.scheme == "https":
        conn = http.client.HTTPSConnection(PROXY_HOST, PROXY_PORT, timeout=TIMEOUT, context=CONTEXT)
        conn.set_tunnel(parts.hostname, parts.port or 443)
    elif bypasses_proxy(parts.hostname):
        conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=TIMEOUT)
    else:
        conn = http.client.HTTPConnection(PROXY_HOST, PROXY_PORT, timeout=TIMEOUT)
        target = f"http://{parts.netloc}{target}"
    headers = {"Accept": "application/json", **RECORDED}
    payload = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        payload = json.dumps(body, separators=(",", ":")).encode()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    try:
        conn.request(method, target, body=payload, headers=headers)
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


def wait_for_escape():
    """Reads keys until Esc, the key that closes the client's dialogs. The terminal's line mode would hold a lone Esc
    back until a return, so the read runs in cbreak mode."""
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    # TCSANOW keeps a key that arrived before the switch, which tty's default TCSAFLUSH would drop.
    tty.setcbreak(fd, termios.TCSANOW)
    try:
        while os.read(fd, 1) not in (b"\x1b", b""):
            pass
    finally:
        termios.tcsetattr(fd, termios.TCSANOW, saved)


def repaint(old, new):
    """What the client's renderer sends to turn a line holding `old` into `new` from the line's start: each run of
    cells that already holds its character skipped with a cursor-forward, then the rest of the line erased."""
    out, skip = "", 0
    for i, char in enumerate(new):
        if i < len(old) and old[i] == char:
            skip += 1
            continue
        out += (f"\x1b[{skip}C" if skip else "") + char
        skip = 0
    return out + (f"\x1b[{skip}C" if skip else "") + "\x1b[K"


def login_command():
    say("Select login method:")
    say("> 1. Claude account with subscription · Pro, Max, Team, or Enterprise")
    say("  2. Anthropic Console account · API usage billing")
    say("  3. 3rd-party platform · Amazon Bedrock, Microsoft Foundry, Google Vertex AI")
    authorize = AUTHORIZE_URL
    # The line mode hands a down arrow over with the return that follows it.
    if "\x1b[B" in sys.stdin.readline():
        # A return sent before the second menu shows can land on the client's first menu, so the stub drops one sent
        # that early and the driver's run times out.
        termios.tcflush(sys.stdin.fileno(), termios.TCIFLUSH)
        # The client draws the second menu's header over the first's, rewriting only the cells that change, so only a
        # reader that keeps a screen grid sees it in full. Its row is five up, past the line the terminal echoed for
        # the arrow and the return and the menu's three entries.
        say(f"\x1b[5A\r{repaint('Select login method:', 'How do you want to sign in?')}\r\x1b[5B", end="")
        say("> 1. Sign in with your Console account (recommended)")
        say("  2. Create an API key (legacy)")
        sys.stdin.readline()
        authorize = CONSOLE_AUTHORIZE_URL
    state = secrets.token_urlsafe(32)
    url = f"{authorize}?code=true&client_id=stub&state={state}"
    say("Browser didn't open? Use the url below to sign in")
    # The client prints the URL as an OSC 8 hyperlink whose text is the URL, which the driver has to read through.
    say(f"\x1b]8;;{url}\x07{url}\x1b]8;;\x07")
    say("Paste code here if prompted > ", end="")
    code, _, pasted_state = sys.stdin.readline().strip().partition("#")
    if not code or not pasted_state or os.environ.get("STUB_REJECT_CODE") == "1":
        say("Invalid code. Please make sure the full code was copied")
        say("Press Enter to retry.")
        wait_for_escape()
        return
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
    say("Logged in as stub@example.invalid")
    say("Login successful. Press Enter to continue")
    sys.stdin.readline()
    oauth = {
        "accessToken": answer["access_token"],
        "refreshToken": answer.get("refresh_token"),
        "expiresAt": int(time.time() * 1000) + 8 * 3600 * 1000,
        "scopes": ["user:profile"],
    }
    CONFIG.mkdir(parents=True, exist_ok=True)
    (CONFIG / ".credentials.json").write_text(json.dumps({"claudeAiOauth": oauth}, separators=(",", ":")))


def status_command():
    say("Settings  Status  Config  Usage  Stats")
    say("Version stub")
    say("Esc to cancel")
    wait_for_escape()
    request("GET", USAGE_URL, token=stored_oauth().get("accessToken"))


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
# Python 3.13's default context verifies under OpenSSL's strict flags and none is cleared here, so on 3.13 and later a
# handshake that passes shows the run's certificates well-formed for a strict client too.
CONTEXT = ssl.create_default_context(cafile=os.environ.get("NODE_EXTRA_CA_CERTS"))

if "-p" in sys.argv[1:]:
    print_mode()
terminal_mode()
