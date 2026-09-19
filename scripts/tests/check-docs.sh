#!/usr/bin/env bash
# The checker has to fail on what it exists to catch: a dangling link, a route the reference page skipped, an
# entry the router does not serve, and a handler call it cannot consume. Every case runs against a throwaway
# tree, never the real site.
set -euo pipefail

here=$(cd "$(dirname "$0")/../.." && pwd)
check="$here/scripts/check-docs.py"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail() { echo "FAIL $1" >&2; exit 1; }

mkdir -p "$tmp/site/good" "$tmp/site/assets"
: > "$tmp/site/assets/a.png"
cat > "$tmp/site/index.html" <<'HTML'
<a href="good/">ok</a><a href="#top">ok</a><a href="https://example.com/x">ok</a>
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

echo "check-docs: ok"
