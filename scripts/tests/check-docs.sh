#!/usr/bin/env bash
# The checker has to fail on what it exists to catch: a dangling link, a fragment its page lacks, a route the
# client page skipped, a row the proxy listener does not serve, and a handler call it cannot consume. Every
# checker case runs against a throwaway tree, never the real site. The five content checks run against throwaway
# files in the same way: a page list short a line the binary prints, a code with no row, a quoted example that
# drifted, a quoted unit that drifted, and prose that breaks a rule. The favicon case and the unit case at the end
# read the real tree.
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

cat > "$tmp/routes-prebuilt.rs" <<'RS'
    Router::new()
        .route("/admin/new", extra_routes)
        .layer(x)
RS
: > "$tmp/client-prebuilt.md"
out=$(python3 "$check" client "$tmp/routes-prebuilt.rs" "$tmp/client-prebuilt.md" 2>&1) && \
  fail "a route whose handler expression has no recognized call passed"
echo "$out" | grep -q "/admin/new" || fail "unrecognized handler expression did not name the route: $out"

# A call to a method the checker does not know serves the route all the same, so it fails rather than drops out.
cat > "$tmp/routes-unknown.rs" <<'RS'
    Router::new()
        .route("/v1/odd", weird(g))
        .with_state(engine)
RS
: > "$tmp/client-unknown.md"
out=$(python3 "$check" client "$tmp/routes-unknown.rs" "$tmp/client-unknown.md" 2>&1) && \
  fail "a handler call the checker does not consume passed"
echo "$out" | grep -q "weird(...) on /v1/odd is not a method this checker consumes" \
  || fail "the unknown handler call did not report: $out"

# The last route of a table is followed by whatever the router chains next, not always another route or a
# layer; the parser must read it whatever follows.
cat > "$tmp/routes-merge.rs" <<'RS'
    Router::new()
        .route("/admin/a", get(a))
        .route("/admin/last", delete(b))
        .merge(other)
        .with_state(state)
RS
: > "$tmp/client-merge.md"
out=$(python3 "$check" client "$tmp/routes-merge.rs" "$tmp/client-merge.md" 2>&1) && fail "a table ending in merge passed with no rows"
echo "$out" | grep -q "DELETE /admin/last" || fail "the last route before merge was dropped: $out"

# A route in a comment is not served.
cat > "$tmp/routes-comment.rs" <<'RS'
    Router::new()
        // .route("/admin/gone", get(x))
        .route("/admin/a", get(a))
        .layer(x)
RS
printf '| GET | `/admin/a` |\n' > "$tmp/client-comment.md"
python3 "$check" client "$tmp/routes-comment.rs" "$tmp/client-comment.md" >/dev/null || fail "a commented-out route was read as served"

# A path in a quoted string that carries a parenthesis must not end the call early.
cat > "$tmp/routes-paren.rs" <<'RS'
    Router::new()
        .route("/admin/x", get(|| async { ")" }))
        .layer(x)
RS
printf '| GET | `/admin/x` |\n' > "$tmp/client-paren.md"
python3 "$check" client "$tmp/routes-paren.rs" "$tmp/client-paren.md" >/dev/null || fail "a parenthesis inside a string broke the parse"

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

# A moved client page is a finding that names it, as a missing route file is, never a traceback.
out=$(python3 "$check" client "$tmp/proxy.rs" "$tmp/moved-client.md" 2>&1) && fail "a missing client page passed"
echo "$out" | grep -q "moved-client.md is missing" || fail "the missing client page did not report: $out"
echo "$out" | grep -q "Traceback" && fail "the missing client page ended in a traceback: $out"

# The six cases below each pair a route table with a client page whose table lists exactly its served set: a pass
# proves the set (a missing and a stray row are both findings), and a second page proves the route the parser must
# not serve is reported as unserved.

# A route inside a block comment is not served, however the comment nests or spans lines.
cat > "$tmp/routes-block.rs" <<'RS'
    Router::new()
        /* .route("/admin/ghost", get(ghost))
           /* nested */ .route("/admin/ghost2", post(ghost)) */
        .route("/admin/a", get(a))
        .layer(x)
RS
printf '| GET | `/admin/a` |\n' > "$tmp/client-block.md"
out=$(python3 "$check" client "$tmp/routes-block.rs" "$tmp/client-block.md" 2>&1) || fail "a block comment's route was read as served: $out"
printf '| GET | `/admin/ghost` |\n' >> "$tmp/client-block.md"
out=$(python3 "$check" client "$tmp/routes-block.rs" "$tmp/client-block.md" 2>&1) && fail "a route that lives only in a block comment passed as documented"
echo "$out" | grep -q "lists GET /admin/ghost, which the proxy listener does not serve" || fail "the block comment's route was not reported as unserved: $out"

# An inline comment after a route on its own line: the route is served, the comment's route is not.
cat > "$tmp/routes-inline.rs" <<'RS'
    Router::new()
        .route("/admin/a", get(a)) // .route("/admin/ghost", get(ghost))
        .layer(x)
RS
printf '| GET | `/admin/a` |\n' > "$tmp/client-inline.md"
out=$(python3 "$check" client "$tmp/routes-inline.rs" "$tmp/client-inline.md" 2>&1) || fail "a route followed by an inline comment was misread: $out"
printf '| GET | `/admin/ghost` |\n' >> "$tmp/client-inline.md"
out=$(python3 "$check" client "$tmp/routes-inline.rs" "$tmp/client-inline.md" 2>&1) && fail "an inline comment's route passed as documented"
echo "$out" | grep -q "lists GET /admin/ghost, which the proxy listener does not serve" || fail "the inline comment's route was not reported as unserved: $out"

# A raw string path is read as its contents.
cat > "$tmp/routes-raw.rs" <<'RS'
    Router::new()
        .route(r#"/admin/x"#, get(a))
        .layer(x)
RS
printf '| GET | `/admin/x` |\n' > "$tmp/client-raw.md"
out=$(python3 "$check" client "$tmp/routes-raw.rs" "$tmp/client-raw.md" 2>&1) || fail "a raw string path was not read as /admin/x: $out"

# An escaped quote inside a handler string ends neither the string nor the call.
cat > "$tmp/routes-escape.rs" <<'RS'
    Router::new()
        .route("/admin/q", get(|| async { "say \"hi\")" }))
        .layer(x)
RS
printf '| GET | `/admin/q` |\n' > "$tmp/client-escape.md"
out=$(python3 "$check" client "$tmp/routes-escape.rs" "$tmp/client-escape.md" 2>&1) || fail "an escaped quote inside a handler string broke the parse: $out"

# A method name inside a string is not a call: the route is served as GET and nothing else.
cat > "$tmp/routes-string-method.rs" <<'RS'
    Router::new()
        .route("/admin/s", get(|| async { "delete(" }))
        .layer(x)
RS
printf '| GET | `/admin/s` |\n' > "$tmp/client-string-method.md"
out=$(python3 "$check" client "$tmp/routes-string-method.rs" "$tmp/client-string-method.md" 2>&1) || fail "a method name inside a string was read as a call: $out"
printf '| DELETE | `/admin/s` |\n' >> "$tmp/client-string-method.md"
out=$(python3 "$check" client "$tmp/routes-string-method.rs" "$tmp/client-string-method.md" 2>&1) && fail "DELETE /admin/s passed though only a string names delete"
echo "$out" | grep -q "lists DELETE /admin/s, which the proxy listener does not serve" || fail "the string's delete was not reported as unserved: $out"

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
printf '| GET | `/admin/a` |\n' > "$tmp/client-char.md"
out=$(python3 "$check" client "$tmp/routes-char.rs" "$tmp/client-char.md" 2>&1) || fail "a route after a char literal was misread: $out"
: > "$tmp/client-char.md"
out=$(python3 "$check" client "$tmp/routes-char.rs" "$tmp/client-char.md" 2>&1) && fail "a route a char literal hid passed undocumented"
echo "$out" | grep -q "has no row for GET /admin/a" || fail "the route after a char literal was not reported as undocumented: $out"

# The quoted example: equal passes, a changed line names both line numbers, and a block one line short of the file, no
# block, two blocks and a missing file fail.
mkdir -p "$tmp/example"
printf 'a: 1\n# a comment\nb: 2\n' > "$tmp/example/config.example.yaml"
cat > "$tmp/example/page.md" <<'MD'
# Page

```yaml title="deploy/config.example.yaml"
a: 1
# a comment
b: 2
```
MD
python3 "$check" example "$tmp/example/config.example.yaml" "$tmp/example/page.md" >/dev/null || fail "an equal example block was refused"
sed -i 's/^# a comment$/# another comment/' "$tmp/example/page.md"
out=$(python3 "$check" example "$tmp/example/config.example.yaml" "$tmp/example/page.md" 2>&1) && fail "a changed example line passed"
echo "$out" | grep -q "page.md:5: differs from config.example.yaml:2" || fail "the changed line did not name both line numbers: $out"
printf '# Page\n\n```yaml title="deploy/config.example.yaml"\na: 1\n# a comment\n```\n' > "$tmp/example/page.md"
out=$(python3 "$check" example "$tmp/example/config.example.yaml" "$tmp/example/page.md" 2>&1) && fail "an example block one line short of the file passed"
echo "$out" | grep -q "page.md:4: the block has 2 lines and config.example.yaml 3" || fail "the short block did not report both lengths: $out"
printf '# Page\n\nno block\n' > "$tmp/example/page.md"
out=$(python3 "$check" example "$tmp/example/config.example.yaml" "$tmp/example/page.md" 2>&1) && fail "a page without the example block passed"
echo "$out" | grep -q "0 fenced blocks titled deploy/config.example.yaml" || fail "the missing block did not report: $out"
printf '```yaml title="deploy/config.example.yaml"\na: 1\n```\n\n```yaml title="deploy/config.example.yaml"\na: 1\n```\n' > "$tmp/example/page.md"
out=$(python3 "$check" example "$tmp/example/config.example.yaml" "$tmp/example/page.md" 2>&1) && fail "a page with two example blocks passed"
echo "$out" | grep -q "2 fenced blocks titled deploy/config.example.yaml" || fail "the second block did not report: $out"
out=$(python3 "$check" example "$tmp/example/gone.yaml" "$tmp/example/page.md" 2>&1) && fail "an example file that does not exist passed"
echo "$out" | grep -q "gone.yaml is missing" || fail "the missing example file did not report: $out"

# The quoted unit: equal passes, a block one line short names both lengths, a drifted line names both line numbers,
# and a page without the block fails.
mkdir -p "$tmp/unit"
printf '[Service]\n# a comment\nUMask=0077\n' > "$tmp/unit/phoebis.service"
cat > "$tmp/unit/page.md" <<'MD'
# Page

```ini title="deploy/phoebis.service"
[Service]
# a comment
UMask=0077
```
MD
python3 "$check" unit "$tmp/unit/phoebis.service" "$tmp/unit/page.md" >/dev/null || fail "an equal unit block was refused"
sed -i 's/^UMask=0077$/UMask=0022/' "$tmp/unit/page.md"
out=$(python3 "$check" unit "$tmp/unit/phoebis.service" "$tmp/unit/page.md" 2>&1) && fail "a drifted unit line passed"
echo "$out" | grep -q "page.md:6: differs from phoebis.service:3" || fail "the drifted line did not name both line numbers: $out"
printf '# Page\n\n```ini title="deploy/phoebis.service"\n[Service]\n# a comment\n```\n' > "$tmp/unit/page.md"
out=$(python3 "$check" unit "$tmp/unit/phoebis.service" "$tmp/unit/page.md" 2>&1) && fail "a unit block one line short of the file passed"
echo "$out" | grep -q "page.md:4: the block has 2 lines and phoebis.service 3" || fail "the short unit block did not report both lengths: $out"
printf '# Page\n\nno block\n' > "$tmp/unit/page.md"
out=$(python3 "$check" unit "$tmp/unit/phoebis.service" "$tmp/unit/page.md" 2>&1) && fail "a page without the unit block passed"
echo "$out" | grep -q "0 fenced blocks titled deploy/phoebis.service" || fail "the missing unit block did not report: $out"

# The check-config list: the arm's lines read statically, the unconditional ones in order, a conditional one later.
mkdir -p "$tmp/check-config"
cat > "$tmp/check-config/main.rs" <<'RS'
fn main() {
    match cli.command {
        Some(Command::CheckConfig) => {
            println!("{}", describe("proxy"));
            println!(
                "proxy: body budget {} bytes, max request {} bytes, \
                 upstream gzip {}",
                a, b, c
            );
            println!("ledger at {}", path);
            match canary {
                Canary::Missing => println!("registry at {} does not exist yet", path),
                Canary::Present => {}
            }
            if !overlaps.is_empty() {
                for overlap in &overlaps {
                    println!("{overlap}");
                }
            }
            Ok(())
        }
        Some(Command::Other) => {
            println!("never read");
        }
    }
}
RS
cat > "$tmp/check-config/page.md" <<'MD'
It prints, in order:

<!-- check-config: begin -->
1. `proxy listens on {addr}`
2. `proxy: body budget {n} bytes, max request {n} bytes,
   upstream gzip {auto|on|off}`
3. `ledger at {path}`
4. `registry at {path} does not exist yet`, when the registry is missing
<!-- check-config: end -->
MD
python3 "$check" check-config "$tmp/check-config/main.rs" "$tmp/check-config/page.md" >/dev/null || fail "a matching check-config list was refused"
sed '/^3\. /d' "$tmp/check-config/page.md" > "$tmp/check-config/short.md"
out=$(python3 "$check" check-config "$tmp/check-config/main.rs" "$tmp/check-config/short.md" 2>&1) && fail "a list short an unconditional line passed"
echo "$out" | grep -q "short.md:7: item 3 does not match the line 'ledger at {}'" || fail "the short list did not report item 3: $out"
cat > "$tmp/check-config/order.md" <<'MD'
<!-- check-config: begin -->
1. `proxy listens on {addr}`
2. `ledger at {path}`
3. `proxy: body budget {n} bytes, max request {n} bytes, upstream gzip {auto|on|off}`
4. `registry at {path} does not exist yet`, when the registry is missing
<!-- check-config: end -->
MD
out=$(python3 "$check" check-config "$tmp/check-config/main.rs" "$tmp/check-config/order.md" 2>&1) && fail "a list out of order passed"
echo "$out" | grep -q "order.md:3: item 2 does not match the line 'proxy: body budget" || fail "the reordered list did not report item 2: $out"
sed 's/^<!-- check-config: end -->$/5. `something the binary never prints`\n<!-- check-config: end -->/' "$tmp/check-config/page.md" > "$tmp/check-config/extra.md"
out=$(python3 "$check" check-config "$tmp/check-config/main.rs" "$tmp/check-config/extra.md" 2>&1) && fail "a list with an unmatched item passed"
echo "$out" | grep -q "extra.md:9: item 5 matches no line the binary prints" || fail "the unmatched item did not report: $out"
grep -v 'check-config: ' "$tmp/check-config/page.md" > "$tmp/check-config/unmarked.md"
out=$(python3 "$check" check-config "$tmp/check-config/main.rs" "$tmp/check-config/unmarked.md" 2>&1) && fail "a page without the markers passed"
echo "$out" | grep -q "expected one <!-- check-config: begin --> before one <!-- check-config: end -->, found 0 and 0" || fail "the missing markers did not report: $out"
sed '/^[34]\. /d' "$tmp/check-config/page.md" > "$tmp/check-config/two.md"
out=$(python3 "$check" check-config "$tmp/check-config/main.rs" "$tmp/check-config/two.md" 2>&1) && fail "a list short its last two items passed"
echo "$out" | grep -q "two.md: no item for the line 'ledger at {}', which the binary prints as line 3" || fail "the list short an unconditional line did not name it: $out"
echo "$out" | grep -q "two.md: no item for the conditional line 'registry at {} does not exist yet'" || fail "the list short a conditional line did not name it: $out"
out=$(python3 "$check" check-config "$tmp/check-config/gone.rs" "$tmp/check-config/page.md" 2>&1) && fail "a main.rs that does not exist passed"
echo "$out" | grep -q "gone.rs is missing" || fail "the missing main.rs did not report: $out"
out=$(python3 "$check" check-config "$tmp/check-config/main.rs" 2>&1) && fail "check-config with one argument passed"
status=$?
[ "$status" = 2 ] || fail "check-config with one argument exited $status rather than 2: $out"
echo "$out" | grep -qF "check-config [MAIN_RS INSTALLATION_MD]" || fail "check-config with one argument did not print the new usage: $out"

# The refusal codes: a literal, a raw literal, a const resolved across files, an anthropic_error( call, an auth_*
# tuple, the self.code forward, and an expression the check cannot read.
mkdir -p "$tmp/codes"
cat > "$tmp/codes/shape.rs" <<'RS'
/// The one spelling both crates read.
pub const SHAPE_CODE: &str = "body_shape";
Failure { code: "ledger_behind", message: "the router is not accepting requests right now" }
Failure { code: r"raw_code", message: "a raw literal reads the same" }
RS
cat > "$tmp/codes/errors.rs" <<'RS'
let (code, message) = match error {
    AuthError::Missing => ("auth_missing", "a key is required"),
};
Failure { status, error_type: "invalid_request_error", code: other::SHAPE_CODE, message: "x" }
// code: "in_a_comment",
no_store(anthropic_error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", "usage_unavailable", "z", id))
pub fn anthropic_error(status: StatusCode, error_type: &str, code: &str, message: &str) {}
let forwarded = anthropic_error(self.status, self.error_type, self.code, self.message, id);
RS
cat > "$tmp/codes/client.md" <<'MD'
<!-- refusal-codes: begin -->
| Code | Status | Type | Messages | What to do |
| --- | --- | --- | --- | --- |
| `auth_missing` | 401 | `authentication_error` | `a key is required` | Set the key. |
| `body_shape` | 400 | `invalid_request_error` | `x` | Send Claude Code's shape. |
| `ledger_behind` | 503 | `api_error` | `the router is not accepting requests right now` | Wait. |
| `raw_code` | 500 | `api_error` | `a raw literal reads the same` | Retry. |
| `usage_unavailable` | 500 | `api_error` | `z` | Retry. |
<!-- refusal-codes: end -->
MD
python3 "$check" codes "$tmp/codes/errors.rs" "$tmp/codes/shape.rs" "$tmp/codes/client.md" >/dev/null || fail "a matching refusal table was refused"
sed -i '/`body_shape`/d' "$tmp/codes/client.md"
out=$(python3 "$check" codes "$tmp/codes/errors.rs" "$tmp/codes/shape.rs" "$tmp/codes/client.md" 2>&1) && fail "a table missing a code passed"
echo "$out" | grep -q "client.md has no row for body_shape" || fail "the missing code did not report: $out"
sed -i '/refusal-codes: end/i | `body_shape` | 400 | `invalid_request_error` | `x` | y |' "$tmp/codes/client.md"
python3 "$check" codes "$tmp/codes/errors.rs" "$tmp/codes/shape.rs" "$tmp/codes/client.md" >/dev/null || fail "the restored body_shape row was refused"
sed -i '/refusal-codes: end/i | `made_up` | 400 | `x` | `y` | z |' "$tmp/codes/client.md"
out=$(python3 "$check" codes "$tmp/codes/errors.rs" "$tmp/codes/shape.rs" "$tmp/codes/client.md" 2>&1) && fail "a table naming a code no file answers passed"
echo "$out" | grep -q "client.md lists made_up, which the proxy listener does not answer" || fail "the stray code did not report: $out"
sed -i '/`made_up`/d' "$tmp/codes/client.md"
echo 'Failure { code: NOWHERE, message: "q" }' >> "$tmp/codes/errors.rs"
out=$(python3 "$check" codes "$tmp/codes/errors.rs" "$tmp/codes/shape.rs" "$tmp/codes/client.md" 2>&1) && fail "a code that resolves to no const passed"
echo "$out" | grep -q "errors.rs:9: code NOWHERE resolves to no const in the files read" || fail "the unresolved code did not report: $out"
echo 'Failure { code: format!("{kind}_refused"), message: "u" }' >> "$tmp/codes/errors.rs"
out=$(python3 "$check" codes "$tmp/codes/errors.rs" "$tmp/codes/shape.rs" "$tmp/codes/client.md" 2>&1) && fail "a code expression the check cannot read passed"
echo "$out" | grep -qF 'errors.rs:10: unsupported code expression format!("{kind}_refused")' || fail "the unreadable code did not report: $out"
out=$(python3 "$check" codes "$tmp/codes/errors.rs" 2>&1) && fail "codes with one argument passed"
status=$?
[ "$status" = 2 ] || fail "codes with one argument exited $status rather than 2: $out"
echo "$out" | grep -qF "codes [RS_FILE... CLIENT_MD]" || fail "codes with one argument did not print the new usage: $out"
# The codes check reads the same client page, so a moved one is named the same way.
out=$(python3 "$check" codes "$tmp/codes/errors.rs" "$tmp/codes/shape.rs" "$tmp/codes/moved-client.md" 2>&1) && fail "codes passed a missing client page"
echo "$out" | grep -q "moved-client.md is missing" || fail "codes did not report the missing client page: $out"
echo "$out" | grep -q "Traceback" && fail "codes ended in a traceback on a missing client page: $out"

# The prose rules: each rule fires outside a code span and a fenced block and stays quiet inside them, a fence
# closes only on its own character and length, and a span wrapped across two lines is code on both.
mkdir -p "$tmp/prose"
{
  echo '# A page'
  echo
  echo 'A `code; span`, an `api_key` in a span, a `task` in a span and the shape a client sends all pass.'
  printf '| %s |\n' "$(printf 'x%.0s' $(seq 1 117))"
  echo '<!-- a comment; with a semicolon and a task -->'
  echo
  echo '```sh'
  echo 'echo "a fenced line; with a semicolon, an em-dash — and a task"'
  echo '```'
  echo '````markdown'
  echo '```sh'
  echo 'a line; inside a four-backtick block, with a task'
  echo '```'
  echo '````'
  echo 'A span that wraps `git'
  echo 'worktree list; with a semicolon` and ends.'
} > "$tmp/prose/pass.md"
[ "$(sed -n 4p "$tmp/prose/pass.md" | wc -c)" = 122 ] || fail "the table row case is not 121 columns"
python3 "$check" prose "$tmp/prose/pass.md" >/dev/null || fail "prose refused a page that obeys every rule"
{
  echo 'A prose line; with a semicolon.'
  printf '%s\n' "$(printf 'x%.0s' $(seq 1 121))"
  echo 'An em-dash — here.'
  echo 'An en-dash – here.'
  echo 'An api_key account.'
  echo 'A span that wraps `git'
  echo 'worktree list` and then; a semicolon after the close.'
} > "$tmp/prose/fail.md"
out=$(python3 "$check" prose "$tmp/prose/fail.md" 2>&1) && fail "prose passed a page that breaks the rules"
for finding in 'fail.md:1: semicolon:' 'fail.md:2: columns: 121' 'fail.md:3: dash:' 'fail.md:4: dash:' 'fail.md:5: api_key:' 'fail.md:7: semicolon:'; do
  echo "$out" | grep -q "$finding" || fail "prose did not report $finding: $out"
done
echo "$out" | grep -q 'fail.md:6:' && fail "prose reported the line that opens a wrapped span: $out"
for word in sub-project subproject ruling rulings task tasks finding findings worktree worktrees workstream 'item 12' 'shape B' 'option 2'; do
  echo "The $word is named here." > "$tmp/prose/word.md"
  out=$(python3 "$check" prose "$tmp/prose/word.md" 2>&1) && fail "prose passed the word $word in prose"
  echo "$out" | grep -q "word.md:1: vocabulary:" || fail "prose did not report $word as vocabulary: $out"
  echo "The \`$word\` is named here." > "$tmp/prose/word.md"
  python3 "$check" prose "$tmp/prose/word.md" >/dev/null || fail "prose refused the word $word inside a code span"
done
# A span left open ends with its paragraph: a blank line or a fence line closes it, so the prose after either is read.
{
  echo 'A span opened here `and never closed'
  echo
  echo 'A prose line after the blank; with a semicolon.'
} > "$tmp/prose/open-span.md"
out=$(python3 "$check" prose "$tmp/prose/open-span.md" 2>&1) && fail "prose passed the semicolon after a span left open before a blank line"
echo "$out" | grep -q 'open-span.md:3: semicolon:' || fail "prose did not report the line after a blank that ends an open span: $out"
{
  echo 'A span opened here `and never closed'
  echo '```sh'
  echo 'echo fenced'
  echo '```'
  echo 'A prose line after the block; with a semicolon.'
} > "$tmp/prose/open-span-fence.md"
out=$(python3 "$check" prose "$tmp/prose/open-span-fence.md" 2>&1) && fail "prose passed the semicolon after a span left open before a fence"
echo "$out" | grep -q 'open-span-fence.md:5: semicolon:' || fail "prose did not report the line after a block that ends an open span: $out"
# A fence line with an info string inside an open block is content, so only a bare fence line closes the block.
{
  echo '```markdown'
  echo '```sh'
  echo 'a line inside the block'
  echo '```'
  echo 'A prose line after the block; with a semicolon.'
} > "$tmp/prose/inner-fence.md"
out=$(python3 "$check" prose "$tmp/prose/inner-fence.md" 2>&1) && fail "prose passed the prose after a block holding a fence line with an info string"
echo "$out" | grep -q 'inner-fence.md:5: semicolon:' || fail "prose did not report the line after the block's bare closing fence: $out"

# Unlike the cases above this reads the real tree: the site's favicon is a copy of the console's, and nothing else
# keeps the two equal.
cmp -s "$here/web/public/favicon.svg" "$here/docs/site/assets/favicon.svg" || fail "docs/site/assets/favicon.svg differs from web/public/favicon.svg"
# The real tree too: the installation page quotes the shipped unit whole.
out=$(python3 "$check" unit 2>&1) || fail "the installation page's unit block differs from deploy/phoebis.service: $out"

echo "check-docs: ok"
