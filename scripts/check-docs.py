#!/usr/bin/env python3
"""Read-only checks over the documentation site.

`links` walks the built site for an internal link or image that resolves to nothing; `routes` compares the admin
route table with the entries of the API reference page, so a route added or removed in the router fails the build
until the page follows.
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
# The path zensical.toml's site_url publishes the site under; a root-relative link carries it.
PREFIX = "phoebis"

# Another host's links are not this script's business, and a bare fragment stays on the page it is in.
SKIPPED = ("http:", "https:", "mailto:", "javascript:", "data:", "tel:", "#")

# One `.route("path", handlers)` call, up to the next `.route(` or the closing `.layer(`; the handlers may chain
# several methods onto one path, which METHOD_CALL then splits.
ROUTE_CALL = re.compile(r'\.route\(\s*"([^"]+)"\s*,(.*?)\)\s*(?=\.route\(|\.layer\()', re.S)
METHODS = ("any", "delete", "get", "head", "options", "patch", "post", "put", "trace")
METHOD_CALL = re.compile(r"\b(" + "|".join(METHODS) + r")\(")
# Any other call inside a handler expression is a method this checker does not recognize; CALL finds it so the
# check fails closed instead of silently dropping the route.
CALL = re.compile(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\(")
# An entry heading on the reference page: the method in capitals, then the path as the route table spells it.
ENTRY = re.compile(r"^### (ANY|DELETE|GET|HEAD|OPTIONS|PATCH|POST|PUT|TRACE) (/admin\S*)$", re.M)


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


def routes_in(source):
    served = set()
    unknown = []
    for call in ROUTE_CALL.finditer(source):
        path, handlers = call.group(1), call.group(2)
        line = source.count("\n", 0, call.start()) + 1
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


def main(argv):
    if len(argv) >= 2 and argv[1] == "links":
        return check_links(pathlib.Path(argv[2]) if len(argv) > 2 else SITE)
    if len(argv) >= 2 and argv[1] == "routes":
        routes_rs = pathlib.Path(argv[2]) if len(argv) > 2 else ROUTES_RS
        api_md = pathlib.Path(argv[3]) if len(argv) > 3 else API_MD
        return check_routes(routes_rs, api_md)
    print("usage: check-docs.py links [SITE_DIR] | routes [ROUTES_RS] [API_MD]")
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
