#!/usr/bin/env python3
"""Read-only checks over the documentation site.

`links` walks the built site for an internal link or image that resolves to nothing; `routes` compares the admin
route table with the entries of the API reference page, so a route added or removed in the router fails the build
until the page follows. `client` does the same for the proxy listener against the table on the client page.
"""
import html.parser
import pathlib
import re
import sys
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
ROUTES_RS = ROOT / "crates/admin/src/routes.rs"
API_MD = ROOT / "docs/site/api.md"
# The proxy listener's route tables: the relay's own router, the keepalive mod's route, and every router the
# binary merges beside them. A file listed here that does not exist fails the check rather than narrowing it.
PROXY_ROUTE_FILES = [
    ROOT / "crates/keepalive/src/routes.rs",
    ROOT / "crates/ledger/src/usage.rs",
    ROOT / "crates/relay/src/messages.rs",
]
CLIENT_MD = ROOT / "docs/site/client.md"
# The path zensical.toml's site_url publishes the site under; a root-relative link carries it.
PREFIX = "phoebis"

# Another host's links are not this script's business, and a bare fragment stays on the page it is in.
SKIPPED = ("http:", "https:", "mailto:", "javascript:", "data:", "tel:", "#")

METHODS = ("any", "delete", "get", "head", "options", "patch", "post", "put", "trace")
METHOD_CALL = re.compile(r"\b(" + "|".join(METHODS) + r")\(")
# Any other call inside a handler expression is a method this checker does not recognize; CALL finds it so the
# check fails closed instead of silently dropping the route.
CALL = re.compile(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\(")
# An entry heading on the reference page: the method in capitals, then the path as the route table spells it.
ENTRY = re.compile(r"^### (ANY|DELETE|GET|HEAD|OPTIONS|PATCH|POST|PUT|TRACE) (/admin\S*)$", re.M)
# A row of the "What the proxy accepts" table: the method in capitals, then the path in backticks.
CLIENT_ROW = re.compile(r"^\| (ANY|DELETE|GET|HEAD|OPTIONS|PATCH|POST|PUT|TRACE) \| `([^`]+)` \|", re.M)
# `pub const NAME: &str = "...";`, so a route registered under a constant is read as its path.
PATH_CONST = re.compile(r'const ([A-Z_][A-Z0-9_]*): &str = "([^"]+)";')
# A char or byte-char literal (`'x'`, `b'x'`, `'\n'`, `'\u{2026}'`); a quote inside one is a character and
# opens no string, while a lifetime (`'a`, with no closing quote) is not matched at all.
CHAR_LIT = re.compile(r"b?'(\\u\{[0-9a-fA-F_]+\}|\\.|[^\\'\n])'")
# A raw string opener (`r"`, `r#"`, `br##"`); the closing quote carries the same hashes.
RAW_OPEN = re.compile(r'b?r(#*)"')
# A route path written as a raw string, read back to its contents.
RAW_PATH = re.compile(r'^r(#*)"(.*)"\1$', re.S)


class Links(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.found = []

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if name in ("href", "src") and value:
                self.found.append(value)


def resolves(site, page, raw):
    path = urllib.parse.unquote(urllib.parse.urlsplit(raw).path)
    if not path:
        return True
    site = site.resolve()
    if path.startswith("/"):
        # A root-relative link must carry the prefix site_url publishes this site under; a browser resolves
        # it against the host root, not against site/, so a link missing the prefix is dangling regardless
        # of whether some other path happens to exist under site/.
        parts = path.lstrip("/").split("/", 1)
        if parts[0] != PREFIX:
            return False
        remainder = parts[1] if len(parts) == 2 else ""
        candidates = [site / remainder] if remainder else [site]
    else:
        candidates = [page.parent / path]
    for target in candidates:
        target = target.resolve()
        if not target.is_relative_to(site):
            continue
        if target.is_file():
            return True
        if target.is_dir() and (target / "index.html").is_file():
            return True
    return False


def check_links(site):
    pages = sorted(site.rglob("*.html"))
    if not pages:
        print(f"no pages under {site}; build the site first")
        return 1
    dangling = 0
    for page in pages:
        parser = Links()
        parser.feed(page.read_text(encoding="utf-8"))
        for raw in parser.found:
            if raw.startswith(SKIPPED):
                continue
            if not resolves(site, page, raw):
                print(f"{page.relative_to(site)}: {raw} resolves to nothing")
                dangling += 1
    return 1 if dangling else 0


def line_of(text, at):
    return text.count("\n", 0, at) + 1


def blank(text):
    """`text` as spaces with its newlines kept, so a span hidden from a scan leaves every line number in place."""
    return "".join(c if c == "\n" else " " for c in text)


def tokenized(source):
    """Two texts the length and line structure of `source`: `code`, every comment blanked and every string
    kept, for finding `.route(` and reading its path; `blanked`, the strings' contents blanked too, for
    balancing the parentheses and scanning the handlers. Line comments, block comments nested as Rust nests
    them, `"..."` with escapes, `b"..."`, char literals, and raw strings with any hash count are the
    tokens."""
    code, blanked = [], []
    i, n = 0, len(source)
    while i < n:
        ch = source[i]
        # `b"` and `r#"` open a string only at a token boundary; inside an identifier they are letters.
        ident_before = i > 0 and (source[i - 1].isalnum() or source[i - 1] == "_")
        raw = None if ident_before else RAW_OPEN.match(source, i)
        char = None if ident_before else CHAR_LIT.match(source, i)
        if source.startswith("//", i):
            end = source.find("\n", i)
            end = n if end < 0 else end
            code.append(" " * (end - i))
            blanked.append(" " * (end - i))
            i = end
        elif source.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if source.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif source.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            if depth:
                raise SystemExit(f"an unterminated block comment at line {line_of(source, i)}")
            code.append(blank(source[i:j]))
            blanked.append(blank(source[i:j]))
            i = j
        elif raw:
            close = '"' + raw.group(1)
            end = source.find(close, raw.end())
            if end < 0:
                raise SystemExit(f"an unterminated raw string at line {line_of(source, i)}")
            end += len(close)
            code.append(source[i:end])
            blanked.append(source[i : raw.end()] + blank(source[raw.end() : end - len(close)]) + close)
            i = end
        elif char:
            # Kept in `code` like a string, its one character blanked in `blanked` so a `'('` opens no call.
            opened = i + char.group(0).index("'") + 1
            end = char.end()
            code.append(source[i:end])
            blanked.append(source[i:opened] + blank(source[opened : end - 1]) + "'")
            i = end
        elif ch == '"' or (source.startswith('b"', i) and not ident_before):
            opened = i + (2 if ch == "b" else 1)
            j = opened
            while j < n and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            if j >= n:
                raise SystemExit(f"an unterminated string at line {line_of(source, i)}")
            code.append(source[i : j + 1])
            blanked.append(source[i:opened] + blank(source[opened:j]) + '"')
            i = j + 1
        else:
            code.append(ch)
            blanked.append(ch)
            i += 1
    return "".join(code), "".join(blanked)


def route_path(first, consts):
    """The path a route's first argument names: a string, a raw string, or a `PATH_CONST` constant."""
    if len(first) >= 2 and first.startswith('"') and first.endswith('"'):
        return first[1:-1]
    raw = RAW_PATH.match(first)
    if raw:
        return raw.group(2)
    if first in consts:
        return consts[first]
    raise SystemExit(f"a route path this checker cannot read: {first}")


def route_calls(source):
    """Every `.route(<path>, <handlers>)` call as (line, path, handlers) over the tokenized source; the
    handlers text is the blanked one, so a method name inside a string is not a call."""
    code, blanked = tokenized(source)
    consts = dict(PATH_CONST.findall(code))
    calls = []
    at = 0
    while True:
        start = code.find(".route(", at)
        if start < 0:
            return calls
        opened = start + len(".route(")
        # Balanced over the blanked text, where nothing inside a string or a comment opens or closes a call.
        depth, pos = 1, opened
        while pos < len(blanked) and depth:
            depth += {"(": 1, ")": -1}.get(blanked[pos], 0)
            pos += 1
        if depth:
            raise SystemExit(f"unbalanced parentheses after .route( at line {line_of(code, start)}")
        comma = blanked.find(",", opened, pos - 1)
        if comma < 0:
            raise SystemExit(f".route( with one argument at line {line_of(code, start)}")
        path = route_path(code[opened:comma].strip(), consts)
        calls.append((line_of(code, start), path, blanked[comma + 1 : pos - 1]))
        at = pos


def routes_in(source):
    served = set()
    unknown = []
    for line, path, handlers in route_calls(source):
        calls = list(CALL.finditer(handlers))
        if not calls:
            # A handler expression with no call at all, such as a prebuilt router passed by name, still
            # serves the route; fail closed on it like an unrecognized method rather than dropping it.
            unknown.append((line, handlers.strip(), path))
            continue
        for method in calls:
            name = method.group(1)
            if name in METHODS:
                served.add((name.upper(), path))
            else:
                unknown.append((line, name, path))
    return served, unknown


def check_routes(routes_rs, api_md):
    source = routes_rs.read_text(encoding="utf-8")
    served, unknown = routes_in(source)
    documented = set(ENTRY.findall(api_md.read_text(encoding="utf-8")))
    if not served and not unknown:
        print(f"no routes parsed from {routes_rs}")
        return 1
    findings = 0
    for line, name, path in sorted(unknown):
        print(f"{routes_rs.name}:{line}: {name}(...) on {path} is not a method this checker consumes")
        findings += 1
    for method, path in sorted(served - documented):
        print(f"{api_md.name} has no entry for {method} {path}")
        findings += 1
    for method, path in sorted(documented - served):
        print(f"{api_md.name} documents {method} {path}, which the router does not serve")
        findings += 1
    return 1 if findings else 0


def check_client(route_files, client_md):
    """The proxy listener's served (method, path) pairs against the client page's table."""
    served = set()
    findings = 0
    for path in route_files:
        if not path.is_file():
            print(f"{path} is missing; the proxy route table names it")
            return 1
        found, unknown = routes_in(path.read_text(encoding="utf-8"))
        served |= found
        for line, name, route in sorted(unknown):
            print(f"{path.name}:{line}: {name}(...) on {route} is not a method this checker consumes")
            findings += 1
    documented = set(CLIENT_ROW.findall(client_md.read_text(encoding="utf-8")))
    for method, path in sorted(served - documented):
        print(f"{client_md.name} has no row for {method} {path}")
        findings += 1
    for method, path in sorted(documented - served):
        print(f"{client_md.name} lists {method} {path}, which the proxy listener does not serve")
        findings += 1
    return 1 if findings else 0


USAGE = "usage: check-docs.py links [SITE_DIR] | routes [ROUTES_RS] [API_MD] | client [ROUTE_RS... CLIENT_MD]"


def main(argv):
    if len(argv) >= 2 and argv[1] == "links":
        return check_links(pathlib.Path(argv[2]) if len(argv) > 2 else SITE)
    if len(argv) >= 2 and argv[1] == "routes":
        routes_rs = pathlib.Path(argv[2]) if len(argv) > 2 else ROUTES_RS
        api_md = pathlib.Path(argv[3]) if len(argv) > 3 else API_MD
        return check_routes(routes_rs, api_md)
    if len(argv) >= 2 and argv[1] == "client":
        if len(argv) == 3:
            # A route file without the client page to check it against is a mistake, not a default.
            print(USAGE)
            return 2
        files = [pathlib.Path(p) for p in argv[2:-1]] if len(argv) > 3 else PROXY_ROUTE_FILES
        client_md = pathlib.Path(argv[-1]) if len(argv) > 3 else CLIENT_MD
        return check_client(files, client_md)
    print(USAGE)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
