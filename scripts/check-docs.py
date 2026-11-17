#!/usr/bin/env python3
"""Read-only checks over the documentation site.

`links` walks the built site for an internal link or image that resolves to nothing, and for a link whose fragment
names no heading or anchor on the page it lands on. `client` compares the proxy listener's route table with the table
on the client page, so a route added or removed in the router fails the build until the page follows.

`check-config` compares the lines the binary's `check-config` arm prints with the installation page's list, `codes`
the refusal codes the `x-phoebis-error-code` header carries on the proxy and the feature listener with the client
page's table, `example` the installation page's quoted example with the file, `unit` the installation page's quoted
unit with `deploy/phoebis.service`, `schema` the schema versions the operations page states with the last step of the
registry's and the ledger's migration tables, and `prose` holds the mechanical content rules of the site's prose.
"""
import functools
import html.parser
import pathlib
import re
import sys
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
# The proxy listener's route tables: the relay's own router, the keepalive mod's route, and every router the
# binary merges beside them. A file listed here that does not exist fails the check rather than narrowing it.
PROXY_ROUTE_FILES = [
    ROOT / "crates/keepalive/src/companion.rs",
    ROOT / "crates/keepalive/src/routes.rs",
    ROOT / "crates/ledger/src/usage.rs",
    ROOT / "crates/relay/src/messages.rs",
]
CLIENT_MD = ROOT / "docs/site/client.md"
MAIN_RS = ROOT / "crates/router/src/main.rs"
INSTALLATION_MD = ROOT / "docs/site/installation.md"
EXAMPLE_YAML = ROOT / "deploy/config.example.yaml"
UNIT_FILE = ROOT / "deploy/phoebis.service"
REGISTRY_SCHEMA_RS = ROOT / "crates/store/src/schema.rs"
LEDGER_SCHEMA_RS = ROOT / "crates/ledger/src/schema.rs"
OPERATIONS_MD = ROOT / "docs/site/operations.md"
# Every file whose refusals carry a code in the `x-phoebis-error-code` header on the proxy or the feature listener, and
# the bus constant the body-shape code is spelled by. A file listed here that does not exist fails the check rather
# than narrowing it.
CODE_FILES = [
    ROOT / "crates/bus/src/event.rs",
    ROOT / "crates/ledger/src/usage.rs",
    ROOT / "crates/relay/src/errors.rs",
    ROOT / "crates/relay/src/exhaustion.rs",
    ROOT / "crates/relay/src/models.rs",
]
# The markers render to nothing and anchor what two checks read on a page.
CHECK_CONFIG_BEGIN, CHECK_CONFIG_END = "<!-- check-config: begin -->", "<!-- check-config: end -->"
REFUSAL_CODES_BEGIN, REFUSAL_CODES_END = "<!-- refusal-codes: begin -->", "<!-- refusal-codes: end -->"
# The path zensical.toml's site_url publishes the site under; a root-relative link carries it.
PREFIX = "phoebis"

# Another host's links are not this script's business.
SKIPPED = ("http:", "https:", "mailto:", "javascript:", "data:", "tel:")
# The theme's skip link points at a page's first heading, and 404.html, built from no Markdown, has none, so the
# theme writes `#__skip` there with nothing carrying that id.
SKIP_LINK = "#__skip"

METHODS = ("any", "delete", "get", "head", "options", "patch", "post", "put", "trace")
METHOD_CALL = re.compile(r"\b(" + "|".join(METHODS) + r")\(")
# Any other call inside a handler expression is a method this checker does not recognize; CALL finds it so the
# check fails closed instead of silently dropping the route.
CALL = re.compile(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\(")
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
    """The hrefs and srcs a page carries, and the ids the site build gave its headings and anchors."""

    def __init__(self):
        super().__init__()
        self.found = []
        # A `src` fragment is the theme's light and dark switch on an image, not a place on a page, so only an
        # `href` has its fragment checked.
        self.hrefs = []
        self.ids = set()

    def handle_starttag(self, tag, attrs):
        for name, value in attrs:
            if name in ("href", "src") and value:
                self.found.append(value)
            if name == "href" and value:
                self.hrefs.append(value)
            if name == "id" and value:
                self.ids.add(value)


@functools.cache
def ids_of(page):
    parser = Links()
    parser.feed(page.read_text(encoding="utf-8"))
    return parser.ids


def target_of(site, page, raw):
    """The file a link lands on, a directory read as its `index.html` and a link without a path as `page`
    itself, or None when the link resolves to nothing."""
    path = urllib.parse.unquote(urllib.parse.urlsplit(raw).path)
    if not path:
        return page
    site = site.resolve()
    if path.startswith("/"):
        # A root-relative link must carry the prefix site_url publishes this site under; a browser resolves
        # it against the host root, not against site/, so a link missing the prefix is dangling regardless
        # of whether some other path happens to exist under site/.
        parts = path.lstrip("/").split("/", 1)
        if parts[0] != PREFIX:
            return None
        remainder = parts[1] if len(parts) == 2 else ""
        candidates = [site / remainder] if remainder else [site]
    else:
        candidates = [page.parent / path]
    for target in candidates:
        target = target.resolve()
        if not target.is_relative_to(site):
            continue
        if target.is_file():
            return target
        if target.is_dir() and (target / "index.html").is_file():
            return target / "index.html"
    return None


def resolves(site, page, raw):
    return target_of(site, page, raw) is not None


def fragment_missing_from(site, page, raw):
    """The page whose ids lack a link's fragment: the page itself for a bare `#x`, the resolved target for
    `page/#x`. None when the fragment is among them or there is none to check."""
    fragment = urllib.parse.unquote(urllib.parse.urlsplit(raw).fragment)
    if not fragment or raw == SKIP_LINK:
        return None
    target = target_of(site, page, raw)
    # A link that lands nowhere is the path check's finding, and an image or a file other than a page has no ids.
    if target is None or target.suffix != ".html":
        return None
    return None if fragment in ids_of(target) else target


def check_links(site):
    # Resolved first, so a page and the target a link resolves to share one root for the paths a finding prints.
    site = site.resolve()
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
        for raw in parser.hrefs:
            if raw.startswith(SKIPPED):
                continue
            target = fragment_missing_from(site, page, raw)
            if target is not None:
                print(f"{page.relative_to(site)}: {raw} names no heading or anchor in {target.relative_to(site)}")
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


def check_client(route_files, client_md):
    """The proxy listener's served (method, path) pairs against the client page's table."""
    if missing([client_md]):
        return 1
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


def between(lines, begin, end, path):
    """The lines between one `begin` and one `end` marker with the number of the first, or None after printing
    why there are not exactly one of each: a page without its markers fails the check that reads them."""
    begins = [i for i, line in enumerate(lines) if line.strip() == begin]
    ends = [i for i, line in enumerate(lines) if line.strip() == end]
    if len(begins) != 1 or len(ends) != 1 or ends[0] < begins[0]:
        print(f"{path.name}: expected one {begin} before one {end}, found {len(begins)} and {len(ends)}")
        return None
    return begins[0] + 2, lines[begins[0] + 1 : ends[0]]


def missing(paths):
    """Whether any of `paths` is not a file, after printing each one that is not: a moved file is a finding that
    names it, never a traceback."""
    absent = [path for path in paths if not path.is_file()]
    for path in absent:
        print(f"{path} is missing")
    return bool(absent)


def check_quoted_block(fence, title, kind, source, page_md):
    """The page's quoted block under `fence` against `source`, line for line, a trailing newline aside: the messages
    both checks answer, `kind` the word the closing-fence message names."""
    if missing([source, page_md]):
        return 1
    expected = source.read_text(encoding="utf-8").splitlines()
    lines = page_md.read_text(encoding="utf-8").splitlines()
    opens = [i for i, line in enumerate(lines) if line.strip() == fence]
    if len(opens) != 1:
        print(f"{page_md.name}: {len(opens)} fenced blocks titled {title}, expected one")
        return 1
    start = opens[0] + 1
    close = next((i for i in range(start, len(lines)) if lines[i].strip() == "```"), None)
    if close is None:
        print(f"{page_md.name}:{start + 1}: the {kind} block has no closing fence")
        return 1
    quoted = lines[start:close]
    for offset, (page, file) in enumerate(zip(quoted, expected)):
        if page != file:
            print(f"{page_md.name}:{start + offset + 1}: differs from {source.name}:{offset + 1}: {page!r} vs {file!r}")
            return 1
    if len(quoted) != len(expected):
        print(f"{page_md.name}:{start + 1}: the block has {len(quoted)} lines and {source.name} {len(expected)}")
        return 1
    return 0


EXAMPLE_FENCE = '```yaml title="deploy/config.example.yaml"'


def check_example(example_yaml, installation_md):
    """The installation page's quoted example against the file, line for line, a trailing newline aside."""
    return check_quoted_block(EXAMPLE_FENCE, "deploy/config.example.yaml", "example", example_yaml, installation_md)


UNIT_FENCE = '```ini title="deploy/phoebis.service"'


def check_unit(unit_file, installation_md):
    """The installation page's quoted unit against the file, line for line, a trailing newline aside."""
    return check_quoted_block(UNIT_FENCE, "deploy/phoebis.service", "unit", unit_file, installation_md)


# Read over the text `tokenized` blanks, so a literal's contents and a comment hold no bracket, comma or semicolon. A
# literal keeps its quotes, so it still stands in its step as the expression.
MIGRATIONS_DECLARATION = re.compile(r"\bpub const MIGRATIONS\b")
# The declaration's type, `&[(u32, &str)]`, is no step, so the list starts after the `= &[`.
MIGRATIONS_LIST = re.compile(r"[^;=]*=\s*&\s*\[([^;]*)\]\s*;")
# A step as rustfmt writes it, on one line or wrapped with a comma after the expression: the number, then a constant
# or a plain or raw string literal.
MIGRATIONS_STEP = re.compile(
    r"\s*\(\s*(?P<number>\d+)\s*,\s*"
    r'(?:[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*|"\s*"|r(?P<hashes>#*)"\s*"(?P=hashes))'
    r"\s*,?\s*\)\s*(?:,|$)"
)
SCHEMA_SENTENCE = re.compile(r"the registry at schema (\d+) and the ledger at schema (\d+)")


def table_steps(listed):
    """The numbers of a `MIGRATIONS` list's steps in order, or None where any of its text is no step."""
    steps, at = [], 0
    while listed[at:].strip():
        step = MIGRATIONS_STEP.match(listed, at)
        if not step:
            return None
        steps.append(int(step.group("number")))
        at = step.end()
    return steps


def last_step(schema_rs):
    """The number of the last step of the `MIGRATIONS` table in `schema_rs`, or None after printing why there is not
    one table that reads as `(number, expression)` steps whose numbers ascend."""
    shown = schema_rs.relative_to(ROOT) if schema_rs.is_relative_to(ROOT) else schema_rs
    _, blanked = tokenized(schema_rs.read_text(encoding="utf-8"))
    declarations = list(MIGRATIONS_DECLARATION.finditer(blanked))
    if len(declarations) != 1:
        print(f"{shown}: {len(declarations)} MIGRATIONS tables, expected one")
        return None
    listed = MIGRATIONS_LIST.match(blanked, declarations[0].end())
    steps = table_steps(listed.group(1)) if listed else None
    if not steps or any(earlier >= later for earlier, later in zip(steps, steps[1:])):
        line = line_of(blanked, declarations[0].start())
        print(f"{shown}:{line}: a MIGRATIONS table this check cannot read as ascending (number, expression) steps")
        return None
    return steps[-1]


def check_schema(registry_rs, ledger_rs, page):
    """The registry's and the ledger's schema versions the operations page states against the last step of each
    store's `MIGRATIONS` table. The page's lines are joined, so a sentence wrapped across two reads as one."""
    if missing([registry_rs, ledger_rs, page]):
        return 1
    registry, ledger = last_step(registry_rs), last_step(ledger_rs)
    findings = [registry, ledger].count(None)
    joined = " ".join(line.strip() for line in page.read_text(encoding="utf-8").splitlines())
    sentences = SCHEMA_SENTENCE.findall(joined)
    if len(sentences) != 1:
        print(f"{page.name}: {len(sentences)} sentences stating the registry's and the ledger's schema, expected one")
        return 1
    stated_registry, stated_ledger = (int(figure) for figure in sentences[0])
    for store, stated, last in (("registry", stated_registry, registry), ("ledger", stated_ledger, ledger)):
        if last is not None and stated != last:
            print(f"{page.name}: the {store} at schema {stated}, but its MIGRATIONS table ends at step {last}")
            findings += 1
    return 1 if findings else 0


CHECK_CONFIG_ARM = "Some(Command::CheckConfig) => {"
PRINTLN = re.compile(r"\bprintln!\(")
PLACEHOLDER = re.compile(r"\{[^{}]*\}")
# A literal continued over a backslash at the end of a line: Rust drops the backslash, the newline and the next
# line's leading whitespace, so the check joins it the same way before it reads the fragments.
CONTINUATION = re.compile(r"\\\n\s*")
LIST_ITEM = re.compile(r"^(\d+)\. (.*)$")


def println_strings(code, blanked, opened, close):
    """Every `println!` call's format string between the arm's braces, in source order, with whether the call sits
    at the arm's own depth (unconditional) or inside a `match`, `if` or `for` block of it (conditional)."""
    found = []
    for call in PRINTLN.finditer(blanked, opened, close):
        depth = blanked.count("{", opened, call.start()) - blanked.count("}", opened, call.start())
        quote = code.find('"', call.end(), close)
        if quote < 0:
            raise SystemExit(f"a println! without a format string at line {line_of(code, call.start())}")
        j = quote + 1
        while j < close and code[j] != '"':
            j += 2 if code[j] == "\\" else 1
        found.append((CONTINUATION.sub("", code[quote + 1 : j]), depth == 1))
    return found


def skeleton(literal):
    """The literal fragments between a format string's placeholders, trimmed, empties dropped. An empty list is a
    wildcard: a line that is one placeholder prints whatever value it carries."""
    return [part.strip() for part in PLACEHOLDER.split(literal) if part.strip()]


def list_items(lines, first):
    """A numbered list's items as (line, text): continuation lines joined by one space, code spans unwrapped."""
    items = []
    for offset, line in enumerate(lines):
        item = LIST_ITEM.match(line)
        if item:
            items.append([first + offset, item.group(2).strip()])
        elif items and line.strip():
            items[-1][1] += " " + line.strip()
    return [(line, text.replace("`", "")) for line, text in items]


def fragments_in(fragments, text):
    at = 0
    for fragment in fragments:
        at = text.find(fragment, at)
        if at < 0:
            return False
        at += len(fragment)
    return True


def check_check_config(main_rs, installation_md):
    """The lines `phoebis check-config` prints, read from the binary's source without building it, against the
    installation page's list: the unconditional lines in order, every conditional line somewhere after them."""
    if missing([main_rs, installation_md]):
        return 1
    code, blanked = tokenized(main_rs.read_text(encoding="utf-8"))
    arm = code.find(CHECK_CONFIG_ARM)
    if arm < 0:
        print(f"{main_rs.name}: no {CHECK_CONFIG_ARM} arm")
        return 1
    opened = arm + len(CHECK_CONFIG_ARM) - 1
    depth, close = 1, opened + 1
    while close < len(blanked) and depth:
        depth += {"{": 1, "}": -1}.get(blanked[close], 0)
        close += 1
    if depth:
        raise SystemExit(f"unbalanced braces after the check-config arm at line {line_of(code, arm)}")
    printed = println_strings(code, blanked, opened, close)
    unconditional = [(literal, skeleton(literal)) for literal, direct in printed if direct]
    conditional = [(literal, skeleton(literal)) for literal, direct in printed if not direct]
    lines = installation_md.read_text(encoding="utf-8").splitlines()
    span = between(lines, CHECK_CONFIG_BEGIN, CHECK_CONFIG_END, installation_md)
    if span is None:
        return 1
    items = list_items(span[1], span[0])
    findings, matched = 0, set()
    for k, (literal, fragments) in enumerate(unconditional):
        if k >= len(items):
            print(f"{installation_md.name}: no item for the line {literal!r}, which the binary prints as line {k + 1}")
            findings += 1
            continue
        line, text = items[k]
        if not fragments_in(fragments, text):
            print(f"{installation_md.name}:{line}: item {k + 1} does not match the line {literal!r}")
            findings += 1
        matched.add(k)
    later = list(enumerate(items))[len(unconditional):]
    for literal, fragments in conditional:
        if not fragments:
            # A conditional line that is one placeholder prints a value no page item can name.
            continue
        hits = [k for k, (line, text) in later if fragments_in(fragments, text)]
        if not hits:
            print(f"{installation_md.name}: no item for the conditional line {literal!r}")
            findings += 1
        matched.update(hits)
    for k, (line, text) in later:
        if k not in matched:
            print(f"{installation_md.name}:{line}: item {k + 1} matches no line the binary prints")
            findings += 1
    return 1 if findings else 0


# A refusal's code: the value of a `code:` field, or the third argument of an `anthropic_error(` call. The value is
# a plain or raw string literal, or an identifier (with or without a path) resolved against a `const` by its last
# segment. Any other expression is reported, because a shape the check cannot read is a finding, never a silent
# pass. `code: &str` and `code: &'static str` are a parameter and a field, not a value: a `&` opens a type, never a
# code, so a value opening with `&` is passed by. The lookahead spans the whitespace itself, since a `\s*` before
# it would backtrack to zero width and test the space ahead of the `&`. `self.code` in `Failure::response` forwards
# a `code:` field the check has read, so it is passed by too.
# A `pub code:` is a member's declaration, never a value, so a `code:` after `pub` is passed by.
# A bare identifier that names no const is a local: it resolves through the last `let <ident> = <fn>(..)` ahead of it
# in the function that holds it, to every arm of `fn <fn>` in the files read, whose body is one `match` and nothing
# else. Each arm's value is a literal, a const, or a `Failure` const's `.code`, read off that const's own `code:`
# literal. A local with no such binding, a function the files read do not hold once, a body of another shape and an
# arm of any other value are each a finding.
CODE_FIELD = re.compile(r"(?<!pub )\bcode:(?!\s*&)\s*")
ERROR_CALL = re.compile(r"(?<!fn )\banthropic_error\(")
CODE_LITERAL = re.compile(r'^r?(#*)"([a-z_]+)"\1$')
CODE_IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*$")
FIELD_FORWARD = "self.code"
CODE_CONST = re.compile(r"\bconst ([A-Z_][A-Z0-9_]*): &(?:'static )?str = \"([a-z_]+)\";")
LOCAL = re.compile(r"^[a-z_][a-z0-9_]*$")
LET_CALL = re.compile(r"\blet ([a-z_][a-z0-9_]*) = ([a-z_][a-z0-9_]*)\(")
# A code read straight off a call, `code: name(..)`, reads that function's arms as a `let` binding of it does.
DIRECT_CALL = re.compile(r"^([a-z_][a-z0-9_]*)\(.*\)$")
FN_NAME = re.compile(r"\bfn ([a-z_][a-z0-9_]*)\b")
MATCH_BODY = re.compile(r"\s*match\b")
FAILURE_CONST = re.compile(r"\bconst ([A-Z_][A-Z0-9_]*): Failure = Failure \{")
FAILURE_CODE = re.compile(r"^([A-Z_][A-Z0-9_]*)\.code$")
# The `auth_*` arms of `errors.rs` answer a two-string tuple, the code first.
CODE_TUPLE = re.compile(r'\(\s*"([a-z][a-z_]*)",\s*"')
# A row of the refusal code table: the code in backticks in its first cell.
CODE_ROW = re.compile(r"^\| `([a-z_]+)` \|")


def split_arguments(blanked, opened, close):
    """The (start, end) of each top-level argument between `opened` and `close`, over the blanked text."""
    spans, depth, at = [], 0, opened
    for pos in range(opened, close):
        depth += {"(": 1, "[": 1, "{": 1, ")": -1, "]": -1, "}": -1}.get(blanked[pos], 0)
        if blanked[pos] == "," and depth == 0:
            spans.append((at, pos))
            at = pos + 1
    if blanked[at:close].strip():
        spans.append((at, close))
    return spans


def expression_end(blanked, start):
    """The offset where the expression opening at `start` ends: the first `,` at its own depth, or the bracket
    closing the field list or the argument list that holds it."""
    depth = 0
    for pos in range(start, len(blanked)):
        char = blanked[pos]
        if char in "([{":
            depth += 1
        elif char in ")]}":
            if depth == 0:
                return pos
            depth -= 1
        elif char == "," and depth == 0:
            return pos
    raise SystemExit(f"an expression with no end at offset {start}")


def closing(blanked, opened):
    """The offset of the bracket that closes the one at `opened`."""
    depth = 0
    for pos in range(opened, len(blanked)):
        if blanked[pos] in "([{":
            depth += 1
        elif blanked[pos] in ")]}":
            depth -= 1
            if depth == 0:
                return pos
    raise SystemExit(f"an unclosed bracket at offset {opened}")


def function_bodies(blanked):
    """(name, open, close) of each function body: the braces after a `fn` signature, a declaration that ends in `;`
    passed by."""
    bodies = []
    for fn in FN_NAME.finditer(blanked):
        opened = blanked.find("{", fn.end())
        semicolon = blanked.find(";", fn.end())
        if opened < 0 or 0 <= semicolon < opened:
            continue
        bodies.append((fn.group(1), opened, closing(blanked, opened)))
    return bodies


def bound_call(blanked, offset, local):
    """The function the last `let <local> = <fn>(..)` ahead of `offset` calls, in the innermost function holding
    `offset`, or None."""
    holding = [(opened, close) for _, opened, close in function_bodies(blanked) if opened < offset < close]
    if not holding:
        return None
    opened, _ = min(holding, key=lambda span: span[1] - span[0])
    calls = [let.group(2) for let in LET_CALL.finditer(blanked, opened, offset) if let.group(1) == local]
    return calls[-1] if calls else None


def match_arms(code, blanked, opened, close):
    """The (offset, value) of each arm when the body between the braces at `opened` and `close` is one `match` and
    nothing else, or None."""
    lead = MATCH_BODY.match(blanked, opened + 1)
    if not lead:
        return None
    block = blanked.find("{", lead.end())
    if block < 0 or block > close:
        return None
    end = closing(blanked, block)
    if blanked[end + 1 : close].strip():
        return None
    arms = []
    for start, stop in split_arguments(blanked, block + 1, end):
        arrow = blanked.find("=>", start, stop)
        if arrow < 0:
            return None
        value = code[arrow + 2 : stop]
        arms.append((arrow + 2 + len(value) - len(value.lstrip()), value.strip()))
    return arms


def failure_codes(code, blanked, consts):
    """The code of each `Failure` const, by the const's name: its `code:` literal, or the value of the code const the
    field names among `consts`."""
    found = {}
    for failure in FAILURE_CONST.finditer(blanked):
        opened = failure.end() - 1
        fields = blanked[opened : closing(blanked, opened)]
        field = CODE_FIELD.search(fields)
        if field:
            start = opened + field.end()
            expression = code[start : expression_end(blanked, start)].strip()
            literal = CODE_LITERAL.match(expression)
            if literal:
                found[failure.group(1)] = literal.group(2)
            elif CODE_IDENT.match(expression) and expression.rsplit("::", 1)[-1] in consts:
                found[failure.group(1)] = consts[expression.rsplit("::", 1)[-1]]
    return found


def code_expressions(code, blanked):
    """Every expression that carries a refusal code, as (offset, text): the value of each `code:` field and the
    third argument of each `anthropic_error(` call, the function's own definition passed by."""
    found = []
    for field in CODE_FIELD.finditer(blanked):
        start = field.end()
        found.append((start, code[start : expression_end(blanked, start)].strip()))
    for call in ERROR_CALL.finditer(blanked):
        opened = call.end()
        depth, pos = 1, opened
        while pos < len(blanked) and depth:
            depth += {"(": 1, ")": -1}.get(blanked[pos], 0)
            pos += 1
        if depth:
            raise SystemExit(f"unbalanced parentheses after anthropic_error( at line {line_of(code, call.start())}")
        arguments = split_arguments(blanked, opened, pos - 1)
        if len(arguments) >= 3:
            start, end = arguments[2]
            start += len(code[start:end]) - len(code[start:end].lstrip())
            found.append((start, code[start:end].strip()))
    return found


def check_codes(code_files, client_md):
    """Every code the `x-phoebis-error-code` header carries on the proxy and the feature listener against the client
    page's refusal table, both ways. A code expression the check cannot read is a finding, never a silent pass."""
    if missing([client_md]):
        return 1
    consts, failures, sources, answered, pending, findings = {}, {}, {}, set(), [], 0
    # Each bound function's arms are read once, however many locals bind it.
    read_arms = set()
    for path in code_files:
        if not path.is_file():
            print(f"{path} is missing; the code file list names it")
            return 1
        code, blanked = tokenized(path.read_text(encoding="utf-8"))
        sources[path] = code, blanked
        consts.update(CODE_CONST.findall(code))
        failures.update(failure_codes(code, blanked, consts))
        for offset, expression in code_expressions(code, blanked):
            literal = CODE_LITERAL.match(expression)
            if literal:
                answered.add(literal.group(2))
            elif CODE_IDENT.match(expression) or DIRECT_CALL.match(expression):
                pending.append((path, offset, expression))
            elif expression != FIELD_FORWARD:
                print(f"{path.name}:{line_of(code, offset)}: unsupported code expression {expression}")
                findings += 1
        if path.name == "errors.rs":
            answered.update(CODE_TUPLE.findall(code))
    for path, offset, ident in pending:
        code, blanked = sources[path]
        line = line_of(code, offset)
        direct = DIRECT_CALL.match(ident)
        if direct:
            called = direct.group(1)
        else:
            name = ident.rsplit("::", 1)[-1]
            if name in consts:
                answered.add(consts[name])
                continue
            if not LOCAL.match(ident):
                print(f"{path.name}:{line}: code {ident} resolves to no const in the files read")
                findings += 1
                continue
            called = bound_call(blanked, offset, ident)
            if called is None:
                print(f"{path.name}:{line}: code {ident} resolves to no const or binding in the files read")
                findings += 1
                continue
        bodies = [
            (source, opened, close)
            for source, (_, source_blanked) in sources.items()
            for fn, opened, close in function_bodies(source_blanked)
            if fn == called
        ]
        if len(bodies) != 1:
            print(f"{path.name}:{line}: code {ident} binds fn {called}, found {len(bodies)} times in the files read")
            findings += 1
            continue
        if called in read_arms:
            continue
        read_arms.add(called)
        source, opened, close = bodies[0]
        source_code, source_blanked = sources[source]
        arms = match_arms(source_code, source_blanked, opened, close)
        if arms is None:
            print(f"{source.name}:{line_of(source_code, opened)}: fn {called} is no single match the check reads")
            findings += 1
            continue
        for at, value in arms:
            literal = CODE_LITERAL.match(value)
            failure = FAILURE_CODE.match(value)
            if literal:
                answered.add(literal.group(2))
            elif failure and failure.group(1) in failures:
                answered.add(failures[failure.group(1)])
            elif CODE_IDENT.match(value) and value.rsplit("::", 1)[-1] in consts:
                answered.add(consts[value.rsplit("::", 1)[-1]])
            else:
                where = f"{source.name}:{line_of(source_code, at)}"
                print(f"{where}: arm {value} of fn {called} is no code the check reads")
                findings += 1
    lines = client_md.read_text(encoding="utf-8").splitlines()
    span = between(lines, REFUSAL_CODES_BEGIN, REFUSAL_CODES_END, client_md)
    if span is None:
        return 1
    documented = {row.group(1) for row in map(CODE_ROW.match, span[1]) if row}
    for name in sorted(answered - documented):
        print(f"{client_md.name} has no row for {name}")
        findings += 1
    for name in sorted(documented - answered):
        print(f"{client_md.name} lists {name}, which neither the proxy nor the feature listener answers")
        findings += 1
    return 1 if findings else 0


FENCE = re.compile(r"^\s*(`{3,}|~{3,})")
CODE_SPAN = re.compile(r"(`+)(.+?)\1")
HTML_COMMENT = re.compile(r"<!--.*?-->")
DASHES = ("—", "–")
# The words of a working document that a reference page does not use. "commit", "lane", "plan" and "review" have a
# product sense (a committed stream, the fast lane, the plan meter, auto mode's review) and stay off the list. "shape"
# with a capital letter is case-sensitive, since "the shape a client sends" is prose.
VOCABULARY = re.compile(
    r"\b(sub-projects?|subprojects?|rulings?|tasks?|findings?|worktrees?|workstreams?|item [0-9]+|option [0-9])\b",
    re.IGNORECASE,
)
SHAPE_LETTER = re.compile(r"\bshape [A-Z]\b")
BARE_API_KEY = re.compile(r"\bapi_key\b")


def prose_files():
    return sorted((ROOT / "docs/site").glob("*.md")) + [ROOT / "README.md"]


def check_prose(files):
    """The mechanical content rules over a page's prose: fenced blocks dropped (a fence closes only on a bare fence
    line of its own character, at least as long), code spans removed with an open span carried across lines until a
    blank line or a fence ends its paragraph, HTML comments removed from each remaining line, the column count read
    on the original line."""
    findings = 0
    for path in files:
        if not path.is_file():
            print(f"{path} is missing")
            findings += 1
            continue
        shown = path.relative_to(ROOT) if path.is_relative_to(ROOT) else path
        fence, spanned = None, False
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            marker = FENCE.match(line)
            if fence is None and (marker or not line.strip()):
                # A code span never crosses a paragraph, so a backtick left open ends at a blank line or a fence.
                spanned = False
            if fence is None and marker:
                fence = marker.group(1)
                continue
            if fence is not None:
                # A shorter fence line, or one with an info string (a three-backtick `sh` line inside a three- or
                # four-backtick block), is content, not the close.
                run = marker.group(1) if marker else None
                if run and line.strip() == run and run[0] == fence[0] and len(run) >= len(fence):
                    fence = None
                continue
            if len(line) > 120 and not line.lstrip().startswith("|"):
                print(f"{shown}:{number}: columns: {len(line)}")
                findings += 1
            text = line
            if spanned:
                # The previous line opened a span: the text up to its closing backtick is code.
                closing = text.find("`")
                spanned = closing < 0
                text = "" if spanned else text[closing + 1 :]
            text = CODE_SPAN.sub("", text)
            if text.count("`") % 2:
                # An odd backtick count opens a span that wraps onto the next line.
                text = text[: text.rfind("`")]
                spanned = True
            text = HTML_COMMENT.sub("", text)
            hits = (
                ("semicolon", ";" in text),
                ("dash", any(dash in text for dash in DASHES)),
                ("vocabulary", bool(VOCABULARY.search(text) or SHAPE_LETTER.search(text))),
                ("api_key", bool(BARE_API_KEY.search(text))),
            )
            for rule, hit in hits:
                if hit:
                    print(f"{shown}:{number}: {rule}: {text.strip()}")
                    findings += 1
    return 1 if findings else 0


USAGE = (
    "usage: check-docs.py links [SITE_DIR] | client [ROUTE_RS... CLIENT_MD]"
    " | check-config [MAIN_RS INSTALLATION_MD] | codes [RS_FILE... CLIENT_MD]"
    " | example [EXAMPLE_YAML INSTALLATION_MD] | unit [UNIT_FILE INSTALLATION_MD]"
    " | schema [REGISTRY_SCHEMA_RS LEDGER_SCHEMA_RS OPERATIONS_MD] | prose [FILE...]"
)


def main(argv):
    if len(argv) >= 2 and argv[1] == "links":
        return check_links(pathlib.Path(argv[2]) if len(argv) > 2 else SITE)
    if len(argv) >= 2 and argv[1] == "client":
        if len(argv) == 3:
            # A route file without the client page to check it against is a mistake, not a default.
            print(USAGE)
            return 2
        files = [pathlib.Path(p) for p in argv[2:-1]] if len(argv) > 3 else PROXY_ROUTE_FILES
        client_md = pathlib.Path(argv[-1]) if len(argv) > 3 else CLIENT_MD
        return check_client(files, client_md)
    if len(argv) >= 2 and argv[1] == "check-config":
        if len(argv) not in (2, 4):
            print(USAGE)
            return 2
        main_rs = pathlib.Path(argv[2]) if len(argv) == 4 else MAIN_RS
        page = pathlib.Path(argv[3]) if len(argv) == 4 else INSTALLATION_MD
        return check_check_config(main_rs, page)
    if len(argv) >= 2 and argv[1] == "codes":
        if len(argv) == 3:
            # A code file without the page to check it against is a mistake, as it is for `client`.
            print(USAGE)
            return 2
        files = [pathlib.Path(p) for p in argv[2:-1]] if len(argv) > 3 else CODE_FILES
        client_md = pathlib.Path(argv[-1]) if len(argv) > 3 else CLIENT_MD
        return check_codes(files, client_md)
    if len(argv) >= 2 and argv[1] == "example":
        if len(argv) not in (2, 4):
            print(USAGE)
            return 2
        example = pathlib.Path(argv[2]) if len(argv) == 4 else EXAMPLE_YAML
        page = pathlib.Path(argv[3]) if len(argv) == 4 else INSTALLATION_MD
        return check_example(example, page)
    if len(argv) >= 2 and argv[1] == "unit":
        if len(argv) not in (2, 4):
            print(USAGE)
            return 2
        unit = pathlib.Path(argv[2]) if len(argv) == 4 else UNIT_FILE
        page = pathlib.Path(argv[3]) if len(argv) == 4 else INSTALLATION_MD
        return check_unit(unit, page)
    if len(argv) >= 2 and argv[1] == "schema":
        if len(argv) not in (2, 5):
            print(USAGE)
            return 2
        registry_rs = pathlib.Path(argv[2]) if len(argv) == 5 else REGISTRY_SCHEMA_RS
        ledger_rs = pathlib.Path(argv[3]) if len(argv) == 5 else LEDGER_SCHEMA_RS
        page = pathlib.Path(argv[4]) if len(argv) == 5 else OPERATIONS_MD
        return check_schema(registry_rs, ledger_rs, page)
    if len(argv) >= 2 and argv[1] == "prose":
        return check_prose([pathlib.Path(p) for p in argv[2:]] or prose_files())
    print(USAGE)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
