# Operations

The router keeps its state in two SQLite files and one key file. This page covers backing them up and restoring them,
what the storage key protects and how a lost one is recovered, reading a running router with `phoebis status`,
upgrading the binary or the container, renewing a certificate pair, and what a drift refusal is and what ends it. The
build and the tests are on [Development](development.md).

## Backup

The router's state lives in three files. `storage.path` is the registry: the accounts and their sealed credentials,
the cooldowns, the keys, the pins, the pools, the pricing versions and the relay's session memory. The registry also
holds what a companion mod reports, sealed under the storage key: the branches, paths, remotes and titles of a session,
and the descriptions, names and types of its agents. Each backup of it carries the sealed rows. The registry keeps a
session's own facts until `sessions.expire_after_seconds` has passed since the session's last report, and an agent's
facts until `stats.retention_days` has, and the hourly retention pass removes them. The deletion of a key removes the
facts of its sessions at once. `stats.ledger_path` is the ledger: the attempts, captured bodies, events, requests,
rollups and samples. `storage.encryption_key_file` is the key that seals every credential the registry holds.
`phoebis backup` takes a snapshot of both databases and names the key file, and it never copies the key. A router with
the `features` block holds two more files in the data directory, `feature-ca.pem` and `feature-ca-key.pem`, the feature
CA and its key, which `phoebis backup` leaves out too (step 5 of [Restore](#restore)).

Both databases are SQLite in WAL mode. A plain file copy taken while the service runs can land mid-transaction and
split a database file from its `-wal` file, losing the last transactions. SQLite's online backup API copies a
consistent snapshot instead, and `phoebis backup` uses it.

A process crash loses nothing that committed, since the WAL is written before a commit returns. The ledger runs with
`synchronous = NORMAL`, so a power loss or an operating-system crash may lose its last transactions. The registry runs
with `synchronous = FULL`, so every change to its own rows (accounts, credentials, keys, members, pools, refresh
attempts, rules and secrets) is on disk before it is acknowledged. The routing state (affinity, cooldowns, the marks a
failing account earns, pins, usage windows, and an operator's cooldown clear) commits at `NORMAL` in one transaction per
writer wake, so a power loss can lose the last of it: the router rebuilds it from the next requests, and an operator
redoes a clear it undid.

```sh
phoebis backup --to /path/to/backups/$(date +%F)
```

Under the container the command runs in the running container and writes onto the data volume. `docker compose cp`
copies the directory out of the volume onto the host, or a bind mount over `/var/lib/phoebis/backups` puts it there:

```sh
docker compose exec phoebis phoebis backup --to /var/lib/phoebis/backups/$(date +%F)
docker compose cp phoebis:/var/lib/phoebis/backups/<name> .
```

`phoebis backup` copies each database through the online backup API from a read-only connection, one consistent
snapshot per database inside one read transaction, while the router serves or while it is stopped. It takes no lock,
opens no socket and loads the file without the admin secret, as `check-config` does. It runs as the user that owns the
databases (`phoebis` on the host, uid 65532 in the container, which `exec` already is), since the files and their
`-shm` files are `0600`. `--to DIR` names the directory, and without it the directory is
`./phoebis-backup-<YYYYMMDD>T<HHMMSS>Z`, stamped in UTC. `backup` creates the directory and every missing parent at
`0700`, or takes an existing empty directory that only its owner can write, and refuses one that group or other can
write or that another user owns, since the ledger copy holds the prompts and tool results its captured bodies contain,
in plain text. A mode wider than `0700` that gives group or other no write access is taken with a warning. The
directory then holds `router.sqlite`, `ledger.sqlite` and `manifest.json`, each at `0600`, and nothing else.

`manifest.json` is written last, once both copies have answered `PRAGMA quick_check` with `ok` and every file and the
directory are synced, so a directory that holds a manifest holds two complete copies. It carries `format` (1),
`phoebis_version`, `taken_at` (UTC), `config` (the file read), `key_file` (the path `storage.encryption_key_file`
names, and nothing derived from the key, which `backup` never reads) and, under `registry` and `ledger`, each copy's
`source`, `schema_version`, `bytes` and `sha256`. `--json` prints the manifest on stdout, which otherwise stays empty.
Stderr carries one `wrote {path} ({n} bytes)` line per file, `quick_check on {path}: ok` for each copy,
`manifest at {path}`, and last `the key file {path} is not in this backup. Keep it apart, since the registry copy is
sealed under it`. A key file that does not exist earns `warning: the key file {path} does not exist, and the registry
copy cannot be opened without the key that sealed it`, and the backup proceeds.

`backup` exits 1 with one `Error:` line and nothing written when the file does not load, when `--to` names a file or a
directory that is not empty (`{path} is not an empty directory`), when it names a directory that group or other can
write or that another user owns (`{path}: group or other can write it, or another user owns it. Nothing was written.`),
or when either database does not exist (`a backup carries both databases, but {path} does not exist`). A copy that
fails, a database another connection holds locked for ten seconds among them, removes every file and directory the
run created and exits 1 with `backup failed: {reason}. Nothing was written to {dir}`. It exits 0 once the manifest is
written, and 2 for a usage error ([Commands](installation.md#commands) has the row). Where the binary is not at hand,
`sqlite3`'s `.backup` produces the same snapshot, one database at a time.

A backup taken before an upgrade needs both snapshots: the registry and the ledger migrate together, and a restore
built from one of them alone leaves the two databases at different points in the request history.

The key seals every credential in the registry, so a backup that carries the database is useless without the key, and
dangerous with it. The registry also holds the keepalive engine's retained recipes, each prompt sealed under the same
key, and the facts that a companion mod reports, sealed the same way, so a restored registry needs its key for them as
it does for the credentials. Keep the key somewhere separate, such as a password manager or a secrets store, and never
beside the database copy. The ledger holds captured bodies, which are prompts, so a ledger backup needs the same care as
the registry.

## Restore

The binary has no restore command: a restore is the procedure below, which puts both databases back with the key that
sealed the registry, in this order:

1. Stop the router: `sudo systemctl stop phoebis`, or `docker compose stop`.
2. Move the current `router.sqlite` and `ledger.sqlite` aside, each with its `-wal` and `-shm` files.
3. Copy each file of the backup to the path the configuration names, at mode `0600` and owned by the service user:
   `sudo install -m 0600 -o phoebis -g phoebis <backup>/router.sqlite /var/lib/phoebis/router.sqlite`, and the same
   for `ledger.sqlite`. The copies are in rollback-journal mode, and the router's open puts them back in WAL mode.
4. Put the key file that sealed the registry at `storage.encryption_key_file`. The manifest's `key_file` says where it
   lived. A wrong key is refused at start and by `check-config` with `the storage key does not match the registry at
   {path}`.
5. On a new host, put the certificate pair where the file names it: the backup carries neither the pair nor the key. A
   router with the `features` block needs `feature-ca.pem` and `feature-ca-key.pem` in the data directory as well, as
   the first start wrote them, at `0600` and owned by the service user. Without them the first start creates a new CA
   with a new fingerprint, and each user of the claude.ai features needs that fingerprint before the launch script
   starts a client again ([The CA over time](claude-ai-features.md#the-ca-over-time)).
6. Run `phoebis check-config`.
7. Start the router: `sudo systemctl start phoebis`, or `docker compose up -d`.
8. Confirm with `phoebis status`, which reads `ready`.

Under the container the two copies go onto the `phoebis-data` volume, owned by uid 65532 at `0600`, from the host or
from a one-off container that mounts the volume, as the [storage key](#the-storage-key) recovery runs. Steps 6 and 8
are `docker compose run --rm phoebis check-config` and `docker compose exec phoebis phoebis status` there.

The binary that restores is the manifest's `phoebis_version` or newer. An older binary refuses a copy a newer build
wrote with `the registry's schema version {found} is newer than the {supported} this build opens: restore the backup
taken before the upgrade, or recreate a registry a pre-release build wrote with phoebis init`, and the ledger's line
reads the same with `ledger` in place of `registry`. A newer binary migrates the copies as it opens them.

A registry restored alone brings back the accounts, their sealed credentials, the keys and the pools, and the ledger
starts empty, so the keepalive ledger, the parked records, the request log, the rollups, the statistics and the usage
samples are gone, while the cooldowns and pins rebuild from the next requests. A ledger restored alone brings back the
statistics, and the registry starts empty, so every account is paired again and every key is minted anew with a new
secret, and the ledger's rows name account and key ids that nothing resolves.

## The storage key

A lost key loses every credential the registry holds and every downstream key it authenticates: without it, no
sealed credential can be opened again, and no downstream key's digest can be reproduced to check a client's secret.

The key is 32 raw bytes or 64 hex characters, held in the file `storage.encryption_key_file` names. A fresh installation
creates it with `phoebis init`, which writes the 64 hex characters and a newline into a file created at mode `0600` in
one open. Where the binary is not at hand, `head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'` written into a file
created empty at `0600` is the same key. Writing through a redirect under the usual umask would leave the key
world-readable for the moment between the file's creation and a later `chmod`, and an interrupted run would leave it
that way for good. The key file may end in one newline after the 64 hex characters, so a key written as a line is
accepted too, such as `printf '%s\n' "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"` written into that same
empty file, and any other shape is refused with `storage key file must hold exactly 32 raw bytes or 64 hex characters,
with at most one trailing newline`.

The registry carries a canary sealed under the key at its first open, and every open proves the key against it before
any credential is read: a restore onto a host whose key file differs, or a key regenerated after the file was moved, is
refused at start and by `check-config` with `the storage key does not match the registry at {path}`. A registry from
before the canary existed is proved against its sealed rows instead, the first 32 of each kind (credential bundles,
API-key secrets, keepalive recipe bodies and the facts that a companion mod reports), passing when any of them opens,
and the next start seals its canary. `check-config` names such a registry with `registry at {path} has no key canary
yet. A sealed row opens under the key, and the next start seals one`. The proof's scope, the first 32 rows of each kind,
leaves one shape of a registry without a canary refusing the right key at the start: one whose readable credential
bundles, API-key secrets, recipe bodies and reported facts all sit past the 32nd of their kind. `phoebis recover-key` is
the way out: it scans every sealed row, so on such a registry it seals the canary under the key that opens the rows and
exits 0 with `the storage key opens {readable_rows} sealed {rows} and the registry has no canary, so a canary was sealed
under it and nothing was discarded`, keeping every row, and the next start passes through the canary.

`init` refuses a key file that exists with `{path} already exists, and init never replaces a key, since every stored
credential is sealed under it`, and refuses a data directory that already holds a registry, so a lost key is never
papered over by a new one.

The key seals the OAuth credential bundles, the API-key secrets, the keepalive engine's retained recipe bodies and the
facts that a companion mod reports. From the same key the store derives the digest every downstream key authenticates
against, so the secrets the admin API mints and hands to the clients are checked under it too.

Under a lost or replaced key no sealed row opens again and the canary refuses the start, so recovering means discarding
what the old key sealed while keeping everything else. `phoebis recover-key` does that on a stopped registry. It is a
recovery, not a configuration step, and the accounts, pools and keys it leaves stay the console's to edit. On the
service layout, as root:

1. Stop the service: `sudo systemctl stop phoebis`.
2. Take a [backup](#backup): `phoebis backup --to <dir>`.
3. Run `sudo phoebis recover-key --config /etc/phoebis/config.yaml --owner phoebis`. A key file already in place is
   read as the new key. When the file `storage.encryption_key_file` names is missing, the run generates the key in
   memory and writes its file once the operator confirms (at once under `--yes`), as `init` does, at `0600` and owned
   by the `--owner` user with the directories it created for it, then prints `wrote a new storage key to {path} (mode
   0600)`, so a run stopped at the prompt leaves no key file. The `--owner` user also receives the registry's lock
   file when this run creates it, and handing these to the `--owner` user needs root (`--owner needs root` refuses a
   run without it).
4. Read its summary, which opens `the recovery discards what the old key sealed:` over the counts of OAuth credential
   bundles, API-key secrets, keepalive recipe bodies and companion fact sets, then `and marks the accounts:` over the
   counts of OAuth accounts disconnected, API-key accounts unpaired and open refresh attempts failed.
5. Answer the prompt `type yes to discard them: `. Any other answer prints `the registry was not changed` and exits 2,
   and a declined run discards nothing, writes no key file and keeps the registry's lock file. `--yes` skips the
   question for a script.

It refuses with `the registry is locked, stop the service first` while a router holds the registry, with `the storage
key matches the registry, nothing to recover` when the key file still opens the registry's canary, with `the storage key
opens {readable_rows} sealed {rows} but not the corrupt canary, so nothing was discarded: restore the canary from a
backup` (`{rows}` reading `row` or `rows`) when the canary alone does not open, since the canary is then the corrupt
part and a backup restores it, and with `no registry at {path}, nothing to recover` when the registry does not exist.
Before it discards anything it tries every sealed row, never a sample. Each refusal exits 1. A key file that opens the
sealed rows of a registry without a canary is no refusal: the run seals the canary under its key in one transaction,
prints `the storage key opens {readable_rows} sealed {rows} and the registry has no canary, so a canary was sealed under
it and nothing was discarded` and exits 0, without the summary, the prompt or the discard. Otherwise it prepares the
registry's schema, running every migration above the stored version, and a preparation that fails prints `the registry's
schema could not be prepared for the recovery, so nothing was discarded: {error}` and exits 1. A run that stops before
the preparation leaves the registry at the schema version it found, and one whose preparation fails leaves it at the
step it reached, since each migration step commits alone. A run that exits 1 or 2 removes the key file it wrote, with
any directory it created for it, and names it in the line `removed the key file {path} this run created`, with a line
after it for each it could not remove. Every run keeps the registry's lock file, a refused open included, since a start
that opened the file while the run held it goes on to lock that same file, and a file created again in its place would
let a later start lock a second one beside it. Then it runs the sequence under [What it runs](#what-it-runs) as one
transaction, seals a new canary under the new key in that same transaction, prints the counts again under `discarded
what the old key sealed:` and `and marked the accounts:`, then `sealed a new canary under the key at {path}`, then
`start the service, then:` over the three steps that remain, and exits 0 once the accounts are marked, keeping the key
file and the lock file for the service's next start. It prints counts and paths, never a key. Then start the service,
and:

- Re-enter every API-key secret in the console.
- Pair every OAuth account again in the console.
- Rotate every downstream key. Replacing the storage key changes the digest the store derives, so every digest already
  stored against the old key stops matching a client's presented secret, and every existing downstream key starts
  answering `401`. Rotate each one (`POST /admin/keys/{id}/rotate`) or recreate it, then distribute the new secrets to
  the clients.

Under the container the key is the Compose secret `./storage.key`, which Compose refuses to start without, so the new
key is written on the host first and the subcommand reads it rather than generating one. Take the backup with `docker
compose exec phoebis phoebis backup --to /var/lib/phoebis/backups/<name>` before `docker compose stop`, since `exec`
needs the running container, or with `docker compose run --rm phoebis backup --to /var/lib/phoebis/backups/<name>` once
it is stopped. Where `./storage.key` is gone, write the new key with `(umask 077 && openssl rand -hex 32 > storage.key)`
and hand it over with `sudo chown 65532:65532 storage.key && sudo chmod 0400 storage.key`. Then run `docker compose run
--rm phoebis recover-key`, which takes the same answer or `--yes`.

### What it runs

The subcommand runs this sequence. Where the binary is not at hand, an operator runs it by hand against the registry
as the `phoebis` user, after the same stop and backup:

```sh
sudo -u phoebis sqlite3 -bail /var/lib/phoebis/router.sqlite <<'SQL'
BEGIN IMMEDIATE;
DELETE FROM meta WHERE key = 'storage_key_canary';
DELETE FROM credentials;
DELETE FROM api_key_secrets;
UPDATE keepalive_agents SET body_sealed = NULL, body_nonce = NULL WHERE body_sealed IS NOT NULL;
DELETE FROM companion_facts;
UPDATE accounts SET state = CASE kind WHEN 'oauth' THEN 'disconnected' ELSE 'unpaired' END,
  state_cause = 'operator', lifecycle_generation = lifecycle_generation + 1, revision = revision + 1,
  updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000,
  state_changed_at = CASE WHEN state = CASE kind WHEN 'oauth' THEN 'disconnected' ELSE 'unpaired' END
    THEN state_changed_at ELSE CAST(strftime('%s', 'now') AS INTEGER) * 1000 END;
UPDATE refresh_attempts SET resolved_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000, outcome = 'failed',
  detail = 'disconnected', cause = 'disconnected' WHERE resolved_at IS NULL OR outcome = 'uncertain';
COMMIT;
SQL
```

Under the container the image ships no `sqlite3`, so the sequence runs as uid 65532, either from the host against
`router.sqlite` on the `phoebis-data` volume or from a one-off container that carries `sqlite3` and mounts that volume.

The sequence discards everything that the storage key seals, and nothing else: the API-key secrets, the canary, the
OAuth credential bundles, the keepalive's recipe bodies and the facts that a companion mod reported go, and each account
rests where the Accounts view's Disconnect leaves it, with the cause `operator` and its open refresh attempts settled.
The accounts, downstream keys, pins, pools, the session engine's sessions and the relay's session memory stay. `-bail`
stops at the first statement that fails, and the transaction it leaves open rolls back, so the file is either wholly
changed or untouched. The subcommand seals the new canary inside the same transaction, and after the sequence run by
hand the next start under the new key finds nothing sealed and seals one. The steps that remain are the same either way.

An operator who would rather start over moves the registry file aside instead, with its `-wal` and `-shm` files if
present, and lets the next start create an empty registry, in which every account, pool and key is created again.

## Is it up?

`phoebis status` is the operator's read and `phoebis health` the orchestrator's: `health` sends one `HEAD /api/ready`
to the proxy listener and prints one line, while `status` runs that probe beside its reads of `GET /admin/system`,
`/admin/attention`, `/admin/pools`, `/admin/accounts`, `/admin/sessions` and `/admin/settings`, which it makes in that
order with the admin secret from the environment, each call under the `--timeout` deadline, and prints one screen:

```text title="phoebis status"
phoebis 0.1.0, up 3d 4h, ready
  build 9f3c2a7e5b1d4c6f8a0e2b4d6f8a1c3e5b7d9f0a
  proxy https://0.0.0.0:8317  admin http://127.0.0.1:8081
  metrics http://127.0.0.1:9464
  ledger:
    2 parked
  attention:
    pool_exhausted work since 12:03
    refresh_failed alice@example.com since 11:40

pools
  home  disabled  1 member, 0 keys, 0 pins, 0 requests/24h
    erin@example.com  ready  unavailable  5h -  7d -
  work  active    3 members, 2 keys, 2 pins, 41 requests/24h
    alice@example…  ready   eligible     5h 42%   7d 18%  resets 14:00
    bob@example.c…  ready   cooldown     5h 100%  7d 61%  until 15:30 rate_limit
    carol@example…  paused  unavailable  5h -     7d -

accounts in no pool
  dave@example.com  unpaired

pins (one per session, the main agent's)
  session       agent  account            state
  7b21aa90f3d1  a1     alice@example.com  shed from bob@example.com, 12s ago
  3f9a0c12e4b7  -      alice@example.com  pinned 4m ago, claude-opus-4-1
```

The header carries the version and the uptime from `GET /admin/system` and one of three readings: `ready`, `not ready:`
with each dead component (`ledger`, `router`, `store`), or `proxy down:` with the probe's line when the proxy listener
did not answer `200`, which wins over the other two. The line under it names the build as `build {commit}` from the same
answer, the full commit the router was built from or `unknown` for a build that had none. A build run with
`PHOEBIS_BUILD_COMMIT` set names that value, trimmed and lowercased, in place of the checkout's commit, and a blank
value reads as unset. When the admin API could not be read, the header carries the binary's own version and
`readiness unknown`, unless the proxy is down, and the line under it the binary's own build. The listeners follow with
their schemes as the file names them, `features https://{listen}` among them where the file has the `features` block.
Under them, a listener's certificate whose leaf expires within 30 days prints
`proxy.tls.cert_file {path} expires in {n} days ({date})` (or `admin.tls.cert_file`), and a certificate file the loader
refuses when the operator reads it prints the loader's line for it
([TLS on the listeners](installation.md#tls-on-the-listeners)). The running router keeps the pair it loaded until
`phoebis tls reload`. A router with the feature listener then prints `feature CA {fingerprint}, expires {date}`, the
end as a UTC date, from its settings answer, with a file or with `--url` alone, since the CA's files are the service
user's to read, and a settings read that failed prints `settings:` with its failure in that line's place. `ledger:`
prints only when a figure needs a look, and lists each one on its own line under it: records `queued` in the writer
channel, `behind` while admission refuses, `degraded` after a failed batch, priced records `parked` in the ledger
file, a batch `retrying`, and after `writer:` the state writer's `deferred` writes and its `degraded` flag (see
[API reference](api-reference.md)). `attention:` lists every attention item on its own line under it as
`{kind} {subject} since {HH:MM}`, in the items' order, or reads `attention: none`.

Under `pools` each pool prints by id with its status, its member, key and pin counts and its requests in the last 24
hours, then one line per member by priority, then account id: the account's label, its state (`paused` when routing is
off on it), the member's eligibility, the five-hour and seven-day windows in whole percent (`-` with no sample) and the
7-day Fable when the account has one, then the nearest reset as `resets {HH:MM}` or, under a cooldown, `until {HH:MM}
{reason}`, with the scope ahead of the reason (`fast`, `fast_credits`, `model:{canonical}`) when the cooldown holds less
than the whole account. A member in the `disconnected` or `reauth_required` state prints `-` under both windows, since
the router answers it none. A usage-based seat prints its spend window in place of the windows, after its eligibility
and outside the other members' columns: `spend {NN}%`, the spend and its cap in whole major units (`$1025 of $2500`, the
spend alone without a cap, `-` without amounts, a currency other than USD by its code), then `resets {HH:MM}` for a
head's reset, `resets {MM-DD} est` for the estimate on the first of the next month in UTC, or a cooldown as above. The
block reads `pools: none` when no pool exists. `accounts in no pool` lists the accounts no pool holds, with their state,
and is left out when there are none.

The pins block is the sessions route's answer, one line per session that holds a pin: the route groups pins by key
generation and session and prefers the main agent's pin, so a session whose subagent is pinned to another account shows
its main agent's line. Each line carries the first twelve characters of the session id, the agent id or `-`, the
account, and `pinned {age} ago, {model}`, or, on a pin a move stamped, `shed from {account}, {age} ago` when a shedding
round moved the session off that account and `failed over from {account}, {age} ago` when a failover landed it, followed
by `, keepalive {state}` and `next probe {HH:MM}` while the keepalive engine tracks it. Times on the screen print in the
host's zone as `HH:MM`, with the date in front as `MM-DD` when it is not today, while the router's log lines print their
instants in UTC with the zone named. Ages print in their largest whole unit as `12s`, `4m`, `3h` or `2d`. The block
reads `pins: none` when no session holds a pin.

A read that fails prints one line in its section, such as `pools: the admin listener did not answer on {url}: {reason}`
or `sessions: the admin secret was refused`, and the rest of the screen still prints. When the router is down, the
screen is the header with the binary's version and `proxy down: proxy does not answer on {addr}: {error}`, the
listeners, and one such line per read. A label column is as wide as its widest label where the line has the room. A
member's reset or cooldown word and the age on a `shed from` or `failed over from` line keep their width ahead of the
labels, so a long account label is cut before them, and once it reaches its floor of 12 columns the account after `shed
from` or `failed over from` is cut too. A cut label ends in `…`, and `--json` carries every label and id whole. A
table line past 80 columns is cut and ends in `…`, and a line of text past 80 columns wraps. The pins and the accounts
in no pool print 20 lines each and then `and {n} more`, and `--all` prints every one. `--json` prints the same reads as
one object for a script: `health` (`ready`, `addr` and `reason`, or `null` when no probe ran), the six answers verbatim
(`null` for a read that failed) and `errors`, each failed read's reason under its name. Neither output carries the admin
secret.

With `--url` and `--secret-env` both given, as for a router on another machine, the file is not read and `status` has
no proxy listener to probe: it prints `proxy: not probed, no configuration file names the listener` under the listeners
and reads readiness from `/admin/system` alone. The shared flags are on [Commands](installation.md#commands).

`status` exits 0 when `/admin/system` answers `ready` and the probe, where it ran, answered `200`, 1 otherwise (the
proxy does not answer, a component is dead, the secret is refused, the admin listener does not answer, or the file does
not load), and 2 for a usage error. `ready` is the liveness of the router's components, the answer `/api/ready` gives,
and not a promise that a request finds an account: a router whose every pool member is cooling down reads `ready` and
exits 0. `pool_exhausted` joins the attention lines once a request of the pool has ended exhausted, so a pool whose
members all cooled with no request since shows none, and a script that asks whether requests can be served reads each
member's eligibility under `pools` too. Under the container, `docker compose exec phoebis phoebis status` runs it in
the running container, which inherits the container's environment and so the admin secret.

## Upgrades and renewals { #upgrades }

On the host:

1. Build the new binary: `cargo build --release`.
2. Validate the live configuration file with the binary just built, not the one still installed, since step 5 is
   what replaces that one: `sudo target/release/phoebis check-config --config /etc/phoebis/config.yaml`. The secret is
   optional here (an unset one prints `warning: environment variable {name} is unset or empty`), and `check-config`
   takes no lock and opens no socket, so it is safe to run while the old binary still serves traffic. It reads the key
   file and proves it against the registry's canary, so a key file that drifted is caught before the restart.
3. Read the release's [changelog](changelog.md) entry for a bootstrap key it names as changed.
4. Take a [backup](#backup): `phoebis backup --to <dir>`.
5. Install the new binary: `sudo install -m 0755 target/release/phoebis /usr/local/bin/phoebis`.
6. Restart the service: `sudo systemctl restart phoebis`. The stop runs under one budget of 35 seconds, which cuts its
   phases in this order: the session engine's and the cache warming's shutdown and the listeners' drain of in-flight
   requests, the relayed bodies still running, the catalog's retries, a credential refresh still in flight, the ledger's
   last writes, then the routing state's. In-flight requests drain for up to 30 seconds inside the budget, which leaves
   the phases after them at least five. Each phase takes what the ones before it left, and the session memory's last
   flush and the store's close run whatever is left. The process then stops its runtime within what remains of the
   budget, so a ledger or routing write the budget cut, which still runs on a blocking thread, ends with the process,
   and a warn line names that write. The unit's `TimeoutStopSec=40` leaves five seconds past the budget.
7. Confirm the upgrade with `phoebis --version`, then `phoebis status` for `ready`, or `GET /admin/system` for
   `version`, `storage_ok`, `ready`, `ledger.dropped` and `ledger.write_failures` each reading `0`, and
   `router.dropped_reports` all reading `0`. `router.dropped_reports` counts the reports a closed queue refused at
   shutdown, zero on a running router, and a loop that stopped keeping up shows as `router.queued` at `command_depth`
   and as keys holding their requests past the answer.

Under the container, the same sequence is: read the changelog, take the backup (`docker compose exec phoebis phoebis
backup --to /var/lib/phoebis/backups/<name>`), set `image:` in `compose.yaml` to the new version, validate the mounted
configuration with the new image (`docker compose run --rm phoebis check-config`, which reads the service's environment
file and mounts), then `docker compose up -d`. Compose replaces the container, stopping the old one with the 40-second
grace period, five seconds past the same 35-second shutdown budget, and the new binary migrates both databases on the
volume as it opens them. `docker compose ps` reads `healthy` once `phoebis health` has read the router as ready, and
`GET /admin/system` confirms the version.

A listener's certificate pair is read once at start, before the listener binds its port, and re-read on `phoebis tls
reload`, which re-reads the paths the file named at start, so a moved path needs a restart. A renewal is: replace both
files, then reload. The service user reads the pair at reload as it did at start, so a new pair keeps the modes
[TLS on the listeners](installation.md#tls-on-the-listeners) gives. `certbot` and `acme.sh` do both steps in their
deploy hooks, here for the service unit's paths and its environment file:

```sh
sudo certbot certonly -d phoebis.example.com --deploy-hook '
  install -m 0644 "$RENEWED_LINEAGE/fullchain.pem" /etc/phoebis/proxy.pem &&
  install -m 0640 -g phoebis "$RENEWED_LINEAGE/privkey.pem" /etc/phoebis/proxy.key &&
  set -a && . /etc/phoebis/phoebis.env && set +a &&
  phoebis tls reload --config /etc/phoebis/config.yaml'
```

`phoebis tls reload` sends `POST /admin/tls/reload` and prints one line per listener: `{listener}: plain HTTP,
nothing to reload`, `{listener}: reloaded, certificate {cert_file} expires {expires}`, or `{listener}: not reloaded,
{error}. The router keeps the pair it was serving.`, with `the router named no reason` as the error when the answer
carries none. `--json` prints the route's body instead. It exits 0 once every pair reloaded, a plain listener aside, 1
when any pair was refused, the secret was refused, the admin listener did not answer or the file does not load, and 2
for a usage error. A pair that fails to load (a file the service user cannot read, a key that does not match its
certificate) keeps the pair the router was serving, and the next reload clears the failure. A reload that lands between
the two writes meets a key mismatch and is retried once both files are in place. Under the container the command is
`docker compose exec phoebis phoebis tls reload`. The dashboard raises `certificate_reload_failed` for a listener's pair
until a reload lands and `certificate_expiring` from 30 days before a loaded leaf's end, as
[Conditions and gauges](statistics.md#conditions-and-gauges) describes.

The feature listener's certificates need no step and no reload. The router issues them itself under its feature CA, each
for seven days, and serves the next one from the next connection on, about a day before the current one ends, so
`phoebis tls reload` names the proxy and the admin listener alone. The CA is valid for ten years, and what its
replacement means for each user is under [The CA over time](claude-ai-features.md#the-ca-over-time).

Schema migrations run when the new binary opens each database, every migration above the stored version in order, each
inside its own transaction, so a crash leaves the last version that landed. The registry's migrations may drop or
rebuild a table, and a ledger migration can drop or rebuild one too. This build writes the registry at schema 11 and the
ledger at schema 6. Both refuse to open a file a newer build wrote: the registry with `the registry's schema version
{found} is newer than the {supported} this build opens: restore the backup taken before the upgrade, or recreate a
registry a pre-release build wrote with phoebis init`, and the ledger with the same words, `ledger` in place of
`registry`. `phoebis check-config` reads both versions read-only, taking no lock, and refuses first with the same
sentence under `reading the registry at {path}` or `reading the ledger at {path}`. Returning to an older binary after an
upgrade therefore means restoring from the backup, not downgrading the binary in place.

Every configuration block denies unknown keys, so a key the new build does not recognize refuses startup, and running
`check-config` first catches that before the restart does.

### Certificates from step-ca, renewed by acme.sh { #step-ca }

This example gives the proxy and the admin listener a certificate from a CA of your own. A
[step-ca](https://smallstep.com/docs/step-ca/installation/) with an ACME provisioner signs it, and
[`acme.sh`](https://github.com/acmesh-official/acme.sh/wiki/How-to-install) fetches it, installs it and renews it, and
each link leads to that tool's own install. The CA runs on `ca.example.com` and the router on `router.example.com`. The
example ran with step-ca 0.30.2, its `step` CLI 0.31.0 and `acme.sh` 3.1.6, and both tools' defaults differ between
versions. The feature CA has no part in it: the router makes that CA itself for the two feature hosts, and step-ca signs
the router's own name alone.

On the CA's host, the first line creates the CA, the second adds the ACME provisioner and the third starts the CA on its
configuration:

```sh
step ca init --deployment-type standalone --name "Example CA" --dns ca.example.com --address :443 \
  --provisioner admin --password-file /etc/step-ca/password.txt
step ca provisioner add acme --type ACME --x509-default-dur 2160h --x509-max-dur 2160h
step-ca "$(step path)/config/ca.json" --password-file /etc/step-ca/password.txt
```

With these flags `step ca init` asks nothing. It reads the password of the CA's keys from the password file, so write
the file before the first line runs, with a password of your own in it and at `0600`. The start reads the same file,
since a service has no terminal. `step ca provisioner add` writes the provisioner into the CA's `ca.json`, so here it
runs before the CA starts. Its two duration flags make the provisioner issue leaves of 90 days. step-ca's default is 24
hours, and under it `certificate_expiring` stands from the first install on, since the item holds while a leaf is within
30 days of its end. `acme.sh` would also renew such a leaf too late most of the time: it renews a leaf of a day or less
in the leaf's last hour, and its cron entry runs every six hours, so a run falls into that hour about one time in six.

The router's host needs the CA's root certificate, `$(step path)/certs/root_ca.crt` on the CA's host. Copy it to
`/etc/phoebis/step-ca-root.crt` at `0644`, since it holds no secret.

The remaining lines run as root on the router's host. The key and the signing request are the operator's: the two
`install` lines create the pair's files at the modes [TLS on the listeners](installation.md#tls-on-the-listeners) gives,
and `openssl` writes the key into the first and a request for the router's name beside it:

```sh
install -m 0640 -g phoebis /dev/null /etc/phoebis/proxy.key
install -m 0644 /dev/null /etc/phoebis/proxy.pem
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout /etc/phoebis/proxy.key -out /etc/phoebis/router.csr \
  -subj /CN=router.example.com -addext subjectAltName=DNS:router.example.com
```

`openssl` and `acme.sh` both write into a file that exists and leave its mode and its owner as they are, so the two
`install` lines hold for each later renewal. `acme.sh` then has the provisioner sign the request:

```sh
acme.sh --signcsr --csr /etc/phoebis/router.csr --standalone \
  --server https://ca.example.com/acme/acme/directory --ca-bundle /etc/phoebis/step-ca-root.crt
```

`--server` is the provisioner's directory, `/acme/{name}/directory` on the CA, and `--ca-bundle` is the root `acme.sh`
trusts the CA by. `--standalone` answers step-ca's challenge itself, through `socat` on port 80 of the router's host, so
step-ca has to reach `router.example.com` there. `acme.sh` reads the name from the request, and the line takes no `-d`.

The install copies the certificate and its chain to the pair's path. The first install names no reload command, since
the router does not serve the pair yet and `acme.sh` ends an install with status 1 when its reload command fails:

```sh
acme.sh --install-cert -d router.example.com --fullchain-file /etc/phoebis/proxy.pem
```

The line names no key file either. Under a signing request `acme.sh` holds no key, and an install with `--key-file` ends
with status 1 and leaves the file it names empty.

Both listeners then name the pair in the bootstrap file. The router reads a pair once at its start and refuses to start
without a pair it can load, so the blocks follow the first install, and a `tls` block added to a running router's file
takes a restart, here `systemctl restart phoebis`:

```yaml title="config.yaml"
admin:
  listen: 0.0.0.0:8081
  tls:
    cert_file: /etc/phoebis/proxy.pem
    key_file: /etc/phoebis/proxy.key
  secret_env: PHOEBIS_ADMIN_SECRET
proxy:
  listen: 0.0.0.0:8317
  tls:
    cert_file: /etc/phoebis/proxy.pem
    key_file: /etc/phoebis/proxy.key
```

One pair serves both listeners: `proxy.tls` and `admin.tls` name the same two files, since `acme.sh` installs the
certificate at one path and the key stays at its own.

The block takes the admin listener off the loopback address of a host install, `127.0.0.1:8081`, and binds it on every
interface, because the reload command below reaches the listener by the router's name. On `0.0.0.0` the console's login
and each admin route answer on each network the host is on, behind TLS and the one admin secret. To narrow the bind, put
an address of the router's host in place of `0.0.0.0`, one that `router.example.com` resolves to on that host. The
listener then answers on that address alone, and the reload command stays as printed, `--url` included, since the leaf
names the router's name and no address.

The install then runs once more, with the reload command that `acme.sh` stores and runs after each renewal:

```sh
acme.sh --install-cert -d router.example.com --fullchain-file /etc/phoebis/proxy.pem --reloadcmd '
  set -a && . /etc/phoebis/phoebis.env && set +a &&
  phoebis tls reload --config /etc/phoebis/config.yaml \
    --ca /etc/phoebis/step-ca-root.crt --url https://router.example.com:8081'
```

The command reads the service unit's environment file ahead of its line, as the `certbot` hook above does. Its line
carries `--ca` and `--url` because the pair is on the admin listener too: the admin client trusts the system's roots
unless `--ca` names a bundle, and its default address is the loopback address, which the leaf does not name. The install
runs the command at once, and its two lines show that the router serves the pair:

```text
proxy: reloaded, certificate /etc/phoebis/proxy.pem expires {expires}
admin: reloaded, certificate /etc/phoebis/proxy.pem expires {expires}
```

The renewal is `acme.sh`'s own from here on. The cron entry of its install runs `acme.sh --cron`, and a run renews each
certificate that is due, installs it at the stored path and runs the stored reload command. For a certificate that is
not due it prints `Skipping. Next renewal time is: {date}`, and for a leaf of 90 days that date lies 29 days after the
issue. A renewal signs the same request again, so the key file stays as it is and the certificate file alone changes. A
forced renewal walks the whole road at once:

```sh
acme.sh --renew -d router.example.com --force
```

It ends in the two `reloaded` lines again, with the later end of the new leaf. The cron entry sends the standard output
of `acme.sh` to `/dev/null`, the reload's two lines with it, so a renewal on the timer is read from the router's log,
which holds `proxy.tls: certificate pair reloaded (certificate {cert_file}, expires {date})` and the same line for
`admin.tls`, or from `phoebis tls reload` run by hand, which prints the served leaf's end again. `certificate_expiring`
is the sign that renewals stopped: the item stands from 30 days before the served leaf's end until a leaf with more than
30 days left is loaded ([Conditions and gauges](statistics.md#conditions-and-gauges)), and `phoebis status` lists it
under `attention:`.

A client trusts the certificate through the same root: a copy of `step-ca-root.crt` on the client's machine, named in
`NODE_EXTRA_CA_CERTS` beside `ANTHROPIC_BASE_URL` in the client's [environment block](client.md#the-environment-block).
`phoebis status` and `phoebis tls reload` take the file through `--ca`, with the router's name through `--url`, as the
reload command carries them. In a feature setup [the launch script](claude-ai-features.md#the-launch-script) takes the
root as `BASE_CA` and writes it and the feature CA into one file, since the client's variable names one file.

## Re-pinning the client build

A drift refusal is the `client_drift` answer to a request whose billing hash or `cc_version` suffix this build cannot
reproduce. [Client drift](relay.md#client-drift) has the two checks and their messages, and the formula they check
against comes from the one profile this build carries. The refusal lasts until the router runs a build re-pinned to the
client: no setting clears it. What the router restores and edits is on [The relay](relay.md#the-restoration). The
`client_drift` attention item dates from the first refusal of a client version, while its key, its trigger and its
link follow the newest one ([Conditions and gauges](statistics.md#conditions-and-gauges)). The re-pin is a source
change and a release: a capture of the new build's requests, a new profile with its constants, the fixtures
regenerated and the tests graded against them, then a release the operator installs. The capture steps are on
[Development](development.md#re-pinning).

## On other pages

The continuous checks that run on every push and pull request are on [Development](development.md#continuous-checks).
Publishing this site is on [Development](development.md#publishing-the-site).

<a id="releasing"></a>

Releasing the container image and the static binaries is on [Development](development.md#releasing).

The license is on [the Overview](index.md#license).
