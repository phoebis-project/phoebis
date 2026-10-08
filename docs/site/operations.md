# Operations

The router keeps its state in two SQLite files and one key file. This page covers backing them up with `phoebis
backup` and restoring them, what the storage key protects, reading a running router with `phoebis status`, upgrading
the binary or the container, re-pinning the Claude Code build the router is measured against, publishing this site, and
releasing the container image.

## Backup

The router's state lives in three files: `storage.path`, the registry of accounts, sealed credentials, pools, keys,
pricing versions, pins, cooldowns, and the relay's session memory; `stats.ledger_path`, the ledger of requests,
attempts, events, samples, rollups, and captured bodies; and `storage.encryption_key_file`, the key that seals every
credential the registry holds. `phoebis backup` takes the snapshot of both databases and names the key file; it
never copies the key.

Both databases are SQLite, and both run in WAL mode. A plain file copy taken while the service is running can land
mid-transaction and split either database file from its `-wal` file, losing the last transactions in between.
SQLite's own `.backup` avoids that by copying a consistent snapshot instead.

A process crash loses nothing that committed, since the WAL is written before a commit returns. The ledger runs with
`synchronous = NORMAL`, so a power loss or an operating-system crash may lose its last transactions. The registry runs
with `synchronous = FULL`, so every change to its own rows (accounts, credentials, keys, members, pools, refresh
attempts, rules and secrets) is on disk before it is acknowledged, while the routing state (affinity, cooldowns, pins,
usage windows, the marks a failing account earns, and an operator's cooldown clear or session release) commits at
`NORMAL` in one transaction per writer wake, so a power loss can lose the last of it: the router rebuilds it from the
next requests, and an operator redoes a clear or a release it undid.

```sh
phoebis backup --to /path/to/backups/$(date +%F)
```

Under the container the command runs in the running container and writes onto the data volume; `docker compose cp`
copies the directory out of the volume onto the host, or a bind mount over `/var/lib/phoebis/backups` puts it there:

```sh
docker compose exec phoebis phoebis backup --to /var/lib/phoebis/backups/$(date +%F)
docker compose cp phoebis:/var/lib/phoebis/backups/<name> .
```

`phoebis backup` copies each database through SQLite's online backup API, one consistent snapshot per database, while
the router serves or while it is stopped; it takes no lock, opens no socket and needs no admin secret. It runs as the
user that owns the databases (`phoebis` on the host, uid 65532 in the container, which `exec` already is), since the
files and their `-shm` files are `0600`. The directory it writes holds `router.sqlite`, `ledger.sqlite` and
`manifest.json`, each at `0600`, and nothing else. `backup` creates the directory and every missing parent at `0700`,
or takes an existing empty directory and warns when its mode is wider, since the ledger copy holds the prompts and
tool results its captured bodies contain, in plain text.

`manifest.json` is written last, once both copies have answered `PRAGMA quick_check` with `ok` and every file and the
directory are synced, so a directory that holds a manifest holds two complete copies. It carries `format` (1),
`phoebis_version`, `taken_at` (UTC), `config` (the file read), `key_file` (the path `storage.encryption_key_file`
names, and nothing derived from the key), and under `registry` and `ledger` each copy's `source`, `schema_version`,
`bytes` and `sha256`.

`backup` exits 1 with nothing written when the file does not load, when `--to` names a file or a directory that is not
empty, or when either database does not exist (`a backup carries both databases; {path} does not exist`). A copy that
fails, such as one whose database another connection holds locked for ten seconds, removes every file and directory
the run created and exits 1 with `backup failed: {reason}; nothing was written to {dir}`. It exits 0 once the manifest
is written ([commands](installation.md#commands)). Where the binary is not at hand, `sqlite3`'s `.backup` produces the
same snapshot, one database at a time.

A backup taken before an upgrade needs both snapshots to succeed: the registry and the ledger migrate together,
and a restore built from only one of them leaves the two databases at different points in the request history.

The key seals every credential in the registry, so a backup that carries the database is useless without the key, and
dangerous with it. The registry also holds the keepalive engine's retained recipes, each prompt sealed under the same
key, so a restored registry needs its key for them as it does for the credentials. Keep the key somewhere separate, such
as a password manager or a secrets store, and never beside the database copy. The ledger holds captured bodies, which
are prompts, so a ledger backup needs the same care as the registry.

## Restore

A restore puts both databases back with the key that sealed the registry, in this order:

1. Stop the router: `sudo systemctl stop phoebis`, or `docker compose stop`.
2. Move the current `router.sqlite` and `ledger.sqlite` aside, each with its `-wal` and `-shm` files.
3. Copy each file of the backup to the path the configuration names, at mode `0600` and owned by the service user:
   `sudo install -m 0600 -o phoebis -g phoebis <backup>/router.sqlite /var/lib/phoebis/router.sqlite`, and the same
   for `ledger.sqlite`.
4. Put the key file that sealed the registry at `storage.encryption_key_file`; the manifest's `key_file` says where it
   lived. A wrong key is refused at start and by `check-config` with `the storage key does not match the registry at
   {path}`.
5. On a new host, put the certificate pair where the file names it: the backup carries neither the pair nor the key. A
   pair `init` generated lives under `tls/` beside the configuration (`/etc/phoebis/tls/` under the unit, `tls/` beside
   `compose.yaml` in the container shape) with the `ca.pem` the clients trust; copy that directory with the
   configuration, keeping its links, since a new CA would need every client to trust it again.
6. Run `phoebis check-config`.
7. Start the router: `sudo systemctl start phoebis`, or `docker compose up -d`.
8. Confirm with `phoebis status`, which reads `ready`.

Under the container the two copies go onto the `phoebis-data` volume, owned by uid 65532 at `0600`, from the host or
from a one-off container that mounts the volume, as the [storage key](#the-storage-key) recovery runs; steps 6 and 8
are `docker compose run --rm phoebis check-config` and `docker compose exec phoebis phoebis status` there.

The binary that restores is the manifest's `phoebis_version` or newer: an older binary refuses a newer schema with
`restore the backup taken before the upgrade`, and a newer one migrates the copies as it opens them.

A registry restored alone brings back the accounts, their sealed credentials, the pools and the keys, and the ledger
starts empty, so the statistics, the request log, the usage samples, the rollups, the keepalive ledger and the parked
records are gone, while the pins and cooldowns rebuild from the next requests. A ledger restored alone brings back the
statistics, and the registry starts empty, so every account is paired again and every key is minted anew with a new
secret, and the ledger's rows name account and key ids that nothing resolves any more.

## The storage key

A lost key loses every credential the registry holds and every downstream key it authenticates: without it, no
sealed credential can be opened again, and no downstream key's digest can be reproduced to check a client's secret.

The key is 32 raw bytes or 64 hex characters, held in the file `storage.encryption_key_file` names. A fresh installation
creates it with `phoebis init`, which writes the 64 hex characters into a file created at mode `0600` in one open; where
the binary is not at hand, `head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'` written into a file created empty at
`0600` is the same key. Writing through a redirect under the usual umask would leave the key world-readable for the
moment between the file's creation and a later `chmod`, and an interrupted run would leave it that way for good. The key
file may end in one newline after the 64 hex characters, so a key written as a line is accepted too, such as
`printf '%s\n' "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"` written into that same empty file.

The registry carries a canary sealed under the key at its first open, and every open proves the key against it before
any credential is read: a restore onto a host whose key file differs, or a key regenerated after the file was moved,
is refused at start and by `check-config` with `the storage key does not match the registry at {path}`. A registry
from before the canary existed is proved against its sealed rows instead, up to the first 32 of each kind (credentials
and Anthropic-key secrets), passing when any of them opens, by `check-config` and at start, and is sealed at that
start.

`init` refuses a data directory that already holds a registry, so a lost key is never papered over by a new one.

The key seals OAuth credential bundles and Anthropic-key secrets. `Store::digest_key` also derives, from this same key,
the digest every downstream key authenticates against: the secrets the admin API mints and hands to clients and the
companion.

There is no re-key command. Under a lost or replaced key no sealed row opens again and the canary refuses the start,
so recovering means discarding the registry's secrets while keeping everything else. Stop the service
(`sudo systemctl stop phoebis`), take a [backup](#backup) (`phoebis backup --to <dir>`), and run this sequence against
the registry as the `phoebis` user:

```sh
sudo -u phoebis sqlite3 -bail /var/lib/phoebis/router.sqlite <<'SQL'
BEGIN IMMEDIATE;
DELETE FROM meta WHERE key = 'storage_key_canary';
DELETE FROM credentials;
DELETE FROM api_key_secrets;
UPDATE accounts SET state = CASE kind WHEN 'oauth' THEN 'disconnected' ELSE 'unpaired' END,
  lifecycle_generation = lifecycle_generation + 1, revision = revision + 1;
UPDATE refresh_attempts SET resolved_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000, outcome = 'failed',
  detail = 'disconnected' WHERE resolved_at IS NULL OR outcome = 'uncertain';
COMMIT;
SQL
```

Under the container the image ships no `sqlite3`. The backup is `docker compose exec phoebis phoebis backup --to
/var/lib/phoebis/backups/<name>` before `docker compose stop`, since `exec` needs the running container, or `docker
compose run --rm phoebis backup --to /var/lib/phoebis/backups/<name>` once it is stopped; then the same sequence runs
as uid 65532, either from the host against `router.sqlite` on the `phoebis-data` volume or from a one-off container that
carries `sqlite3` and mounts that volume.

The sequence discards every secret the registry holds, and nothing else: the Anthropic-key secrets, the canary and the
OAuth credential bundles go, and each account rests where the Accounts view's Disconnect leaves it, with its open
refresh attempts settled. The accounts, downstream keys, pins, pools and session memory stay. `-bail` stops at the
first statement that fails, and the transaction it leaves open rolls back, so the file is either wholly changed or
untouched. The next start under the new key finds nothing sealed and seals a new canary; then:

- Re-enter every `api_key` secret.
- Re-pair every `oauth` account.
- Rotate every downstream key. Replacing the storage key changes what `digest_key` derives, so every digest already
  stored against the old key stops matching a client's presented secret, and every existing downstream key starts
  answering `401`. Rotate each one (`POST /admin/keys/{id}/rotate`) or recreate it, then distribute the new secrets
  to the clients and the companion.

An operator who would rather start over moves the registry file aside instead, with its `-wal` and `-shm` files if
present, and lets the next start create an empty registry, in which every account, pool and key is created again.

## Is it up?

`phoebis status` is the operator's read and `phoebis health` the orchestrator's: `health` asks the proxy listener one
question and prints one line, `status` runs `health` first and then reads the admin API with the admin secret from the
environment, and prints one screen:

```text title="phoebis status"
phoebis 0.1.0, up 3d 4h, ready
  proxy https://0.0.0.0:8317  admin http://127.0.0.1:8081
  metrics http://127.0.0.1:9464
  ledger: 2 parked
  attention: pool_exhausted work since 12:03;
    refresh_failed alice@example.com since 11:40

pools
  home  disabled  1 member, 0 keys, 0 pins, 0 requests/24h
    erin@example.com  ready  unavailable  5h -  7d -
  work  active    3 members, 2 keys, 2 pins, 41 requests/24h
    alice@example.com  ready   eligible     5h 42%   7d 18%  resets 14:00
    bob@example.com    ready   cooldown     5h 100%  7d 61%  until 15:30 rate_l…
    carol@example.com  paused  unavailable  5h -     7d -

accounts in no pool
  dave@example.com  unpaired

pins (one per session, the main agent's)
  session       agent  account            state
  7b21aa90f3d1  a1     alice@example.com  shedding from bob@example.com, 12s ago
  3f9a0c12e4b7  -      alice@example.com  pinned 4m ago, claude-opus-4-1
```

The header carries the version and the uptime from `GET /admin/system` and one of three readings: `ready`; `not ready:`
with each dead component (`ledger`, `router`, `store`); or `proxy down:` with the probe's line when the proxy listener
did not answer `200`, which wins over the other two. When the admin API could not be read, the header carries the
binary's own version and `readiness unknown`, unless the proxy is down. The listeners follow with their schemes as the
file names them. Under them, a listener's certificate whose leaf expires within 30 days prints
`proxy.tls.cert_file {path} expires in {n} days ({date})`, and a certificate the next start would refuse prints the
start's line for it ([TLS on the listeners](installation.md#tls-on-the-listeners)); the running router keeps the pair it
read at start. `ledger:` prints only when a figure needs a look, and names each one: records `queued` in the writer
channel, `behind` while admission refuses, `degraded` after a failed batch, priced records `parked` in the ledger file,
a batch `retrying`, and after `writer:` the state writer's `deferred` writes and its `degraded` flag.
`attention:` lists every attention item as `<kind> <subject> since <HH:MM>`, or reads
`none`.

Under `pools` each pool prints by id with its status, its member, key and pin counts and its requests in the last 24
hours, then one line per member by priority, then account id: the account's label, its state (`paused` when routing is
off on it), the member's eligibility, the five-hour and seven-day windows in whole percent (`-` with no sample) and the
Fable week when the account has one, then the nearest reset as `resets <HH:MM>` or, under a cooldown, `until <HH:MM>
<reason>`, with the scope ahead of the reason (`fast`, `fast_credits`, `model:<canonical>`) when the cooldown holds less
than the whole account. The block reads `pools: none` when no pool exists. `accounts in no pool` lists the accounts no
pool holds, with their state, and is left out when there are none.

The pins block is the sessions route's answer, one line per session that holds a pin: the route groups pins by key
generation and session and prefers the main agent's pin, so a session whose subagent is pinned to another account shows
its main agent's line. Each line carries the first twelve characters of the session id, the agent id or `-`, the
account, and `pinned <age> ago, <model>`, or `shedding from <account>, <age> ago` while the session moves off an
account, followed by `, keepalive <state>` and `next probe <HH:MM>` while the keepalive engine tracks it. Times print in
the local zone as `HH:MM`, with the date in front as `MM-DD` when it is not today; ages print as `12s`, `4m`, `3h` or
`2d`. The block reads `pins: none` when no session holds a pin.

A read that fails prints one line in its section, such as `pools: the admin listener did not answer on <url>: <reason>`
or `sessions: the admin secret was refused`, and the rest of the screen still prints. When the router is down, the
screen is the header with the binary's version and `proxy down: proxy does not answer on <addr>: <error>`, the
listeners, and one such line per read. A label column is as wide as its widest label where the line has the room; a
label cut there ends in `…`, and `--json` carries every label and id whole. A table line past 80 columns is cut and
ends in `…`, and a line of text past 80 columns wraps. The pins and the accounts in no pool print 20 lines each and
then `and N more`; `--all` prints every one. `--json` prints the same reads as one object for a script, each answer
verbatim ([commands](installation.md#commands)).

With `--url` and `--secret-env` standing in for the file, as for a router on another machine, `status` has no proxy
listener to probe: it prints `proxy: not probed, no configuration file names the listener` under the listeners and
reads readiness from `/admin/system` alone.

`status` exits 0 when the router answers `ready` and the probe, where it ran, answered; 1 otherwise; 2 for a usage
error. `ready` is the liveness of the router's components, the answer `/api/ready` gives, and not a promise that a
request finds an account: a router whose every pool member is cooling down reads `ready` and exits 0. `pool_exhausted`
joins the attention line once a request finds the pool dry, so a pool whose members all cooled with no request since
shows none; a script that asks whether requests can be served reads each member's eligibility under `pools` too. Under
the container, `docker compose exec phoebis phoebis status` runs it in the running container, which inherits the
container's environment and so the admin secret.

## Upgrades

1. Build the new binary: `cargo build --release`.
2. Validate the live configuration file with the binary just built, not the one still installed, since step 5 is
   what replaces that one: `sudo target/release/phoebis check-config --config /etc/phoebis/config.yaml`. The secret is
   optional here (an unset one prints a warning), and `check-config` takes no lock and opens no socket, so it is safe
   to run while the old binary still serves traffic; it reads the key file and proves it against the registry's
   canary, so a key file that drifted is caught before the restart.
3. Read the [changelog](changelog.md) for a bootstrap key that changed.
4. Take a [backup](#backup): `phoebis backup --to <dir>`.
5. Install the new binary: `sudo install -m 0755 target/release/phoebis /usr/local/bin/phoebis`.
6. Restart the service: `sudo systemctl restart phoebis`. The service drains in-flight requests for up to 30 seconds;
   the unit's `TimeoutStopSec=40` covers that drain.
7. Confirm the upgrade with `phoebis --version`, then `phoebis status` for `ready`, or `GET /admin/system` for
   `version`, `storage_ok`, `ready`, `ledger.write_failures` reading `0` and `router.dropped_reports` all reading `0`.
   `router.dropped_reports` counts the reports a closed queue refused at shutdown, zero on a running router; a loop
   that stopped keeping up shows as `router.queued` at `command_depth` and as keys holding their requests past the
   answer.

Under the container, the same sequence is: read the changelog, take the backup (`docker compose exec phoebis phoebis
backup --to /var/lib/phoebis/backups/<name>`), set `image:` in `compose.yaml` to the new version, validate the mounted
configuration with the new image (`docker compose run --rm phoebis check-config`, which reads the service's environment
file and mounts), then `docker compose up -d`. Compose replaces the container, stopping the old one with the 40-second
grace period that covers the drain, and the new binary migrates both databases on the volume as it opens them. `docker
compose ps` reads `healthy` once `phoebis health` has read the router as ready, and `GET /admin/system` confirms the
version.

A certificate pair is read at start and never watched: a renewal is stop, replace, start, and the 30-second drain,
inside the 40-second stop grace, bounds the gap. For a pair `init` generated, the replace step is
`phoebis init --tls self-signed --config <path>`, which writes new leaves under the same CA
([installation](installation.md#commands)); in the container shape it runs from the compose directory under `sudo`,
since the leaf keys belong to uid 65532. Each generated pair sits in its own generation directory under `tls/` behind
the link `tls/<listener>` the configuration names it through, so the renewal switches certificate and key together in
one step and a renewal that stops part way leaves the previous pair live. The renewal holds an exclusive lock on
`tls/renewal.lock` while it runs, and a second renewal started meanwhile refuses rather than writing beside it.

Schema migrations run when the new binary opens each database, in order above the stored version, each inside its
own transaction. The registry's migrations may drop or rebuild a table, and a ledger migration can rebuild a table
too. Both the registry and the ledger refuse to open when their schema version is newer than the binary knows, naming
the file and both versions. `phoebis check-config` reads both versions read-only and refuses the same way before
anything starts. Returning to an older binary after an upgrade therefore means restoring from the backup, not
downgrading the binary in place.

Every configuration block denies unknown keys, so a key the new build does not recognize refuses startup; running
`check-config` first catches that before the restart does.

## Re-pinning the client build

A drift refusal lasts until the router is re-pinned: every request whose hash this build cannot reproduce, or whose
`cc_version` suffix does not reproduce, is refused with `client_drift` meanwhile, the `client_drift` attention condition
names the first refusal, and the re-pin is the procedure below, a source change and a release.

A "pinned" build is the one Claude Code release the router is measured against: `claude-code-2.1.269-linux-x64`, the one profile
this build carries, named by no key. A profile is a fixed set of constants, the version and build tag, the two user
agents, the Stainless headers, the three beta lists, the control-plane URLs and client id, and the TLS curves and cipher
list, checked against the fixtures under `crates/profile/fixtures/`.
A newer client build is not relayed in its own shape automatically: headers travel as the
client sends them, with the `anthropic-beta` line copied as sent and edited for three betas only: the OAuth beta,
claimed for a subscription credential and stripped for an Anthropic key on every request that carries the line; the
cache-diagnosis beta under the pool's `cache_diagnostics` switch; and the extended-TTL beta the cache TTL rewrite adds
or drops. The profile's beta lists record what the measured client sends, as the fixtures show, and the fields the
router restores for a Claude Code request are the ones on the [client page](client.md#the-environment-block), not what a
newer client might add.

To re-pin the profile against a new build:

1. With the build installed under `~/.local/share/claude/versions/<version>`, run `scripts/capture-baseline.sh
   <path-to-claude-binary> <output-dir>`. It re-executes itself under `unshare -rn` so the client has loopback as
   its only route, drives the four credential modes and a gzip-forced run past a recording receiver (the forced run
   records a plain body, since the 2.1.286 client gzips toward `api.anthropic.com` alone), records the ClientHello
   through a CONNECT receiver, and finishes by running `python3 scripts/build-fixtures.py --check <output-dir>`, exiting
   with that command's status. It refuses to start with a real `ANTHROPIC_API_KEY` or a real `CLAUDE_CODE_OAUTH_TOKEN`
   set in the environment. The system prompt the client sends embeds the current date, so a re-capture taken on another
   day differs there; `--check` prints a bounded diff per body so that shows as a date difference rather than a shape
   change.
2. Read the diff. A header added or renamed, a beta list that changed, or a new body field is the signal that the
   pinned profile no longer matches the client.
3. The control plane (the token exchange and the profile, usage, and roles calls) is not captured by the script:
   it needs a manual session behind a CONNECT tunnel, which the script's header comment describes. `--check` lists
   those fixtures under `not captured (control plane):` rather than failing on them.
4. Land the re-pin as a change to the repository: `python3 scripts/build-fixtures.py <output-dir>` writes the new
   fixtures; a new `Profile` value in `crates/profile/src/lib.rs` carries the new constants, and `by_name` learns
   its name; the tests in
   the profile, relay, and router crates grade the relay against the new fixtures. Until that change lands, the router
   is measured against the one profile this build carries.
5. `scripts/capture-through-router.sh` captures what the router hands its own upstream from a real paired account
   and grades it with `--through-router`. Its header names the two router keys it needs, that the service must be
   stopped while it runs since it opens the same store the running service does, and that it shares its client
   state with the baseline capture, so the two scripts never run at the same time.
6. `scripts/capture-count-tokens.sh <path-to-claude-binary> <output-dir>` records the client's own token count in
   each credential mode, inside the same namespace and past a receiver that answers the first request with a
   canned tool call, so the count the Read tool sends before a large file reaches the wire; `python3
   scripts/build-fixtures.py --count <output-dir>` writes the `count_*` fixtures, and `--check` beside it grades a
   new build against them.

## Continuous checks

A GitHub Actions workflow named `Tests` runs on every push to `main` and every pull request. Its `rust` job builds the
console and embeds it, then runs `cargo fmt --check`, `cargo clippy` with warnings denied and `cargo test` over the
workspace; its `web` job runs the console's unit tests, its jsdom smoke checks and the mock browser pass under Chromium;
a third job, `Tests`, reports the two as the one check the branch protection on `main` requires. The documentation
workflow below and the release workflow are separate, so a failed test never blocks the site's build.

The release workflow's jobs run with a read-only token except the build and publish jobs, which write packages, and the
smoke job, which reads them.

## Publishing this site

A GitHub Actions workflow builds this site on every push to `main` and every pull request: `zensical build --clean
--strict`, the link and route checks under `scripts/check-docs.py`, and the built `site/` directory uploaded as an
artifact. The deploy step runs only when the repository variable `DOCS_PAGES` is `1`.

To publish: in the repository's Pages settings, set the source to GitHub Actions. A private repository needs a
GitHub plan that offers Pages there; otherwise the repository has to be made public first. Under Settings, Secrets
and variables, Actions, Variables, add `DOCS_PAGES` with the value `1`. The next push to `main` deploys the site to
`https://daystrom-labs.github.io/phoebis/`.

To pause deployments, delete the `DOCS_PAGES` variable. The build step keeps running on every push, but the deploy
step no longer does, and GitHub Pages keeps serving the last deployment at the same URL.

To unpublish the site instead, use the Pages settings' unpublish operation: choose "Unpublish site" and confirm,
then check that the published URL answers `404` before treating the site as taken down.

To build the site locally, run `pipx run zensical==0.0.62 build --clean --strict` at the repository root; it writes
`site/`. The workflow pins the same version, so a local build passes or fails as the workflow's does.
`pipx run zensical==0.0.62 serve` previews the site.

## Releasing

A GitHub release publishes the container image and a static binary per Linux architecture. The workflow runs on a
published release, on a manual dispatch, and on a pull request that touches the image's or the binary's inputs (the
Dockerfile and `Cross.toml`, the workflow, the manifests, the crates, the deploy files, or the console's sources). To
release:

1. Set `[workspace.package].version` in `Cargo.toml` to the version (`X.Y.Z`, or `X.Y.Z-rc.1` for a prerelease),
   head the changelog's section for it `## X.Y.Z`, and merge to `main`.
2. Create the release on GitHub with the tag `X.Y.Z` on that commit, no `v` prefix. Publishing it starts the
   workflow.
3. The workflow refuses the release before anything builds when the tag has another shape, differs from the workspace
   version, or has no `## X.Y.Z` section in the changelog, naming both values. Otherwise it builds `linux/amd64` and
   `linux/arm64` on native runners and pushes each by digest, and runs the amd64 image through `--version` (which must
   print the version), `check-config` against `deploy/config.container.yaml`, a container in the compose example's shape
   that must reach `healthy` within 30 seconds, serve the built console (the page from the admin listener is not the
   placeholder and one of its assets is served), answer `phoebis status` inside the container with a first line ending
   in `ready`, take a `phoebis backup` onto the volume and stop within the grace period, and a second container under a
   certificate pair the smoke generates, whose certificate verifies the console over TLS. Beside the images it builds a
   static binary for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` inside the `cross` tool's images, runs
   each on its own platform (`--version`, a static link check, `init` and `check-config` in a temporary directory) and
   packs it with the license. Once every smoke passes it publishes one manifest list at `ghcr.io/daystrom-labs/phoebis`
   under `X.Y.Z`, `X.Y` and `X` (from major 1, never for a prerelease), `latest` (never for a prerelease), and
   `sha-<short commit>`, and uploads the two archives with `SHA256SUMS` to the release.

A failed smoke, of the image or of a binary, leaves untagged digests in the registry, no tag and no archive on the
release; a failed guard leaves nothing. A manual dispatch (Actions, Release, Run workflow) is the dry run: it builds
both platforms and runs the smoke checks without pushing; run it after a change to the Dockerfile, `Cross.toml` or the
workflow, since a pull request builds the amd64 side alone, and before the first release. The package inherits the
repository's visibility: while the repository is private, pulling needs a token with `read:packages`. The image name is
derived from the repository that runs the workflow, so a renamed repository publishes under its new name without a
workflow change.

## License

`phoebis` is published under the MIT license. The terms are in the `LICENSE` file at the root of the repository, which
the workspace manifest declares and the container image carries at `/usr/share/doc/phoebis/LICENSE`.
