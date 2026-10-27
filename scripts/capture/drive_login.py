# Runs a client's /login and /status in a pseudo-terminal with no human at it, so the control capture records the
# code grant and the reads that a login and /status make. The capture's receiver answers the token exchange with the
# canned token whatever code it gets, so the driver pastes a dummy code and the login completes.
#
# Usage: drive_login.py --binary <path> --cwd <dir> --out <dir> --method claude|console [--timeout <seconds>]
#
# The client inherits the driver's environment, which the capture sets to the loopback proxy, the run's CA and a
# throwaway config directory, so the client's calls reach the loopback receiver alone. The capture also passes a
# throwaway HOME, TERM and LANG, and BROWSER=true, which has the login show its paste form at once. The driver opens
# no socket and sends the client nothing but keystrokes. It writes the authorize URL to <out>/login-<method>-url.txt
# and every byte the client printed to <out>/login-<method>-screen.txt, so a screen it failed to recognize is readable
# afterwards. Both files are raw capture output and stay out of the repository. A wait longer than --timeout, a client
# that exits while the driver still waits on it, or a login that ends on the client's error screen exits 2 once the
# screen is on disk, which a capture logs and moves past. A run that gets through /exit exits 0.
import argparse, fcntl, os, pty, re, select, signal, struct, sys, termios, time, urllib.parse

# A step's text can arrive in several chunks and the client redraws its screen between steps, so a wait acts on what
# it read only after this long without a new byte.
QUIET = 1.0
# A client in raw mode can read a chunk of several keys as a paste, which takes a return inside it as text, so each
# key goes in a write of its own once the client has had this long to draw the one before.
SETTLE = 0.3
# The client wraps text at the terminal's width, and the authorize URL runs to several hundred characters, so a wide
# terminal keeps it on one line for the match.
COLUMNS, ROWS = 500, 50

DOWN, ESC = b"\x1b[B", b"\x1b"
ESCAPES = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[@-Z\\-_]")
# A row's lead: blanks, among them the no-break space a client may draw, and the border of a boxed row.
LEAD = r"^[ \t\xa0│┃|]*"
# The /login menu lists the subscription first and the Console second, and the Console entry opens a second menu whose
# first entry, the Console sign-in, holds the cursor.
METHOD_KEYS = {"claude": [], "console": [DOWN]}
# The input marker at a line start, behind the border of a boxed input line, and never a menu's selected entry.
PROMPT = re.compile(LEAD + r"[>❯](?:[ \xa0]|$)(?![ \xa0]*\d+\.)", re.M)
URL = re.compile(r"https://\S+/oauth/authorize\S*")


class Exited(Exception):
    pass


class Refused(Exception):
    pass


class Timeout(Exception):
    pass


class Parser(argparse.ArgumentParser):
    # A capture reads exit 2 as a timeout and moves on, so a usage error exits 1 and stops it instead.
    def error(self, message):
        self.print_usage(sys.stderr)
        self.exit(1, f"{self.prog}: error: {message}\n")


class Terminal:
    def __init__(self, fd, screen, timeout):
        self.fd, self.screen, self.timeout = fd, screen, timeout
        self.read = bytearray()
        self.mark = 0
        self.last = time.monotonic()

    def pump(self, seconds):
        """Reads what the client prints within `seconds` into the screen file, and raises Exited once the client's
        side of the terminal has closed."""
        ready, _, _ = select.select([self.fd], [], [], seconds)
        if not ready:
            return
        try:
            chunk = os.read(self.fd, 65536)
        except OSError:
            # Linux answers EIO here once the client's side has closed.
            chunk = b""
        if not chunk:
            raise Exited
        self.read += chunk
        self.screen.write(chunk)
        self.screen.flush()
        self.last = time.monotonic()

    def text(self):
        """What the client printed since the last return the driver sent, without escape sequences."""
        text = ESCAPES.sub("", self.read[self.mark:].decode("utf-8", "replace"))
        return text.replace("\r\n", "\n").replace("\r", "\n")

    def wait(self, what, found):
        """Returns what found() makes of the text once the screen has been quiet for QUIET seconds and found() returns
        a value, or raises Timeout after --timeout seconds."""
        deadline = time.monotonic() + self.timeout
        while True:
            now = time.monotonic()
            if now - self.last >= QUIET:
                result = found(self.text())
                if result:
                    return result
            if now >= deadline:
                raise Timeout(what)
            try:
                self.pump(0.1)
            except Exited:
                raise Exited(what) from None

    def wait_exit(self):
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            try:
                self.pump(0.1)
            except Exited:
                return
        raise Timeout("the client to exit after /exit")

    def settle(self, key):
        end = time.monotonic() + SETTLE
        while (left := end - time.monotonic()) > 0:
            try:
                self.pump(left)
            except Exited:
                raise Exited(f"the client to draw {key!r}") from None

    def press(self, *keys):
        """Sends each key, then a return, and starts the next wait's text at the return."""
        for key in keys:
            os.write(self.fd, key)
            self.settle(key)
        self.mark = len(self.read)
        os.write(self.fd, b"\r")

    def escape(self):
        """Sends Esc alone and starts the next wait's text at it. The client gets SETTLE seconds to take it, since a key
        right behind a lone Esc can read as that key with Alt, and the driver may stop the client next."""
        self.mark = len(self.read)
        os.write(self.fd, ESC)
        self.settle(ESC)


def prompt(text):
    return PROMPT.search(text) is not None


def authorize_url(text):
    match = URL.search(text)
    return match.group(0) if match else None


def login_menu(text):
    """("url", the URL) once the authorize URL shows, as it does at once where a setting presets the login method,
    ("menu", None) on the method menu, or None while neither shows."""
    url = authorize_url(text)
    if url:
        return "url", url
    return ("menu", None) if "Select login method:" in text else None


def showing(*texts):
    return lambda text: any(t in text for t in texts)


def login_outcome(text):
    if "Press Enter to continue" in text:
        return "success"
    if "Press Enter to retry" in text or "Invalid code" in text:
        return "refused"
    return "prompt" if prompt(text) else None


def drive(terminal, method, url_path):
    terminal.wait("the first prompt", prompt)
    terminal.press(b"/login")
    kind, url = terminal.wait("the login menu or the authorize URL", login_menu)
    if kind == "menu":
        terminal.press(*METHOD_KEYS[method])
        if method == "console":
            terminal.wait("the Console sign-in menu", showing("How do you want to sign in?"))
            terminal.press()
        url = terminal.wait("the authorize URL", authorize_url)
    with open(url_path, "w") as f:
        f.write(url + "\n")
    state = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).get("state", [""])[0]
    terminal.press(f"DUMMYCODE#{state}".encode() if state else b"DUMMYCODE")
    outcome = terminal.wait("the login's outcome", login_outcome)
    if outcome == "refused":
        # A retry would paste the same dummy code and fail the same way, so the driver closes the dialog and gives up.
        # Where the token exchange is what failed, the recording already holds the code grant.
        terminal.escape()
        raise Refused
    if outcome == "success":
        # The success screen holds the dialog open until a return.
        terminal.press()
        terminal.wait("the prompt after the login", prompt)
    terminal.press(b"/status")
    # /status opens the settings dialog, which only a key closes.
    terminal.wait("the status dialog", showing("Settings", "Esc to cancel"))
    terminal.escape()
    terminal.wait("the prompt after /status", prompt)
    terminal.press(b"/exit")
    terminal.wait_exit()


def stop(pid, grace):
    """Reaps the client, given `grace` seconds to exit on its own before SIGTERM and then SIGKILL, so no run leaves
    a client behind."""
    for sig in (signal.SIGTERM, signal.SIGKILL, None):
        end = time.monotonic() + grace
        while True:
            if os.waitpid(pid, os.WNOHANG)[0]:
                return
            if time.monotonic() >= end:
                break
            time.sleep(0.05)
        if sig is None:
            os.waitpid(pid, 0)
            return
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            pass
        grace = 2


def main():
    parser = Parser(description="Drive a client's /login and /status in a pseudo-terminal.")
    parser.add_argument("--binary", required=True)
    parser.add_argument("--cwd", required=True)
    parser.add_argument("--method", required=True, choices=("claude", "console"))
    parser.add_argument("--out", required=True)
    parser.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()
    # The client starts in --cwd, where a relative path given here would no longer resolve.
    binary = os.path.abspath(args.binary)
    if not os.access(binary, os.X_OK):
        parser.error(f"{binary} is not an executable file")
    if not os.path.isdir(args.cwd):
        parser.error(f"{args.cwd} is not a directory")
    os.makedirs(args.out, exist_ok=True)
    url_path = os.path.join(args.out, f"login-{args.method}-url.txt")
    screen_path = os.path.join(args.out, f"login-{args.method}-screen.txt")

    pid, fd = pty.fork()
    if pid == 0:
        try:
            fcntl.ioctl(1, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))
            os.chdir(args.cwd)
            os.execv(binary, [binary])
        except OSError as e:
            os.write(2, f"drive_login.py: cannot start {binary}: {e}\n".encode())
        os._exit(127)

    failure = None
    with open(screen_path, "wb") as screen:
        try:
            drive(Terminal(fd, screen, args.timeout), args.method, url_path)
        except Timeout as e:
            failure = f"no sign of {e} within {args.timeout:g} s"
        except Exited as e:
            failure = f"the client exited while the driver waited for {e}"
        except Refused:
            failure = "the client answered the pasted code with an error"
        finally:
            stop(pid, 0 if failure else 2)
            os.close(fd)
    if failure:
        print(f"drive_login.py: {failure}, screen in {screen_path}", file=sys.stderr)
        sys.exit(2)


main()
