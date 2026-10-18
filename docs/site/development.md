# Development

This page is for building the router from its sources, running the tests and the browser pass, regenerating the
fixtures, re-pinning the client build, checking this site, and releasing. What a drift refusal is and what ends it is on
[Operations](operations.md#re-pinning-the-client-build), and the relay's own checks are on
[The relay](relay.md#client-drift).

## Build

```sh
cargo build --release
```

The binary lands at `target/release/phoebis`, as [Installation](installation.md#build) says for the operator who builds
from source. `rust-toolchain.toml` pins the channel to 1.96.1 with the `clippy` and `rustfmt` components, which `rustup
toolchain install` reads. BoringSSL builds from source through the `boring` and `boring-sys` crates, both at 5.2.0, and
its bindings need clang, cmake, libclang and pkg-config on the host.

The console is embedded at build time. The UI crate's build script builds `web/dist` when the bundle is missing or older
than its sources: it checks `node --version` against `^20.19.0 || >=22.12.0` and stops with
`the console build needs Node ^20.19.0 || >=22.12.0, and {found} was found` outside that range, then runs
`npm ci --no-audit --no-fund` in `web/` unless `node_modules` carries the stamp of an install at least as new as the
lockfile, and `npm run build`. `web/.nvmrc` names 20.19, the version the workflows install. When the bundle cannot be
built, the script discards `web/dist` and warns
`the web bundle could not be built, so web/dist was discarded and the router will serve the placeholder page. Set
PHOEBIS_SKIP_UI_BUILD=1 to embed the bundle on disk as it is.`, since a build that quietly embeds the previous console
reports nothing wrong and ships the wrong one. `PHOEBIS_SKIP_UI_BUILD=1` skips the Node step and embeds `web/dist` as it
stands, and a tree without a bundle then embeds the placeholder page, which the admin listener serves in place of the
console.

## The tests

The `Tests` workflow's two jobs are the commands below, each run from the directory the job runs in. The `rust` job,
from the repository root, builds the console first and sets `PHOEBIS_SKIP_UI_BUILD=1` for every step after it, so the
bundle it built is embedded as it stands:

```sh
(cd web && npm ci --no-audit --no-fund && npm run build)
export PHOEBIS_SKIP_UI_BUILD=1
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
PHOEBIS_UI_EXPECT=bundle cargo test --workspace --locked
```

`PHOEBIS_UI_EXPECT=bundle` makes the UI crate's test assert that the build embedded the bundle rather than the
placeholder. Without the variable the test accepts either and checks only that the embedded shell and its assets agree.

The `web` job, from `web/`:

```sh
npm ci --no-audit --no-fund
npx playwright install --with-deps chromium
npm run build
node scripts/check-build.mjs
npm run lint
npm test
npm run smoke
PHOEBIS_SCREENSHOT_DIR=/tmp/phoebis-mock-shots npm run e2e:mock
npm run e2e:reference
```

`npm run build` is `tsc --noEmit && vite build`. `node scripts/check-build.mjs` reads the built files and fails when the
build emitted one chunk, the entry passes 500 kB, or the entry or a chunk it loads carries Recharts code, so the chart
pages stay chunks of their own and the first paint loads none of them, and it holds the API reference page's entry to
the same Recharts rule and to loading neither entry from the other. `npm run lint` is `eslint . --max-warnings 0`.
`npm test` builds the test bundle and runs `node --test test/*.test.mjs`. `npm run smoke` builds the smoke bundle and
runs `smoke/run.mjs` under jsdom with `TZ=UTC`. The last two lines are the browser pass against the mock and the
reference smoke, both in the next section. The workflow writes the screenshots under the runner's temporary directory,
and locally the directory has to lie outside the repository.

## The browser pass

`web/e2e/views.mjs` drives a headless Chromium over every view of the console in the light and the dark theme. It logs
in, then per theme visits the thirteen views `VIEWS` lists (the dashboard, quota, the quota windows section, accounts,
pools, keys, models, requests, analytics, the analytics cache warming tab, sessions, settings and the routing log, two
of them in the mock pass alone), waits for one element per view, asserts the document title, records every console
error and page error, writes a full-page screenshot to `$PHOEBIS_SCREENSHOT_DIR/<theme>/<view>.png`, and runs the layout
assertions its README lists, one function each, exiting non-zero with the list of failures. The README beside the
script, [web/e2e/README.md](https://github.com/daystrom-labs/phoebis/blob/main/web/e2e/README.md), lists every assertion
and names the Playwright image for a host without Chromium.

The screenshot directory must lie outside the repository and outside every checkout `git worktree list` reports: the
script resolves the directory's symlinks, reads the checkouts from `git worktree list --porcelain` (or takes the tree
holding the nearest `Cargo.toml` when `git` cannot answer), and refuses a directory inside any of them with
`PHOEBIS_SCREENSHOT_DIR must lie outside the repository ({inside})`, exit 2. Unset, the directory is
`phoebis-screenshots` under the system's temporary directory. The page runs on Playwright's clock: it starts at
`PHOEBIS_E2E_CLOCK` (an ISO instant) or at the launch instant, and moves only in the fixed steps the script takes while
a view loads and again before its shot, with animations off and the time zone UTC, so two passes over one tree at one
clock origin draw the same pixels.

Against the mock, from `web/`, with Chromium installed once per host:

```sh
npx playwright install chromium
PHOEBIS_SCREENSHOT_DIR=/tmp/phoebis-mock-shots npm run e2e:mock
```

`e2e:mock` builds the console with the fixture-backed mock into `dist-mock/` (`npm run build:mock`), serves that bundle
from a static server on a loopback port, and runs `mock-pass.mjs`, which drives `views.mjs` against it with the mock's
admin secret and the clock pinned at `2026-09-16T14:30:00Z`. The mock's random draws are seeded, so two runs write the
same screenshots byte for byte, which `diff -r` over the two directories shows. Beside the views' shots it writes five
more per theme: the open catalog popup as `<theme>/accounts-catalog.png`, the open cost reference's three tabs as
`<theme>/cost-reference-tiers.png`, `cost-reference-plan-rates.png` and `cost-reference-api-prices.png`, and the
Analytics page's Burn tab as `<theme>/analytics-burn.png`.

Against a running router, from `web/`:

```sh
PHOEBIS_E2E_BASE=http://127.0.0.1:8081 PHOEBIS_E2E_SECRET=... PHOEBIS_SCREENSHOT_DIR=/tmp/phoebis-shots npm run e2e
```

`PHOEBIS_E2E_BASE` is the admin listener's URL (`http://127.0.0.1:8081` when unset) and `PHOEBIS_E2E_SECRET` the admin
secret, without which the script stops with `set PHOEBIS_E2E_SECRET`. The router seeds its registry at real time, so
the clock origin there is the launch instant, and the pass skips the two mock-only views and writes none of the five
extra shots. `cargo test -p phoebis-router --test end_to_end the_ui_passes` runs the same script against a router the
test starts and seeds itself when `PHOEBIS_E2E_BROWSER=1` is set, and prints
`skipped: set PHOEBIS_E2E_BROWSER=1 to run the browser pass` otherwise, so a build host without Chromium still runs the
rest of that suite.

`npm run e2e:reference` is the reference smoke: it serves `dist/` on a loopback port with a stub document, opens
`/api-reference/` in both color schemes, and asserts that the page loads with no console error, lists the stub's
operations, sends a typed token as `Authorization: Bearer` to the server's origin and no other, and keeps the token out
of the browser's storage. The README lists its assertions, and
`cargo test -p phoebis-router --test end_to_end the_reference_page` runs it through the router it starts when
`PHOEBIS_E2E_BROWSER=1` is set.

The site's screenshots are the mock pass's. The committed names are `docs/site/assets/<view>-light.png` and
`<view>-dark.png`, so after a pass that exited 0, copy `light/<view>.png` to `docs/site/assets/<view>-light.png` and
`dark/<view>.png` to `docs/site/assets/<view>-dark.png` for the sixteen pairs the pages embed: `accounts`, `analytics`,
`analytics-burn`, `analytics-cache-warming`, `cost-reference-plan-rates`, `dashboard`, `guardrails`, `keys`,
`pools`, `quota`, `quota-windows`, `requests`, `routing-log`, `sessions` and `settings`. The three other shots the pass
writes take no page.

## The fixtures

The console's fixtures under `web/test/fixtures/` are written by two crate tests and never edited by hand.
`PHOEBIS_WRITE_FIXTURES=1 cargo test -p phoebis-admin --test fixtures` rewrites the admin fixtures. Without the variable
the test compares each committed file's shape with the live answer and fails a missing one with
`{name}.json is missing; run with PHOEBIS_WRITE_FIXTURES=1`, and every captured body is searched for the secrets the
test planted before it is written, since a committed capture that picked a credential up would publish it.
`PHOEBIS_WRITE_FIXTURES=1 cargo test -p phoebis-routing --test walk_steps` rewrites
`web/test/fixtures/routing/walk_steps.json`, the walk's step ids and verdict words, and without the variable fails when
the committed file differs from the engine's table. After a capture, `npm run sync-mock` from `web/` renders the admin
fixtures into `web/src/mock/generated/` as one module, so every mock row starts from a real answer and a test tells a
stale copy from a fresh one byte for byte.

The profile fixtures under `crates/profile/fixtures/` are the pinned client build's wire shape.
`scripts/build-fixtures.py` writes them from a capture, as [Re-pinning](#re-pinning) describes, and nothing else does.

## The pinned client build

The router is measured against one Claude Code release, the profile `claude-code-2.1.269-linux-x64` compiled into the build, and reproduces the client's own billing stamp after its edits. The
binary the profile was measured from:

| Field | Value |
| --- | --- |
| Path | `~/.local/share/claude/versions/2.1.269` |
| SHA-256 | `25e44883f54419569a3d739f38cbbdaebe83b09895da0f343e1b003710a4775b` |

`phoebis` links BoringSSL through the `boring` and `boring-sys` crates, both pinned at 5.2.0. `boring-sys` vendors its
own copy of BoringSSL under `deps/boringssl/` rather than linking a system build, and that vendored tree carries no
revision id of the BoringSSL project: no commit file exists anywhere under it. `include/openssl/base.h` in it declares
`BORINGSSL_API_VERSION 41`, the only revision marker the vendored source carries.

Every fixture under `crates/profile/fixtures/` was captured from a client binary running against loopback receivers
with dummy credentials, never against `api.anthropic.com`. The inference and control fixtures come from a 2026-09-13
capture of the pinned 2.1.269 binary. The six `count_*` fixtures come from a 2026-09-21 capture of the 2.1.278 build,
the build installed when the client's own token count was recorded. The inference plane (the TLS ClientHello and the
`/v1/messages` request head and body) came from `-p` print-mode runs pointed at loopback receivers over
`ANTHROPIC_BASE_URL`, once per credential mode: an API key, an OAuth token, and an OAuth token with the first-party
override. The control plane (the OAuth token exchange and the profile, usage and roles calls) came from an interactive
login and status session run through a tunnel harness, a CONNECT proxy paired with a TLS-terminating fake upstream that
answered with canned responses while recording what it received.

`scripts/capture-baseline.sh` reproduces the inference-plane half of this recipe in one command. It re-executes itself
under `unshare -rn`, so the client runs with loopback as its only route, then drives the three credential modes, the
first-party override a second time as a second hash vector, and the gzip-forced run past `scripts/capture/http_raw.py`
and one more run past `scripts/capture/connect_capture.py` over `HTTPS_PROXY` for the ClientHello. Into the output
directory it writes `clean_modes.json` with one entry per recorded request, one `clean_body_<i>.json` per request
holding that body's raw bytes, `gzip_modes.json` for the gzip-forced run, and `connect.json` for the hello. It finishes
by running `python3 scripts/build-fixtures.py --check <output-dir>` and exits with that status, so capturing and
comparing are the same command. The system prompt carries the current date, so a re-capture on a later day differs
there, and `--check` prints a bounded diff of each changed body to tell that apart from a change in the request shape.
`scripts/capture-count-tokens.sh` reproduces the count half beside it, in the same namespace and with the same dummy
credentials: three runs, one per credential mode, past `scripts/capture/count_receiver.py`, which answers the first
request with a canned tool call so the count the Read tool sends before a large file reaches the wire. It finishes by
running `python3 scripts/build-fixtures.py --count <output-dir> --check`.

The control-plane half needs a live session and cannot run unattended, so the baseline script's header comment covers
it rather than automating it. A capture without that half still compares: `--check` lists the control-plane fixtures
under `not captured (control plane):` and grades everything it could reach.

### The fixture manifest

SHA-256 of every file under `crates/profile/fixtures/`. Regenerate the table with:

```sh
cd crates/profile/fixtures && find . -type f | sort | sed 's#^\./##' | xargs sha256sum
```

| File | SHA-256 |
| --- | --- |
| `body_apikey.json` | `64d28fb040dc9c5fcb713288e2296c34fd4b2d9a65f64a84ef59193bc561afe7` |
| `body_oauth.json` | `5c596c45025d12340b1bc9bb90aa41df2c3af49000369d5a4cd6fa2fb62efeb5` |
| `control_profile.http` | `4c8d36c9f0a634191eff458cc19cce358e0c91bdc9d193df5f576916b1e08118` |
| `control_roles.http` | `6d279e276eff083b3c0564f8e73c2524aa576d920b9cfd5a7666ccd4b6e238e8` |
| `control_token.http` | `6f9142f141b27a065019141847cf212d7fbe4ef3ad8b49fd5391442f572c25dc` |
| `control_usage.http` | `b69acf572b0fdcb0b5f9ebeaa3e990643db8fd6ad827e1586eaf3d1bbaf61645` |
| `count_apikey.http` | `3b319a66fa6b276d5a1b8b463658567644af70ed3c6976076751c8cbd19e804d` |
| `count_oauth.http` | `4eb20f37f181a60bcc7ab10983784c4a8d7d94b3d5407ececc51a53b211f2e97` |
| `count_oauth_fp.http` | `722c04e20acdfbb78ea9da932c586afd7d49684f1cffc14d81259a109a878d0d` |
| `hello.http` | `cb0094b27b6ca22d36b0b84a73ad8d27f8ebdcdbf5db2e6c5327e8a8c3587740` |
| `hello_control.hex` | `d0a7d67eac21af00cfd1134317bcf67e2bb4e870f85a821f588d2cb93495c6a6` |
| `hello_inference.hex` | `b50516e0f1a8549d71c9463311f2385148785e40aa5a7c5fd32c233683302600` |
| `inference_apikey.http` | `787a69c938b4bac5e94e6646d9495906fbd46d0894752f1a38a8c7c24ba4cbac` |
| `inference_oauth.http` | `9f8b477f9af0ec3075a598e199cd2b71728a069ad0f7802ac38abbbf05612e27` |
| `inference_oauth_fp.http` | `34400362af283e3a3737e9ecd828f619b7234248c10859a2869993d79349f4de` |
| `responses/profile.json` | `d7a6b9a21e3bfca907fbfaa1174e9eb702bc238e53a82fb0d00bbb0b6d900523` |
| `responses/roles.json` | `37c38c5aba8ccfc67152aed9b511b1e3e688b314ac9f4c3544c8402e0a7c155e` |
| `responses/token.json` | `d76c5a74acdf3855eea8c5f745de13fe13e5e66c798a0de5596c42a195c3f668` |
| `responses/usage.json` | `5793d218829234389178c83b42c23e7586c8d557dbb6ffbf7a9c542a0eca331b` |

A build that changes this manifest without a matching change here has drifted from what this page backs. Regenerate
the table after any accepted re-pin.

### What remains unmeasured

- Interactive (`cli`) and background (`cli-bg`) entrypoints, `x-cc-atis`, `anthropic-usage-limit: slow`, fallback
  headers, `x-anthropic-additional-protection`, and the `x-stainless-helper` markers.
- Live acceptance by Anthropic of any request the relay emits, and any account-restriction consequence.
- Outbound gzip on the wire (gated by the live rollout flag and the first-party host check).
- Session resumption over longer sessions. None was observed within the capture windows.
- The quota-wall usage variant of token counting on the wire. The Read tool's pre-turn count is measured on the 2.1.278
  build (`count_*` under the fixtures).
- TLS record-level behavior after the ClientHello (key update, ticket handling) and the ServerHello path.

## Re-pinning

A profile records the measured build's wire constants: its version and build tag, its two CLI user agents and its
Axios user agent, its Stainless headers, its three beta lists (one per credential mode), the control-plane URLs and
client id, and the TLS curves and cipher list, each a field of the `Profile` value in `crates/profile/src/lib.rs`, with
the fixtures under `crates/profile/fixtures/` as its record. The router's own calls send part of it: the control-plane
calls go to the profile's URLs, the token, profile and roles calls with its Axios user agent and the usage call with
its SDK user agent, the token exchange and the refresh carry its client id in their bodies as the authorize URL does
in its query, and the connections toward the upstream offer its curves, with its cipher list on the control plane.
The Stainless headers and the beta lists are what the fixture suites compare against, and the relay emits none of
them: headers travel as the client sends them, and the beta edits and the restored fields are the relay's fixed rules
([The betas](relay.md#the-betas) and [The restoration](relay.md#the-restoration)), which do not follow a newer build.
A request the profile cannot reproduce is refused until the router runs a build re-pinned to the client. To re-pin the
profile against a new build:

1. With the build installed under `~/.local/share/claude/versions/<version>`, run
   `scripts/capture-baseline.sh <path-to-claude-binary> <output-dir>`. It refuses to start with a real
   `ANTHROPIC_API_KEY` or a real `CLAUDE_CODE_OAUTH_TOKEN` in the environment
   (`capture-baseline.sh: refusing to run with a real ANTHROPIC_API_KEY set`) and where `unshare -rn` does not work,
   and otherwise re-executes itself in the loopback-only namespace, drives the three credential modes, the first-party
   override a second time, and a gzip-forced run past the recording receiver (the forced run records a plain body,
   since the 2.1.286 client gzips toward `api.anthropic.com` alone), records the ClientHello through the CONNECT
   receiver, and ends with `python3 scripts/build-fixtures.py --check <output-dir>`, exiting with that command's status.
   The system prompt the client sends embeds the current date, so a re-capture taken on another day differs there, and
   `--check` prints a bounded diff per body so that shows as a date difference rather than a shape change.
2. Read the diff. A header added or renamed, a beta list that changed, or a new body field is the signal that the
   client has moved past the pinned profile.
3. The control plane (the token exchange and the profile, usage and roles calls) is not captured by the script: it
   needs a manual session behind a CONNECT tunnel, which the script's header comment describes. `--check` lists those
   fixtures under `not captured (control plane):` rather than failing on them.
4. Land the re-pin as a change to the repository. `python3 scripts/build-fixtures.py <output-dir>` writes the new
   fixtures. A new `Profile` value in `crates/profile/src/lib.rs` carries the new constants, and `by_name` learns its
   name. The fixture manifest above is regenerated with the command it shows. The tests in the profile, relay and
   router crates grade the relay against the new fixtures. Until that change lands, the router is measured against the one
   profile this build carries.
5. `scripts/capture-through-router.sh` captures what the router hands its own upstream from a real paired account and
   grades it with `--through-router`. With the service stopped, take a fresh native capture on the same host and the
   same day as in step 1, then run `PHOEBIS_CAPTURE_OAUTH_KEY=<router key> PHOEBIS_CAPTURE_API_KEY=<router key>
   PHOEBIS_ADMIN_SECRET=<secret> PHOEBIS_CAPTURE_BASELINE=<baseline-dir> scripts/capture-through-router.sh
   <path-to-claude-binary> <output-dir> <config.yaml>` as the store's owner (`sudo -u phoebis env ...`). The first key
   names a pool holding exactly one paired OAuth account and the second a pool holding one API-key account toward
   `api.anthropic.com`, both router keys minted through the admin API ahead of time and never Anthropic credentials.
   The service must be stopped while it runs, since the capture instance opens the same store and would otherwise race
   the running service's refresh attempts and keepalive leases on the same accounts, and it shares its client state
   root with the baseline capture, so the two scripts never run at the same time. The script ends with
   `python3 scripts/build-fixtures.py --check --through-router --baseline <baseline-dir> <output-dir>`, which must
   print `fixtures match`. The `Content-Length` and `account_uuid` masks are the only differences the check allows,
   and it treats the billing fields and the `x-client-request-id` the relay restores on a reduced-shape request as
   absent rather than masking them, since a field moving from absent to present is the restoration at work. The
   fresh capture is the baseline because a client run stamps today's date into its own system reminder, which no
   mask touches. The check re-signs every captured body through `cargo run -p phoebis-cch --example resign`
   unless `--skip-hash-check` is added by hand, so cargo must be on the host. Pair the account right before a run: the
   receiver's throwaway certificate carries `api.anthropic.com` alone, so a token refresh the router attempts mid-run
   fails at the TLS handshake and the capture fails with it.
6. `scripts/capture-count-tokens.sh <path-to-claude-binary> <output-dir>` records the client's own token count in each
   credential mode, inside the same namespace and past a receiver that answers the first request with a canned tool
   call, so the count the Read tool sends before a large file reaches the wire. `python3 scripts/build-fixtures.py
   --count <output-dir>` writes the `count_*` fixtures, and `--check` beside it grades a new build against them.

## The documentation

Zensical builds this site from `docs/site/` under `zensical.toml`. From the repository root:

```sh
pipx run zensical==0.0.62 build --clean --strict
python3 scripts/check-docs.py links
python3 scripts/check-docs.py client
python3 scripts/check-docs.py check-config
python3 scripts/check-docs.py codes
python3 scripts/check-docs.py example
python3 scripts/check-docs.py prose docs/site/<page>.md
```

The build writes `site/`, and a warning fails it. The workflow pins the same version, so a local build passes or fails
as the workflow's does. `pipx run zensical==0.0.62 serve` previews the site. The checks under `scripts/check-docs.py`
read files and print one line per problem, exit 1 on any and 2 on a usage error, and fail on a source shape they cannot
read rather than dropping it:

- `links` walks the built site for an internal link or image that resolves to nothing, and for a link whose fragment
  names no heading or anchor on the page it lands on.
- `client` compares the proxy listener's route table with the table on
  [The client](client.md#what-the-proxy-accepts), so a route added or removed in the router fails the build until the
  page follows.
- `check-config` compares the lines the binary's `check-config` arm prints with the list under
  [Commands](installation.md#commands) on Installation.
- `codes` compares the refusal codes the `x-phoebis-error-code` header carries on the proxy listener with the table
  under [Refusal codes](client.md#refusal-codes) on The client.
- `example` compares the installation page's quoted example with `deploy/config.example.yaml`.
- `prose` holds the site's prose to its mechanical rules, by path or, with no path, over every page and the README: a
  semicolon on a prose line, a line over 120 columns that is not a table row, an em-dash or an en-dash, a word from
  its list of words no page carries, and a bare `api_key` outside a code span. It drops fenced blocks and code spans
  before it reads.

`bash scripts/tests/check-docs.sh` runs the checker's own cases against throwaway trees, and the favicon case at the
end against the real one, so the checker fails on what it exists to catch.

## Continuous checks

Two GitHub Actions workflows run on every pull request and every push to `main`. `Tests` runs the `rust` and `web` jobs
of [The tests](#the-tests) and a third job, also named `Tests`, that reports the two as the one check the branch
protection on `main` requires, a canceled or skipped job included. A newer push to a pull request cancels its older
run. `Documentation` builds and checks this site ([Publishing the site](#publishing-the-site)). The third workflow,
`Release`, runs on a release, a manual dispatch, and a pull request that touches the image's or the binary's inputs
([Releasing](#releasing)). The three are separate workflows, so a failed test never blocks the site's build.

The release workflow's jobs run with a read-only token except `build`, which writes packages, `publish`, which writes
packages and the release's archives, and `smoke`, which reads packages. The `Tests` jobs read alone, and in
`Documentation` only the deploy job writes, to Pages.

## Publishing the site

The `Documentation` workflow builds this site on every push to `main` and every pull request:
`scripts/tests/check-docs.sh`, `zensical build --clean --strict`, the checks under `scripts/check-docs.py`, and the
built `site/` directory uploaded as an artifact. The deploy job runs only on a push, and only when the repository
variable `DOCS_PAGES` is `1`, so a push to `main` stays green until Pages is enabled.

To publish: in the repository's Pages settings, set the source to GitHub Actions. A private repository needs a GitHub
plan that offers Pages there. Otherwise the repository has to be made public first. Under Settings, Secrets and
variables, Actions, Variables, add `DOCS_PAGES` with the value `1`. The next push to `main` deploys the site to
`https://daystrom-labs.github.io/phoebis/`.

To pause deployments, delete the `DOCS_PAGES` variable. The build job keeps running on every push, the deploy job
stops, and GitHub Pages keeps serving the last deployment at the same URL.

To unpublish the site instead, use the Pages settings' unpublish operation: choose "Unpublish site" and confirm, then
check that the published URL answers `404` before treating the site as taken down.

## Releasing

A GitHub release publishes the container image and a static binary per Linux architecture. The `Release` workflow runs
on a published release, on a manual dispatch, and on a pull request that touches the image's or the binary's inputs
(`.dockerignore`, the workflow, `Cargo.lock`, `Cargo.toml`, `Cross.toml`, the Dockerfile, the crates, the deploy files,
`rust-toolchain.toml`, and `web/` outside its `e2e`, `smoke` and `test` directories). To release:

1. Set `[workspace.package].version` in `Cargo.toml` to the version (`X.Y.Z`, or `X.Y.Z-rc.1` for a prerelease), head
   the changelog's section for it `## X.Y.Z`, and merge to `main`.
2. Create the release on GitHub with the tag `X.Y.Z` at that head, no `v` prefix. Publishing it starts the workflow.
3. The workflow's guard refuses the release before anything builds when the tag has another shape
   (`release tag '{TAG}' is not X.Y.Z or X.Y.Z-suffix`), differs from the workspace version
   (`release tag '{TAG}' differs from the workspace version '{version}' in Cargo.toml`), or has no section in the
   changelog (`docs/site/changelog.md has no '## {TAG}' section for this release`). Otherwise it builds `linux/amd64`
   and `linux/arm64` on native runners and pushes each by digest, and runs the amd64 image through `--version` (which
   must print `phoebis {VERSION}`), `check-config` against `deploy/config.container.yaml`, a container in the compose
   example's shape that must reach `healthy` within 30 seconds, serve the built console (the page from the admin
   listener is not the placeholder and one of its assets is served) and the API reference page with a document of
   OpenAPI 3.1.0 holding the admin listener's paths, answer `phoebis status` inside the container with a first line
   ending in `, ready`, take a `phoebis backup` onto the volume and stop within the 40-second grace period, and a second
   container under a certificate pair the smoke generates, whose certificate verifies the console over TLS. Beside the
   images it builds the console and checks its bundle with `scripts/check-build.mjs`, failing before the static binaries
   build. Then it builds a static binary for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` inside the
   `cross` tool's images (`cross` 0.2.5, the console embedded as built), runs each on its own platform (`--version`, a
   static link check, an embedded bundle, `init --non-interactive` and `check-config` in a temporary directory) and
   packs it with the license as `phoebis-{VERSION}-{TARGET}.tar.gz`. The `cross` images install clang, cmake and
   libclang without tzdata's time zone prompt, so the build runs through rather than waiting for an answer. Once every
   smoke passes it publishes one manifest list at `ghcr.io/daystrom-labs/phoebis` under `X.Y.Z`, `X.Y` and `X` (from
   major 1, never for a prerelease), `latest` (never for a prerelease), and `sha-<short sha>`, checks that the list
   holds amd64 and arm64, and uploads the two archives with `SHA256SUMS` to the release.

A failed smoke, of the image or of a binary, or a failed bundle check leaves untagged digests in the registry, no tag
and no archive on the release. A failed guard leaves nothing. A manual dispatch (Actions, Release, Run workflow) is the
dry run: it builds both platforms and runs the smoke checks without pushing. Run it after a change to the Dockerfile,
`Cross.toml` or the workflow, since a pull request builds the amd64 side alone, and before the first release. The
package inherits the repository's visibility: while the repository is private, pulling needs a token with
`read:packages`. The image name is derived from the repository that runs the workflow, so a renamed repository publishes
under its new name without a workflow change.
