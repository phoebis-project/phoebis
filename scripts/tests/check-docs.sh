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
echo '<a href="../">ok</a><a href="/prefix/assets/a.png">ok</a>' > "$tmp/site/good/index.html"
python3 "$check" links "$tmp/site" >/dev/null || fail "a site whose links all resolve was refused"
echo '<a href="missing/">gone</a>' >> "$tmp/site/good/index.html"
if python3 "$check" links "$tmp/site" >/dev/null 2>&1; then fail "a dangling link passed"; fi

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
if python3 "$check" routes "$tmp/routes.rs" "$tmp/api.md" >/dev/null 2>&1; then fail "a missing entry passed"; fi
printf '### DELETE /admin/a/{id}\n### GET /admin/nowhere\n' >> "$tmp/api.md"
if python3 "$check" routes "$tmp/routes.rs" "$tmp/api.md" >/dev/null 2>&1; then fail "a stray entry passed"; fi

cat > "$tmp/routes-head.rs" <<'RS'
    Router::new()
        .route("/admin/b", head(f))
        .layer(x)
RS
: > "$tmp/api-head.md"
if python3 "$check" routes "$tmp/routes-head.rs" "$tmp/api-head.md" >/dev/null 2>&1
then fail "an undocumented head route passed"; fi

cat > "$tmp/routes-unknown.rs" <<'RS'
    Router::new()
        .route("/admin/c", weird(g))
        .layer(x)
RS
: > "$tmp/api-unknown.md"
if python3 "$check" routes "$tmp/routes-unknown.rs" "$tmp/api-unknown.md" >/dev/null 2>&1
then fail "a handler call the checker cannot consume passed"; fi

echo "check-docs: ok"
