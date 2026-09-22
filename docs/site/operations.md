# Operations

The router keeps its state in two SQLite files and one key file. This page covers backing them up, what the storage
key protects, upgrading the binary or the container, re-pinning the Claude Code build the router is measured against,
publishing this site, and releasing the container image.

## Backup

The router's state lives in three files: `storage.path`, the registry of accounts, sealed credentials, pools,
keys, alert rules, pricing versions, pins, cooldowns, and the relay's session memory; `stats.ledger_path`, the
ledger of requests, attempts, events, samples, rollups, and captured bodies; and `storage.encryption_key_file`,
the key that seals every credential the registry holds. No script in this repository takes a backup of any of
them.

Both databases are SQLite, and both run in WAL mode. A plain file copy taken while the service is running can land
mid-transaction and split either database file from its `-wal` file, losing the last transactions in between.
SQLite's own `.backup` avoids that by copying a consistent snapshot instead.

Create the destination directory before either backup, and make it private first: `.backup` does not carry the
source file's `0600` mode onto the copy it creates, and the ledger backup holds the prompts and tool results its
captured bodies contain, in plain text.

```sh
install -d -m 0700 /path/to/backups
sqlite3 /var/lib/phoebis/router.sqlite ".backup '/path/to/backups/router-$(date +%F).sqlite'"
sqlite3 /var/lib/phoebis/ledger.sqlite ".backup '/path/to/backups/ledger-$(date +%F).sqlite'"
```

Run as the `phoebis` user, this produces a consistent copy of each database without stopping the service. Where `sqlite3`
is not installed, Python's `sqlite3` module does the same thing:

```python
import pathlib, sqlite3

pathlib.Path("/path/to/backups").mkdir(mode=0o700, exist_ok=True)
sqlite3.connect("/var/lib/phoebis/router.sqlite").backup(sqlite3.connect("/path/to/backups/router.sqlite"))
sqlite3.connect("/var/lib/phoebis/ledger.sqlite").backup(sqlite3.connect("/path/to/backups/ledger.sqlite"))
```

A backup taken before an upgrade needs both snapshots to succeed: the registry and the ledger migrate together,
and a restore built from only one of them leaves the two databases at different points in the request history.

The key seals every credential in the registry, so a backup that carries the database is useless without the key,
and dangerous with it. Keep the key somewhere separate, such as a password manager or a secrets store, and never
beside the database copy. The ledger holds captured bodies, which are prompts, so a ledger backup needs the same
care as the registry.

## The storage key

A lost key loses every credential the registry holds and every downstream key it authenticates: without it, no
sealed credential can be opened again, and no downstream key's digest can be reproduced to check a client's secret.

The key is 32 raw bytes or 64 hex characters, held in the file `storage.encryption_key_file` names. A fresh
installation creates it with `head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'`, written into a file created
empty at mode `0600` before the key ever reaches it. Writing through a redirect under the usual umask would leave
the key world-readable for the moment between the file's creation and a later `chmod`, and an interrupted run would
leave it that way for good.

The key seals OAuth credential bundles, API-key secrets, and alert secrets. `Store::digest_key` also derives, from this
same key, the digest every downstream key authenticates against: the secrets the admin API mints and hands to clients
and the companion.

There is no re-key command, so recovering from a lost or replaced key means:

- Re-pairing every `oauth` account, after disconnecting it first. Confirming a pairing reads the account's
  existing credential to check its generation before committing the replacement; sealed under the key that was
  just replaced, that read fails, and the pairing dialog answers `500 storage failure`. Disconnect the account
  first, from the Upstreams view's Disconnect or `DELETE /admin/accounts/{id}/credentials`, which deletes the
  credential without reading it and leaves the account's identity bound to the same slot, then re-pair.
- Re-entering every `api_key` secret and alert secret under the new key.
- Rotating every downstream key. Replacing the storage key changes what `digest_key` derives, so every digest
  already stored against the old key stops matching a client's presented secret, and every existing downstream key
  starts answering `401`. Rotate each one (`POST /admin/keys/{id}/rotate`) or recreate it, then distribute the new
  secrets to the clients and the companion.

## Upgrades

1. Build the new binary: `cargo build --release`.
2. Validate the live configuration file with the binary just built, not the one still installed, since step 5 is
   what replaces that one: `sudo bash -c 'set -a; . /etc/phoebis/phoebis.env; set +a; exec target/release/phoebis check-config
   --config /etc/phoebis/config.yaml'`. `check-config` resolves its secrets from the process environment, and only
   systemd's `EnvironmentFile` loads `/etc/phoebis/phoebis.env` automatically, so sourcing it into this one shell is what
   lets the command run at all; skipping that fails with `environment variable ROUTER_ADMIN_SECRET is unset or
   empty`. `check-config` opens neither the store nor a socket, so it is safe to run while the old binary still
   serves traffic.
3. Read the [changelog](changelog.md) for a bootstrap key that changed.
4. Take the [backup](#backup) above.
5. Install the new binary: `sudo install -m 0755 target/release/phoebis /usr/local/bin/phoebis`.
6. Restart the service: `sudo systemctl restart phoebis`. The service drains in-flight requests for up to 30 seconds;
   the unit's `TimeoutStopSec=40` covers that drain.
7. Confirm the upgrade with `phoebis --version`, then `GET /admin/system` for `version`, `storage_ok`, and
   `ledger.write_failures` reading `0`.

Under the container, the same sequence is: read the changelog, take the backup, set `image:` in `compose.yaml` to
the new version, validate the mounted configuration with the new image (`docker compose run --rm phoebis
check-config`, which reads the service's environment file and mounts), then `docker compose up -d`. Compose
replaces the container, stopping the old one with the 40-second grace period that covers the drain, and the new
binary migrates both databases on the volume as it opens them. `docker compose ps` reads `healthy` once
`phoebis health` has answered, and `GET /admin/system` confirms the version.

Schema migrations run when the new binary opens each database, in order above the stored version, each inside its
own transaction. The registry's migrations are additive, so an older binary can still read a registry a newer one
migrated. A ledger migration can rebuild a table instead, so returning to an older binary after an upgrade means
restoring from the backup, not downgrading the binary in place.

Every configuration block denies unknown keys, so a key the new build does not recognize refuses startup; running
`check-config` first catches that before the restart does.

## Re-pinning the client build

A "pinned" build is the one Claude Code release the router is measured against: `claude-code-2.1.269-linux-x64`, named by
`compatibility.profile`. A profile is a fixed set of constants, the version and build tag, the two user agents,
the Stainless headers, the three beta lists, the control-plane URLs and client id, and the TLS curves and cipher
list, checked against the fixtures under `crates/profile/fixtures/`.
A newer client build is not relayed in its own shape automatically: headers
travel as the client sends them, but the betas an account may claim come from the profile, and the fields the router
restores for a Claude Code request are the ones on the [client page](client.md#the-environment-block), not what a newer
client might add.

To re-pin the profile against a new build:

1. With the build installed under `~/.local/share/claude/versions/<version>`, run `scripts/capture-baseline.sh
   <path-to-claude-binary> <output-dir>`. It re-executes itself under `unshare -rn` so the client has loopback as
   its only route, drives the four credential modes and a gzip-forced run past a recording receiver, records the
   ClientHello through a CONNECT receiver, and finishes by running `python3 scripts/build-fixtures.py --check
   <output-dir>`, exiting with that command's status. It refuses to start with a real `ANTHROPIC_API_KEY` or a
   real `CLAUDE_CODE_OAUTH_TOKEN` set in the environment. The system prompt the client sends embeds the current
   date, so a re-capture taken on another day differs there; `--check` prints a bounded diff per body so that
   shows as a date difference rather than a shape change.
2. Read the diff. A header added or renamed, a beta list that changed, or a new body field is the signal that the
   pinned profile no longer matches the client.
3. The control plane (the token exchange and the profile, usage, and roles calls) is not captured by the script:
   it needs a manual session behind a CONNECT tunnel, which the script's header comment describes. `--check` lists
   those fixtures under `not captured (control plane):` rather than failing on them.
4. Land the re-pin as a change to the repository: `python3 scripts/build-fixtures.py <output-dir>` writes the new
   fixtures; a new `Profile` value in `crates/profile/src/lib.rs` carries the new constants, and `by_name` learns
   its name; the tests in
   the profile, relay, and router crates grade the relay against the new fixtures. Until that change lands,
   `compatibility.profile` keeps naming the one profile this build carries.
5. `scripts/capture-through-router.sh` captures what the router hands its own upstream from a real paired account
   and grades it with `--through-router`. Its header names the two router keys it needs, that the service must be
   stopped while it runs since it opens the same store the running service does, and that it shares its client
   state with the baseline capture, so the two scripts never run at the same time.
6. `scripts/capture-count-tokens.sh <path-to-claude-binary> <output-dir>` records the client's own token count in
   each credential mode, inside the same namespace and past a receiver that answers the first request with a
   canned tool call, so the count the Read tool sends before a large file reaches the wire; `python3
   scripts/build-fixtures.py --count <output-dir>` writes the `count_*` fixtures, and `--check` beside it grades a
   new build against them.

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

To build the site locally, run `pipx run zensical build --clean` at the repository root; it writes `site/`. `pipx
run zensical serve` previews it.

## Releasing

A GitHub release publishes the container image. The workflow runs on a published release, on a manual dispatch,
and on a pull request that touches the image's inputs (the Dockerfile, the workflow, the manifests, the crates, or
the console's sources). To release:

1. Set `[workspace.package].version` in `Cargo.toml` to the version (`X.Y.Z`, or `X.Y.Z-rc.1` for a prerelease),
   head the changelog's section for it `## X.Y.Z`, and merge to `main`.
2. Create the release on GitHub with the tag `X.Y.Z` on that commit, no `v` prefix. Publishing it starts the
   workflow.
3. The workflow refuses the release before anything builds when the tag has another shape, differs from the
   workspace version, or has no `## X.Y.Z` section in the changelog, naming both values. Otherwise it builds
   `linux/amd64` and `linux/arm64` on native runners and pushes each by digest, runs the amd64 image through
   `--version` (which must print the version), `check-config` against `deploy/config.container.yaml`, and a
   container in the compose example's shape that must reach `healthy` within 30 seconds, serve the built console
   (the page from the admin listener is not the placeholder and one of its assets is served) and stop within
   the grace period, and then publishes one manifest list at `ghcr.io/daystrom-labs/phoebis` under
   `X.Y.Z`, `X.Y` and `X` (from major 1, never for a prerelease), `latest` (never for a prerelease), and
   `sha-<short commit>`.

A failed smoke leaves untagged digests in the registry and no tag; a failed guard leaves nothing. A manual dispatch
(Actions, Release, Run workflow) is the dry run: it builds both platforms and runs the smoke checks without pushing;
run it after a change to the Dockerfile or the workflow, and before the first release. The package inherits the
repository's visibility: while the repository is private, pulling needs a token with `read:packages`.

## License

The workspace manifest declares an MIT license, but the repository carries no license file, so its terms are not
published anywhere a reader can find them.
