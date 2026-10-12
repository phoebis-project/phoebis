#!/usr/bin/env bash
# The checker has to fail on what it exists to catch: a dangling link, a fragment its page lacks, a route the
# reference page skipped, an entry the router does not serve, and a handler call it cannot consume. Every checker
# case runs against a throwaway tree, never the real site. The favicon case at the end reads the real tree.
set -euo pipefail

here=$(cd "$(dirname "$0")/../.." && pwd)
check="$here/scripts/check-docs.py"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail() { echo "FAIL $1" >&2; exit 1; }

mkdir -p "$tmp/site/good" "$tmp/site/assets"
: > "$tmp/site/assets/a.png"
cat > "$tmp/site/index.html" <<'HTML'
<h1 id="top">top</h1><a href="good/">ok</a><a href="#top">ok</a><a href="https://example.com/x">ok</a>
<img src="assets/a.png#only-light"><img src="assets/a.png#only-dark">
HTML
echo '<a href="../">ok</a><a href="/phoebis/assets/a.png">ok</a>' > "$tmp/site/good/index.html"
python3 "$check" links "$tmp/site" >/dev/null || fail "a site whose links all resolve was refused"
echo '<a href="missing/">gone</a>' >> "$tmp/site/good/index.html"
out=$(python3 "$check" links "$tmp/site" 2>&1) && fail "a dangling link passed"
echo "$out" | grep -q "resolves to nothing" || fail "dangling link did not report resolves to nothing: $out"

# Each case below runs against its own throwaway site so the finding it checks for cannot be satisfied by
# a leftover dangling link from an earlier case.

# A root-relative link whose first segment is not the site's own prefix must not fall back to site/, or
# every such link would resolve to the site root's index.html.
mkdir -p "$tmp/prefix-site"
echo '<a href="/anything/">gone</a>' > "$tmp/prefix-site/index.html"
out=$(python3 "$check" links "$tmp/prefix-site" 2>&1) && fail "a root-relative link with an unrelated prefix passed"
echo "$out" | grep -q "index.html: /anything/ resolves to nothing" || fail "wrong finding for unrelated prefix: $out"

# A root-relative link missing the site's own prefix must fail even when an unprefixed path of the same name
# exists directly under site/, which is exactly how the built tree is laid out locally.
mkdir -p "$tmp/unprefixed-site/installation"
: > "$tmp/unprefixed-site/installation/index.html"
echo '<a href="/installation/">gone</a>' > "$tmp/unprefixed-site/index.html"
out=$(python3 "$check" links "$tmp/unprefixed-site" 2>&1) && fail "a root-relative link without the site prefix passed"
echo "$out" | grep -q "index.html: /installation/ resolves to nothing" || fail "wrong finding for unprefixed link: $out"

# The same link, carrying the site's own prefix, must resolve.
mkdir -p "$tmp/prefixed-site/installation"
: > "$tmp/prefixed-site/installation/index.html"
echo '<a href="/phoebis/installation/">ok</a>' > "$tmp/prefixed-site/index.html"
python3 "$check" links "$tmp/prefixed-site" >/dev/null || fail "a root-relative link with the site prefix was refused"

# A root-relative link into a directory that has no index.html must not fall back past its own path.
mkdir -p "$tmp/missing-dir-site"
echo '<a href="/x/index.html">gone</a>' > "$tmp/missing-dir-site/index.html"
out=$(python3 "$check" links "$tmp/missing-dir-site" 2>&1) && fail "a link into a missing directory passed"
echo "$out" | grep -q "index.html: /x/index.html resolves to nothing" || fail "wrong finding for missing directory: $out"

# A relative link must not be allowed to climb out of site/ onto the repository tree.
mkdir -p "$tmp/escape-site"
echo '<a href="../README.md">gone</a>' > "$tmp/escape-site/index.html"
out=$(python3 "$check" links "$tmp/escape-site" 2>&1) && fail "a relative link escaping site/ passed"
echo "$out" | grep -q "index.html: ../README.md resolves to nothing" || fail "wrong finding for escaping link: $out"

# A fragment is read against the ids of the page it lands on: the linked page's for `other/#x`, its own for `#x`.
mkdir -p "$tmp/fragment-site/other"
echo '<h3 id="here">here</h3><a href="other/#there">ok</a><a href="#here">ok</a>' > "$tmp/fragment-site/index.html"
echo '<h2 id="there">there</h2>' > "$tmp/fragment-site/other/index.html"
python3 "$check" links "$tmp/fragment-site" >/dev/null || fail "a site whose fragments all resolve was refused"
echo '<a href="other/#missing">gone</a>' >> "$tmp/fragment-site/index.html"
out=$(python3 "$check" links "$tmp/fragment-site" 2>&1) && fail "a fragment the linked page lacks passed"
echo "$out" | grep -q "index.html: other/#missing names no heading or anchor in other/index.html" \
  || fail "wrong finding for a fragment the linked page lacks: $out"

# A bare fragment stays on its own page, so that page's ids are the ones it must name.
mkdir -p "$tmp/bare-fragment-site"
echo '<h2 id="here">here</h2><a href="#nowhere">gone</a>' > "$tmp/bare-fragment-site/index.html"
out=$(python3 "$check" links "$tmp/bare-fragment-site" 2>&1) && fail "a bare fragment its own page lacks passed"
echo "$out" | grep -q "index.html: #nowhere names no heading or anchor in index.html" \
  || fail "wrong finding for a bare fragment its own page lacks: $out"

# A file that is not a page carries no ids, and a binary one would not decode, so the checker skips its fragment.
mkdir -p "$tmp/asset-fragment-site/assets"
printf '\x89PNG\r\n\x1a\n' > "$tmp/asset-fragment-site/assets/a.png"
echo '<h1 id="top">top</h1><a href="assets/a.png#x">ok</a>' > "$tmp/asset-fragment-site/index.html"
out=$(python3 "$check" links "$tmp/asset-fragment-site" 2>&1) \
  || fail "a fragment on a link to a file that is not a page was read: $out"

# The theme writes its skip link as `#__skip` on a page with no heading to point at, so the checker exempts it.
mkdir -p "$tmp/skip-link-site"
echo '<a href="#__skip">skip</a><p>not found</p>' > "$tmp/skip-link-site/404.html"
out=$(python3 "$check" links "$tmp/skip-link-site" 2>&1) || fail "the theme's skip link was refused: $out"

cat > "$tmp/routes.rs" <<'RS'
    Router::new()
        .route("/admin/a", get(a).post(b))
        .route(
            "/admin/a/{id}",
            patch(c).delete(d),
        )
        .route("/admin/{*rest}", any(e))
        .layer(x)
RS
{
  printf '### GET /admin/a\n### POST /admin/a\n### PATCH /admin/a/{id}\n'
  printf '### DELETE /admin/a/{id}\n### ANY /admin/{*rest}\n'
} > "$tmp/api.md"
python3 "$check" routes "$tmp/routes.rs" "$tmp/api.md" >/dev/null || fail "a matching reference was refused"
sed -i '/^### DELETE/d' "$tmp/api.md"
out=$(python3 "$check" routes "$tmp/routes.rs" "$tmp/api.md" 2>&1) && fail "a missing entry passed"
echo "$out" | grep -q "has no entry for" || fail "missing entry did not report has no entry for: $out"
printf '### DELETE /admin/a/{id}\n### GET /admin/nowhere\n' >> "$tmp/api.md"
out=$(python3 "$check" routes "$tmp/routes.rs" "$tmp/api.md" 2>&1) && fail "a stray entry passed"
echo "$out" | grep -q "which the router does not serve" || fail "stray entry did not report which the router does not serve: $out"

cat > "$tmp/routes-head.rs" <<'RS'
    Router::new()
        .route("/admin/b", head(f))
        .layer(x)
RS
: > "$tmp/api-head.md"
out=$(python3 "$check" routes "$tmp/routes-head.rs" "$tmp/api-head.md" 2>&1) && fail "an undocumented head route passed"
echo "$out" | grep -q "has no entry for" || fail "undocumented head route did not report has no entry for: $out"

cat > "$tmp/routes-unknown.rs" <<'RS'
    Router::new()
        .route("/admin/c", weird(g))
        .layer(x)
RS
: > "$tmp/api-unknown.md"
out=$(python3 "$check" routes "$tmp/routes-unknown.rs" "$tmp/api-unknown.md" 2>&1) && \
  fail "a handler call the checker cannot consume passed"
echo "$out" | grep -q "is not a method this checker consumes" || fail "unknown handler did not report expected finding: $out"

cat > "$tmp/routes-prebuilt.rs" <<'RS'
    Router::new()
        .route("/admin/new", extra_routes)
        .layer(x)
RS
: > "$tmp/api-prebuilt.md"
out=$(python3 "$check" routes "$tmp/routes-prebuilt.rs" "$tmp/api-prebuilt.md" 2>&1) && \
  fail "a route whose handler expression has no recognized call passed"
echo "$out" | grep -q "/admin/new" || fail "unrecognized handler expression did not name the route: $out"

# The last route of a table is followed by whatever the router chains next, not always another route or a
# layer; the parser must read it whatever follows.
cat > "$tmp/routes-merge.rs" <<'RS'
    Router::new()
        .route("/admin/a", get(a))
        .route("/admin/last", delete(b))
        .merge(other)
        .with_state(state)
RS
: > "$tmp/api-merge.md"
out=$(python3 "$check" routes "$tmp/routes-merge.rs" "$tmp/api-merge.md" 2>&1) && fail "a table ending in merge passed with no entries"
echo "$out" | grep -q "DELETE /admin/last" || fail "the last route before merge was dropped: $out"

# A route in a comment is not served.
cat > "$tmp/routes-comment.rs" <<'RS'
    Router::new()
        // .route("/admin/gone", get(x))
        .route("/admin/a", get(a))
        .layer(x)
RS
printf '### GET /admin/a\n' > "$tmp/api-comment.md"
python3 "$check" routes "$tmp/routes-comment.rs" "$tmp/api-comment.md" >/dev/null || fail "a commented-out route was read as served"

# A path in a quoted string that carries a parenthesis must not end the call early.
cat > "$tmp/routes-paren.rs" <<'RS'
    Router::new()
        .route("/admin/x", get(|| async { ")" }))
        .layer(x)
RS
printf '### GET /admin/x\n' > "$tmp/api-paren.md"
python3 "$check" routes "$tmp/routes-paren.rs" "$tmp/api-paren.md" >/dev/null || fail "a parenthesis inside a string broke the parse"

# The proxy listener: a constant path is read, and the client page's table must match.
cat > "$tmp/proxy.rs" <<'RS'
pub const EVENTS_PATH: &str = "/v0/x/events";
    Router::new()
        .route(EVENTS_PATH, get(events))
        .route("/v1/messages", post(messages))
        .with_state(engine)
RS
cat > "$tmp/client.md" <<'MD'
| Method | Path | Credential | Answer |
| --- | --- | --- | --- |
| GET | `/v0/x/events` | router key | the report |
| POST | `/v1/messages` | router key | relayed |
| any unmatched path | | none | `404` |
MD
python3 "$check" client "$tmp/proxy.rs" "$tmp/client.md" >/dev/null || fail "a matching client table was refused"
sed -i '/v0\/x\/events/d' "$tmp/client.md"
out=$(python3 "$check" client "$tmp/proxy.rs" "$tmp/client.md" 2>&1) && fail "a missing client row passed"
echo "$out" | grep -q "has no row for GET /v0/x/events" || fail "missing client row did not report: $out"
printf '| GET | `/v1/nowhere` | none | `404` |\n' >> "$tmp/client.md"
out=$(python3 "$check" client "$tmp/proxy.rs" "$tmp/client.md" 2>&1) && fail "a stray client row passed"
echo "$out" | grep -q "which the proxy listener does not serve" || fail "stray client row did not report: $out"

# A route file without the page to check it against is a mistake rather than a default: the check says how it
# is called and exits 2, the way it does for an unknown command.
out=$(python3 "$check" client "$tmp/proxy.rs" 2>&1) && fail "client with one argument passed"
status=$?
[ "$status" = 2 ] || fail "client with one argument exited $status rather than 2: $out"
echo "$out" | grep -q "usage: check-docs.py" || fail "client with one argument did not print the usage: $out"

# A route file the list names but the tree does not hold fails the check: narrowing the served set silently
# is how a route slips past the page.
out=$(python3 "$check" client "$tmp/gone.rs" "$tmp/client.md" 2>&1) && fail "a missing route file passed"
echo "$out" | grep -q "is missing; the proxy route table names it" || fail "missing route file did not report: $out"

# The six cases below each pair a route table with the reference that lists exactly its served set: a pass
# proves the set (a missing and a stray entry are both findings), and a second reference proves the route the
# parser must not serve is reported as unserved.

# A route inside a block comment is not served, however the comment nests or spans lines.
cat > "$tmp/routes-block.rs" <<'RS'
    Router::new()
        /* .route("/admin/ghost", get(ghost))
           /* nested */ .route("/admin/ghost2", post(ghost)) */
        .route("/admin/a", get(a))
        .layer(x)
RS
printf '### GET /admin/a\n' > "$tmp/api-block.md"
out=$(python3 "$check" routes "$tmp/routes-block.rs" "$tmp/api-block.md" 2>&1) || fail "a block comment's route was read as served: $out"
printf '### GET /admin/ghost\n' >> "$tmp/api-block.md"
out=$(python3 "$check" routes "$tmp/routes-block.rs" "$tmp/api-block.md" 2>&1) && fail "a route that lives only in a block comment passed as documented"
echo "$out" | grep -q "documents GET /admin/ghost, which the router does not serve" || fail "the block comment's route was not reported as unserved: $out"

# An inline comment after a route on its own line: the route is served, the comment's route is not.
cat > "$tmp/routes-inline.rs" <<'RS'
    Router::new()
        .route("/admin/a", get(a)) // .route("/admin/ghost", get(ghost))
        .layer(x)
RS
printf '### GET /admin/a\n' > "$tmp/api-inline.md"
out=$(python3 "$check" routes "$tmp/routes-inline.rs" "$tmp/api-inline.md" 2>&1) || fail "a route followed by an inline comment was misread: $out"
printf '### GET /admin/ghost\n' >> "$tmp/api-inline.md"
out=$(python3 "$check" routes "$tmp/routes-inline.rs" "$tmp/api-inline.md" 2>&1) && fail "an inline comment's route passed as documented"
echo "$out" | grep -q "documents GET /admin/ghost, which the router does not serve" || fail "the inline comment's route was not reported as unserved: $out"

# A raw string path is read as its contents.
cat > "$tmp/routes-raw.rs" <<'RS'
    Router::new()
        .route(r#"/admin/x"#, get(a))
        .layer(x)
RS
printf '### GET /admin/x\n' > "$tmp/api-raw.md"
out=$(python3 "$check" routes "$tmp/routes-raw.rs" "$tmp/api-raw.md" 2>&1) || fail "a raw string path was not read as /admin/x: $out"

# An escaped quote inside a handler string ends neither the string nor the call.
cat > "$tmp/routes-escape.rs" <<'RS'
    Router::new()
        .route("/admin/q", get(|| async { "say \"hi\")" }))
        .layer(x)
RS
printf '### GET /admin/q\n' > "$tmp/api-escape.md"
out=$(python3 "$check" routes "$tmp/routes-escape.rs" "$tmp/api-escape.md" 2>&1) || fail "an escaped quote inside a handler string broke the parse: $out"

# A method name inside a string is not a call: the route is served as GET and nothing else.
cat > "$tmp/routes-string-method.rs" <<'RS'
    Router::new()
        .route("/admin/s", get(|| async { "delete(" }))
        .layer(x)
RS
printf '### GET /admin/s\n' > "$tmp/api-string-method.md"
out=$(python3 "$check" routes "$tmp/routes-string-method.rs" "$tmp/api-string-method.md" 2>&1) || fail "a method name inside a string was read as a call: $out"
printf '### DELETE /admin/s\n' >> "$tmp/api-string-method.md"
out=$(python3 "$check" routes "$tmp/routes-string-method.rs" "$tmp/api-string-method.md" 2>&1) && fail "DELETE /admin/s passed though only a string names delete"
echo "$out" | grep -q "documents DELETE /admin/s, which the router does not serve" || fail "the string's delete was not reported as unserved: $out"

# A quote inside a char literal is a character, not the start of a string: a table after one is still read,
# so a route the page leaves out is still reported rather than hidden with the rest of the file.
cat > "$tmp/routes-char.rs" <<'RS'
fn escape(c: char) -> &'static str {
    match c {
        '"' => "&quot;",
        '\'' => "&#39;",
        _ => "",
    }
}

    Router::new()
        .route("/admin/a", get(a))
        .layer(x)
RS
printf '### GET /admin/a\n' > "$tmp/api-char.md"
out=$(python3 "$check" routes "$tmp/routes-char.rs" "$tmp/api-char.md" 2>&1) || fail "a route after a char literal was misread: $out"
: > "$tmp/api-char.md"
out=$(python3 "$check" routes "$tmp/routes-char.rs" "$tmp/api-char.md" 2>&1) && fail "a route a char literal hid passed undocumented"
echo "$out" | grep -q "has no entry for GET /admin/a" || fail "the route after a char literal was not reported as undocumented: $out"

# Unlike the cases above this reads the real tree: the site's favicon is a copy of the console's, and nothing else
# keeps the two equal.
cmp -s "$here/web/public/favicon.svg" "$here/docs/site/assets/favicon.svg" || fail "docs/site/assets/favicon.svg differs from web/public/favicon.svg"

echo "check-docs: ok"
