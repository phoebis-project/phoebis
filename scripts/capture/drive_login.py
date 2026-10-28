# Runs a client's /login, /status and /usage in a pseudo-terminal with no human at it, so the control capture records
# the code grant and the reads that a login, /status and /usage make. The capture's receiver answers the token exchange
# with the canned token whatever code it gets, so the driver pastes a dummy code and the login completes.
#
# Usage: drive_login.py --binary <path> --cwd <dir> --out <dir> --method claude|console [--timeout <seconds>]
#        drive_login.py --self-test
#
# --self-test checks the screen grid against fixed escape sequences, with no client and no terminal.
#
# The client inherits the driver's environment, which the capture sets to the loopback proxy, the run's CA and a
# throwaway config directory, so the client's calls reach the loopback receiver alone. The capture also passes a
# throwaway HOME, LANG and TERM, and BROWSER=true, which has the login show its paste form at once. The driver opens
# no socket and sends the client nothing but keystrokes. It writes the authorize URL to <out>/login-<method>-url.txt
# and every byte the client printed to <out>/login-<method>-screen.txt, so a screen it failed to recognize is readable
# afterwards. Both files are raw capture output and stay out of the repository. A wait longer than --timeout, a client
# that exits while the driver still waits on it, or a login that ends on the client's error screen exits 2 once the
# screen is on disk, which a capture logs and moves past. A run that gets through /exit exits 0.
import argparse, codecs, fcntl, io, os, pty, re, select, signal, struct, sys, termios, time, urllib.parse

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
# A row's lead: blanks, among them the no-break space a client may draw, and the border of a boxed row.
LEAD = r"^[ \t\xa0│┃|]*"
# The /login menu lists three entries, the subscription first, the Console second and the third-party platforms third,
# and the Console entry opens a second menu whose first entry, the Console sign-in, holds the cursor.
METHOD_KEYS = {"claude": [], "console": [DOWN]}
# The input marker at a line start, behind the border of a boxed input line, and never a menu's selected entry.
PROMPT = re.compile(LEAD + r"[>❯](?:[ \xa0]|$)(?![ \xa0]*\d+\.)", re.M)
# The escape sequence at an ESC, in order: a CSI with its parameters, intermediates and final, an OSC or another string
# up to its BEL or ESC \ terminator, the start of either that the chunk cut off (`partial`), or a short escape such as
# ESC 7 or ESC ( B. An ESC that starts none of them matches alone.
SEQUENCE = re.compile(
    r"\x1b(?:\[(?P<params>[0-?]*)(?P<inter>[ -/]*)(?P<final>[@-~])"
    r"|[\]PX^_].*?(?:\x07|\x1b\\)"
    r"|(?P<partial>(?:\[[0-?]*[ -/]*|[\]PX^_].*|[ -/]*)\Z)"
    r"|[ -/]*[0-~])?",
    re.S,
)
# Printable ASCII alone, so a box-drawing border the dialog draws right after the URL never joins it.
URL = re.compile(r"https://[!-~]+/oauth/authorize[!-~]*")


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
    """The client's side of the pseudo-terminal, read onto a grid of cells. The client's renderer repaints a changed
    line cell by cell and moves the cursor past each cell that already holds its character, so only the grid shows
    such a line in full."""

    def __init__(self, fd, screen, timeout):
        self.fd, self.screen, self.timeout = fd, screen, timeout
        self.read = bytearray()
        self.mark = 0
        self.last = time.monotonic()
        # A line per row and a character per cell, both counted from 0, and per row the length of `read` at its last
        # write or erase, which text() compares with the mark.
        self.lines, self.stamps = [], []
        self.row = self.column = 0
        self.saved = (0, 0)
        # A chunk can end inside a UTF-8 character or an escape sequence, whose rest comes with the next chunk.
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.pending = ""

    def pump(self, seconds):
        """Reads what the client prints within `seconds` onto the grid and into the screen file, and raises Exited
        once the client's side of the terminal has closed."""
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
        self.take(chunk)
        self.last = time.monotonic()

    def take(self, chunk):
        self.read += chunk
        self.screen.write(chunk)
        self.screen.flush()
        self.draw(self.decoder.decode(chunk))

    def draw(self, text):
        text, self.pending = self.pending + text, ""
        i = 0
        while i < len(text):
            char = text[i]
            if char == "\x1b":
                match = SEQUENCE.match(text, i)
                if match["partial"] is not None:
                    self.pending = text[i:]
                    return
                # Private CSIs set modes or speak the keyboard protocol, whose CSI < u and CSI ? u look like a cursor
                # restore, and CSIs with intermediates scroll or set the cursor's shape. None of them changes the grid.
                if match["final"] and not match["inter"] and not match["params"].startswith(("<", "=", ">", "?")):
                    self.csi(match["params"], match["final"])
                i = match.end()
                continue
            if char == "\r":
                self.column = 0
            elif char == "\n":
                self.row += 1
            elif char == "\b":
                self.column = max(self.column - 1, 0)
            elif char == "\t":
                self.column = self.column // 8 * 8 + 8
            elif char >= " " and not "\x7f" <= char <= "\x9f":
                self.put(char)
            i += 1

    def csi(self, params, final):
        args = [int(p) if p.isdigit() else 0 for p in params.split(";")]
        # A terminal reads a count or a position of 0 as 1.
        n = max(args[0], 1)
        if final == "A":
            self.row = max(self.row - n, 0)
        elif final == "B":
            self.row += n
        elif final == "C":
            self.column += n
        elif final == "D":
            self.column = max(self.column - n, 0)
        elif final == "G":
            self.column = n - 1
        elif final in ("H", "f"):
            self.row, self.column = n - 1, max(args[1] if len(args) > 1 else 0, 1) - 1
        elif final == "J" and args[0] in (0, 1, 2, 3):
            self.erase_screen(args[0])
        elif final == "K" and args[0] in (0, 1, 2):
            self.erase_line(args[0])
        elif final == "s":
            self.saved = self.row, self.column
        elif final == "u":
            self.row, self.column = self.saved

    def line(self):
        """The cursor's line, stamped as changed, with the lines up to it made where the cursor sits below the last."""
        while len(self.lines) <= self.row:
            self.lines.append([])
            self.stamps.append(0)
        self.stamps[self.row] = len(self.read)
        return self.lines[self.row]

    def put(self, char):
        line = self.line()
        line.extend(" " * (self.column + 1 - len(line)))
        line[self.column] = char
        self.column += 1

    def erase_line(self, mode):
        """Erases from the cursor to the line's end (0), from its start to the cursor (1), or the whole line (2)."""
        line = self.line()
        if mode == 0:
            del line[self.column :]
        elif mode == 1:
            line[: self.column + 1] = " " * min(len(line), self.column + 1)
        else:
            line.clear()

    def erase_screen(self, mode):
        """Erases from the cursor to the screen's end (0), from its start to the cursor (1), or all of it (2, 3)."""
        if mode == 0:
            self.erase_line(0)
            rows = range(self.row + 1, len(self.lines))
        elif mode == 1:
            self.erase_line(1)
            rows = range(self.row)
        else:
            rows = range(len(self.lines))
        for row in rows:
            self.lines[row] = []
            self.stamps[row] = len(self.read)

    def text(self):
        """The lines the client wrote or erased since the last key the driver sent, as the grid shows them."""
        lines = zip(self.lines, self.stamps)
        return "\n".join("".join(line).rstrip(" ") for line, stamp in lines if stamp > self.mark)

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
    terminal.press(b"/usage")
    # The match needs only the "Esc to cancel" footer the dialog draws under its loading row in both variants, so the
    # driver closes the dialog whether the read then fills the two bars or the nonessential-traffic switch refuses it
    # with an error row. It waits for neither of those, as QUIET seconds without a byte cover a loopback read before
    # the Escape.
    terminal.wait("the usage dialog", showing("Esc to"))
    terminal.escape()
    terminal.wait("the prompt after /usage", prompt)
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


def self_test():
    """Runs the grid against one sequence per rule the client's screens lean on. A None among a case's chunks sets the
    mark as a key the driver sends does."""
    url = "https://claude.com/cai/oauth/authorize?code=true&state=abc"
    old, new = "Select login method:", "How do you want to sign in?"
    cases = [
        ("a line repainted cell by cell reads in full", [f"{old}\x1b[1;1H{repaint(old, new)}".encode()], new),
        (
            "an erased line holds the shorter line written over it alone",
            [b"Opening browser to sign in...\x1b[2K\rPaste code here >"],
            "Paste code here >",
        ),
        ("a hyperlink reads as its URL", [f"\x1b]8;;{url}\x07{url}\x1b]8;;\x1b\\".encode()], url),
        ("the text holds the lines written after the mark alone", [b"one\r\ntwo\r\n", None, b"three"], "three"),
        ("an escape split across two chunks is one sequence", [b"stale\x1b[", b"2K\rfresh"], "fresh"),
    ]
    for name, chunks, want in cases:
        terminal = Terminal(None, io.BytesIO(), 0)
        for chunk in chunks:
            if chunk is None:
                terminal.mark = len(terminal.read)
            else:
                terminal.take(chunk)
        got = terminal.text()
        if got != want:
            print(f"drive_login.py: self-test failed: {name}: the text reads {got!r}, want {want!r}", file=sys.stderr)
            return 1
        print(f"self-test passed: {name}")
    return 0


def main():
    # The self-test starts no client, so it needs none of the arguments a run does.
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    parser = Parser(description="Drive a client's /login, /status and /usage in a pseudo-terminal.")
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
