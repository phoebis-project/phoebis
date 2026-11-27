# Installation

You are putting the router on a host or into the container, or you run one and need every setting it reads. This page
covers the release archive, the build from source, the service unit, the container, the `phoebis` subcommands, the
environment variables, every key of the bootstrap file with its default, its bounds and the line a refused value gets,
TLS on the two public listeners, the two database files and the limits compiled into the binary.

## Download a release

Every release carries a static binary for `x86_64` and `aarch64` Linux, which runs on any Linux of its architecture
with no shared library, glibc version or distribution in the way:

```sh
release=https://github.com/phoebis-project/phoebis/releases/download/<version>
curl -LO "$release/phoebis-<version>-x86_64-unknown-linux-musl.tar.gz"
curl -LO "$release/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf phoebis-<version>-x86_64-unknown-linux-musl.tar.gz
sudo install -m 0755 phoebis-<version>-x86_64-unknown-linux-musl/phoebis /usr/local/bin/phoebis
```

The archive is `phoebis-<version>-<target>.tar.gz` for the two targets `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl`, and it holds `phoebis-<version>-<target>/phoebis` and `LICENSE`. `SHA256SUMS` is the one
checksum file and lists both archives, so `--ignore-missing` checks the one you downloaded.

An operator who builds from source follows [Build](#build) instead.

## Build

```sh
cargo build --release
```

The binary lands at `target/release/phoebis`. The build needs a Unix host, Linux or macOS (untested): the store's lock
and permission checks, the backup and the bootstrap file's loader use `std::os::unix::fs`, and the service's stop signal
is the Unix `SIGTERM`, so the router does not build on Windows. A Windows machine runs it on a Linux host or in a Linux
runtime instead and reaches it as [A second machine](second-machine.md) describes. The toolchain is pinned in
`rust-toolchain.toml` to 1.96.1, and BoringSSL builds from source, which needs cmake, clang and libclang. The embedded
console needs Node 26 or later, and npm on the path: the build script checks `node --version` against that range, then
runs `npm ci` and `npm run build` in `web/` whenever the bundle is missing or older than its sources.
`PHOEBIS_SKIP_UI_BUILD=1` skips the Node step and embeds `web/dist` as it is, and a build with no bundle on disk embeds
a placeholder page, which the admin listener then serves in place of the console.

## Install as a service

```sh title="Install the binary, the unit, and the bootstrap files"
sudo install -m 0755 target/release/phoebis /usr/local/bin/phoebis
sudo useradd --system --home /var/lib/phoebis --shell /usr/sbin/nologin phoebis
sudo install -d -m 0755 /etc/phoebis
sudo install -m 0644 deploy/phoebis.service /etc/systemd/system/phoebis.service
sudo phoebis init --config /etc/phoebis/config.yaml --data-dir /var/lib/phoebis \
  --env-file /etc/phoebis/phoebis.env --owner phoebis
sudo systemctl daemon-reload
```

`init` creates the key file at `0600` in one open, so no moment exists in which the key is readable by anyone else, and
the environment file at `0600` with the secret it generated. `--owner phoebis` hands the key file and the data
directory to that user, and the bootstrap file and the environment file stay root's, since systemd reads the
environment file as root. A proxy listener that serves the LAN takes `--proxy-listen` on the `init` line, as
[The proxy listener on the LAN](second-machine.md#the-proxy-listener-on-the-lan) says.

`deploy/phoebis.service` runs the process as the `phoebis` user from `/var/lib/phoebis` with
`EnvironmentFile=/etc/phoebis/phoebis.env`, which systemd reads and hands over, so the process never opens the file.
`TimeoutStopSec=40` covers the 35-second shutdown budget, and `ProtectSystem=strict` with `StateDirectory=phoebis` and
`ProtectHome=true` is the hardening around it. `LimitNOFILE=65536` raises the descriptor limit above systemd's default
soft limit of 1,024 (`DefaultLimitNOFILE=` in `systemd-system.conf(5)`): the proxy listener and the feature listener
admit 4,096 connections each, and each connection can hold an upstream connection, so both listeners at their bound hold
16,384 descriptors ahead of the admin listener, the metrics listener and the store's files. `StateDirectoryMode=0700`
has systemd create `/var/lib/phoebis` for the `phoebis` user alone, the mode `check-config` expects of the data
directory. The file `init` wrote names `/etc/phoebis/api_prices.yaml` as `stats.api_prices_path` and
`/etc/phoebis/subscription_costs.yaml` as `stats.subscription_costs_path`. Leave both files uninstalled: the shipped
files carry every revision and a release adds the next, and a copy at either path would shadow every later release's
revisions until it was copied again.

A router that already runs from a file in your home directory moves under the unit after every line of the install block
but `init`, which refuses every file that exists: stop the router, create `/var/lib/phoebis` at `0700`
(`useradd --system` creates no home directory), move the bootstrap file to `/etc/phoebis/config.yaml`, the key file to
`/etc/phoebis/storage.key`, the two price files to `/etc/phoebis/` (`api_prices.yaml` and `subscription_costs.yaml`
beside the bootstrap file, where `init` pointed their path keys, each where it exists, in place of the shipped
revisions) and the data directory's contents into `/var/lib/phoebis`, give the key file, that directory
and everything moved into it to `phoebis` (`chown phoebis:phoebis /etc/phoebis/storage.key` and
`chown -R phoebis:phoebis /var/lib/phoebis`, since a moved file keeps its owner), and edit the five path keys `init`
wrote absolute to those places, since `ProtectHome=true` keeps the service out of `/home`:
`storage.encryption_key_file: /etc/phoebis/storage.key`, `storage.path: /var/lib/phoebis/router.sqlite`,
`stats.ledger_path: /var/lib/phoebis/ledger.sqlite`, `stats.api_prices_path: /etc/phoebis/api_prices.yaml` and
`stats.subscription_costs_path: /etc/phoebis/subscription_costs.yaml`. The registry opens under the key that sealed it
wherever the file names it, and the environment file is `deploy/phoebis.env.example` with the secret you export today in
place of its placeholder, at `0600` owned by root.

Then validate the file before enabling the service:

```sh
sudo bash -c 'set -a; . /etc/phoebis/phoebis.env; set +a; exec phoebis check-config --config /etc/phoebis/config.yaml'
```

With both price files left uninstalled, `check-config` names the shipped revisions on its two table lines, and its
`account tiers` count names the tiers the base rows size, leaving out a tier only a regime sizes:

```text
api prices: built in (3 revisions, in force 2026-11-09T18:30:00Z, 12 models), since /etc/phoebis/api_prices.yaml was not found
subscription costs: built in (1 revision, in force 2026-10-05, 11 weighted models, 3 account tiers, 1 regime: interactive), since /etc/phoebis/subscription_costs.yaml was not found
```

While a file holds an entry that takes effect after the one in force, its line adds `next {date}` after the entry in
force, the `effective` of the first such entry as its file wrote it. Every price read from the instant it takes effect
on, 00:00 UTC of a date or the second of an instant, takes the entry, and the burn rates the router builds at start
take it at the first start after that instant. The start's `ledger opened` log line names the same for the price
table: `pricing`, the entry in force, `pricing_revisions`, the count, and `pricing_next`, the held entry, recorded only
while an entry is held.

`check-config` reads `admin.secret_env` from the process environment when it is set and prints `warning: environment
variable PHOEBIS_ADMIN_SECRET is unset or empty` when it is not, so sourcing the environment file is optional for it.
The service, `status` and `tls reload` refuse an unset secret, and the start, `check-config`, `status` and `tls reload`
refuse a secret of another shape with the line [Environment variables](#environment-variables) gives. `phoebis health`
loads the file without resolving its secrets and refuses none. Once the check is clean, the owner warnings for what
`phoebis` owns aside, since root runs the check, `sudo systemctl enable --now phoebis`.

The block below is the shipped `deploy/phoebis.service`, quoted whole:

```ini title="deploy/phoebis.service"
[Unit]
Description=Phoebis Router
Documentation=https://phoebis-project.github.io/phoebis-docs/router/operations/
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=phoebis
Group=phoebis
WorkingDirectory=/var/lib/phoebis
EnvironmentFile=/etc/phoebis/phoebis.env
ExecStart=/usr/local/bin/phoebis --config /etc/phoebis/config.yaml
# The router's shutdown budget is 35 seconds after SIGTERM, and the stop timeout leaves five more.
KillSignal=SIGTERM
TimeoutStopSec=40
Restart=on-failure
RestartSec=5
# Two listeners of 4,096 connections, each with its upstream connection, hold up to 16,384 descriptors.
LimitNOFILE=65536
NoNewPrivileges=true
PrivateTmp=true
ProtectHome=true
ProtectSystem=strict
StateDirectory=phoebis
StateDirectoryMode=0700
UMask=0077

[Install]
WantedBy=multi-user.target
```

## Run the container

Every release publishes `ghcr.io/phoebis-project/phoebis:<version>` for `linux/amd64` and `linux/arm64`. The image holds
the binary alone: no shell, no configuration, no key, and no price table (the shipped one is compiled in). It runs as
uid 65532, reads `/etc/phoebis/config.yaml` (`PHOEBIS_CONFIG` is set in the image, so `phoebis` needs no flag), keeps
both databases under `/var/lib/phoebis`, and probes its own proxy listener with `phoebis health` every 30 seconds,
which Docker reports as the container's health.

`deploy/compose.yaml` and `deploy/config.container.yaml` are the example: the three listeners bind every interface
inside the container, the proxy port is published on every host interface, and the admin and metrics ports on the
host's loopback only. In an empty directory:

```sh title="Set up the compose directory"
install -m 0644 deploy/compose.yaml compose.yaml
phoebis init --container
sudo chown 65532:65532 storage.key && sudo chmod 0400 storage.key
```

`init --container` writes three files beside `compose.yaml`: `config.yaml` with the container's paths and both listeners
on every interface over plain HTTP (no `tls` key), `storage.key`, and `phoebis.env` with a generated admin secret.
`storage.key` and `phoebis.env` hold secrets and stay out of version control. Without the binary on the host, the image
runs the same command against the directory:

```sh title="Write the compose directory's files from the image"
docker run --rm -it -u "$(id -u)" -v "$PWD:/work" ghcr.io/phoebis-project/phoebis:<version> \
  init --container --config /work/config.yaml
```

The key file is owned by uid 65532 at mode `0400` because Compose mounts a file secret with the host file's owner and
mode, and that uid is the one the process runs as. Set `image:` in `compose.yaml` to the version you want, then:

```sh title="Start, watch, and stop"
docker compose up -d
docker compose ps            # "healthy" once phoebis health reads the router as ready
docker compose logs -f phoebis
docker compose down          # SIGTERM, then up to 40 seconds for the 35-second shutdown budget
```

The health check runs `phoebis health`, which loads and validates the configuration file and then asks the proxy
listener whether the router is ready: a file it cannot read, a value it refuses, or a router loop, ledger consumer or
store that died therefore reads as `unhealthy` rather than as a passing check, with the reason in `docker inspect`.
`health` speaks TLS to a proxy listener that terminates it. A ledger writer that stops draining fills its queue, and
once the queue is half full the router refuses new requests with its own `503 ledger_behind` rather than accepting
requests it cannot record ([the refusal codes](client.md#refusal-codes)).

The console answers at `http://localhost:8081`: the example publishes the admin port on the host's loopback alone, so it
is reached from the host's own browser or through an SSH tunnel whose local end is loopback, and nobody else.
Publishing the port on a LAN address is one edit to the `ports` line (`"8081:8081"`), and then the admin secret at login
and the session cookie on every call cross that LAN in the clear, since `config.yaml` names no `tls` pair for the admin
listener, which is the same trust decision the proxy port's publication already makes ([admin](#admin) holds the rule
for what a plain listener admits). For TLS, mount a certificate pair read-only beside the configuration and name the
mounted paths under `tls`, the certificate world-readable and the key at `0400` owned by uid 65532
([TLS on the listeners](#tls-on-the-listeners)), and reload a replaced pair with
`docker compose exec phoebis phoebis tls reload`.

The named volume `phoebis-data` takes the image's ownership and mode of `/var/lib/phoebis` on first use, uid 65532 at
`0700`, and a bind mount in its place needs `chown 65532` and `chmod 0700` on the host directory first. A volume or a
bind mount from an earlier image keeps the mode it has, and the router warns at each start until it is set to `0700`,
from the host or from a one-off container that mounts it. A price table dropped at `/var/lib/phoebis/api_prices.yaml` on
the volume is read at the next start, as `stats.api_prices_path` in the container configuration names it, and a
subscription cost model dropped at `/var/lib/phoebis/subscription_costs.yaml` the same. Upgrading is on
[Operations](operations.md#upgrades).

## Commands

| Command | Effect |
| --- | --- |
| `phoebis --config ./config.yaml` | Serves the proxy, the admin and the metrics listener, and with a `features` block the feature listener, until Ctrl-C or SIGTERM, then stops within 35 seconds. |
| `phoebis init --config ./config.yaml` | Writes the bootstrap file, the storage key and the admin secret, once. |
| `phoebis recover-key --config ./config.yaml` | Discards the registry's secrets after a lost key and seals a new canary, or seals the canary of a registry that has none when the key opens its rows: exit 0 once the accounts are marked or the canary is sealed ([The storage key](operations.md#the-storage-key)). |
| `phoebis check-config --config ./config.yaml` | Validates the file, key file, canary, schemas, certificate pairs and, with a `features` block, the feature CA's two files. |
| `phoebis health --config ./config.yaml` | Probes the proxy listener's readiness: exit 0 when ready, 1 otherwise. |
| `phoebis status --config ./config.yaml` | Prints readiness, pools, accounts and live pins: exit 0 when ready. |
| `phoebis backup --to <dir>` | Copies both databases into `<dir>` with a manifest: exit 0 once it is written. |
| `phoebis tls reload --config ./config.yaml` | Re-reads every listener's certificate pair: exit 0 once each reloaded. |
| `phoebis --version` | Prints the version. |

`--config` defaults to `./config.yaml`. When the flag is absent and `PHOEBIS_CONFIG` is set, every subcommand reads
that path instead, and the flag wins when both are given.

`check-config` takes no lock, runs no migration and opens no socket. It reads the key file, the registry's canary, the
registry's and the ledger's schema versions, each listener's certificate pair and, with a `features` block, the feature
CA's two files read-only, so it is safe to run against a production file while the router is up. It prints, in order:

<!-- check-config: begin -->
1. `{config path} is valid`
2. `proxy listens on {config.proxy.listen} over plain HTTP`, or `proxy listens on {config.proxy.listen} over TLS
   (certificate {cert_file}, expires {date})` for a certificate pair ([TLS on the listeners](#tls-on-the-listeners))
3. the admin line in the same two shapes, with `admin` and `config.admin.listen`
4. `features listener is off` without a `features` block, `features listens on {config.features.listen} over TLS (CA
   {path}, fingerprint {hex}, expires {date})` for the feature CA in the data directory, or `features listens on
   {config.features.listen} over TLS, and the first start creates the CA at {path}` where neither of its files stands
   yet ([features](#features))
5. `metrics listens on {config.stats.metrics_listen}`
6. `proxy: body budget {n} bytes, body read {n}s, key body reads {n}, max request {n} bytes, sign cch placeholder
   {on|off}, upstream gzip {auto|on|off}`
7. `profile {the build's profile}`
8. `routing: cooldown_fallback {n}s, model_rejection {n}s, catalog refresh {n}s, startup fetch {n}s`
9. `ledger at {config.stats.ledger_path}`
10. `api prices: {path} (file, {n} revisions, in force {date}, {n} models)` when `stats.api_prices_path` names a file
    that exists, `api prices: built in ({n} revisions, in force {date}, {n} models), since {path} was not found` when
    it names a file that is missing, or `api prices: built in ({n} revisions, in force {date}, {n} models)` when the
    key is set to `null`, with `revision` for a list of one entry and `, next {date}` after the entry in force while an
    entry is held
11. `subscription costs: {path} (file, {n} revisions, in force {date}, {n} weighted models, {n} account tiers, {n}
    regimes: {names})`, in the same three shapes, from `stats.subscription_costs_path`, where the regime clause reads
    `, no regime` for a model without the map and `, 1 regime: {name}` for one, and a file whose newest entry takes
    effect before the built-in model's newest adds `, the built-in model has a newer revision {date}`
12. `retention {n days|forever}, rollups {n days|forever}`
13. `sessions: {n} sessions, {n} agents`
14. `keepalive: {n} MiB retained`
15. the registry line, `registry at {config.storage.path} does not exist yet, and the first start creates it` when the
    registry is not there yet, or `registry at {config.storage.path} has no key canary yet. A sealed row opens under
    the key, and the next start seals one` for a registry from before the canary
16. `warning: environment variable {name} is unset or empty`, when the secret variable is unset
<!-- check-config: end -->

Further warning lines name a storage key file, data directory, database, TLS key file or, with a `features` block, the
feature CA's key file whose mode or owner is wider than the router expects, and a certificate that expires within 30
days (`warning: proxy.tls.cert_file {path} expires in {n} days ({date})`). The router starts either way and logs the
same warnings. A TLS key owned by root and readable by its group alone, such as `0640 root:phoebis`, earns no warning
from a process that runs as root or holds that group.

A registry or a ledger whose schema is newer than the binary reads fails the check with exit 1 and the start's own
refusal under `reading the registry at {config.storage.path}`: `the registry's schema version {n} is newer than the {m}
this build opens: restore the backup taken before the upgrade, or recreate a registry a pre-release build wrote with
phoebis init`. A ledger fails the same way under `reading the ledger at {config.stats.ledger_path}`, with `ledger` in
place of `registry`.

A secret variable set to a value the start refuses fails the check with exit 1 and the start's line: `environment
variable {name} holds a secret shorter than 16 characters`, `environment variable {name} still holds the example
placeholder`, or `environment variable {name} holds a secret outside the allowed shape (printable ASCII, no surrounding
spaces)`.

An unreadable, mismatched, expired or not-yet-valid certificate pair fails the check with exit 1 and the line the start
prints, naming the key and the path ([TLS on the listeners](#tls-on-the-listeners)).

On shutdown, Ctrl-C or SIGTERM, the router stops under one budget of 35 seconds, which its phases share in the order
[Upgrades and renewals](operations.md#upgrades) gives, with up to 30 of them for the drain of in-flight requests. A
credential refresh still in flight is waited for up to its own 30-second timeout or what is left of the budget,
whichever is shorter, so a token the upstream already rotated is stored rather than lost to the next start's quarantine.

`health` loads the file without resolving its secrets, so it needs the file and not the secret's environment. It
connects to `proxy.listen`, or to the loopback address of the same family when the listener is bound on every interface,
sends `HEAD /api/ready`, over TLS when `proxy.tls` terminates it, without checking the certificate (the probe dials its
own process and asks whether `/api/ready` answers, not who answers), and prints `proxy answers on {addr}` on a `200` or
one line saying why not (`proxy on {addr} answered 503, not 200` when the router loop, the ledger consumer or the store
is dead). The whole probe, connect included, is bounded by three seconds. `/api/hello` stays the liveness answer the
client itself probes. The container image runs `health` as its health check every 30 seconds
([the container](#run-the-container)).

`init` writes a first installation and refuses to touch anything that exists. It writes, in this order, the storage key
file (64 hex characters and a newline, created at `0600` in one open), the environment file when `--env-file` names one
(`0600`, `{name}={secret}` and `RUST_LOG=info`), and the bootstrap file at `--config` (`0644`). It creates the data
directory and every missing parent of those paths at `0700`. It opens no database, so the first start creates both and
seals the key canary. The bootstrap file is [the example](#the-bootstrap-file) with the two listeners,
`admin.secret_env`, the key file, both databases under the data directory, `stats.api_prices_path` (`api_prices.yaml`
beside the bootstrap file) and `stats.subscription_costs_path` (`subscription_costs.yaml` beside it) filled in, the
`features` block in place of its two commented lines where `--features-listen` gives an address, every path absolute
and every other line as the example spells it. The admin secret is 32 random bytes in base64url, 43 characters, and
never lands in the bootstrap file. `init` loads the written file the way `check-config` does before it answers.

| Flag | Default | Effect |
| --- | --- | --- |
| `--admin-cert PATH`, `--admin-key PATH` | unset | An operator's pair as `admin.tls`, both paths made absolute. |
| `--admin-listen ADDR` | `127.0.0.1:8081` | The admin listener's address. |
| `--container` | off | Writes the compose directory's shape, below. |
| `--data-dir PATH` | `data` beside the file | Holds `router.sqlite` and `ledger.sqlite`. |
| `--env-file PATH` | unset | Writes the admin secret there rather than printing it. |
| `--features-listen ADDR` | unset | The feature listener's address, written as the `features` block ([features](#features)). `init` asks no question for it. |
| `--key-file PATH` | `storage.key` beside the file | The storage key file. |
| `--non-interactive` | off | Asks nothing, even on a terminal. |
| `--owner USER` | unset | Gives that user the key file and each directory `init` created for the service, which needs root. |
| `--proxy-cert PATH`, `--proxy-key PATH` | unset | An operator's pair as `proxy.tls`, both paths made absolute. |
| `--proxy-listen ADDR` | `127.0.0.1:8317` | The proxy listener's address. |
| `--secret-env NAME` | `PHOEBIS_ADMIN_SECRET` | The admin secret's variable, written as `admin.secret_env`. |

`init` reads neither file of a pair, which `check-config` and the start check. A pair on a loopback address is written
as given, and a listener with no pair gets no `tls` line on any address. `--owner` hands over the key file, the data
directory when `init` created it, and every parent `init` created on the way to the key file, the data directory or the
bootstrap file, all to the user and its primary group, so the service can traverse them. A directory that existed keeps
its owner. The bootstrap file, the environment file and a parent created only for the environment file stay the
caller's, since systemd reads the environment file as root.

A pair is the operator's own, kept at the modes [TLS on the listeners](#tls-on-the-listeners) gives, and its leaf names
every host name and address the clients dial.

Without `--env-file`, stdout carries one line, `PHOEBIS_ADMIN_SECRET={secret}` under the default name, and the secret is
printed nowhere else, so `(umask 077; phoebis init --non-interactive > phoebis.env)` writes a usable environment file
that only its owner reads. A bare redirect under the usual umask leaves the secret readable by others. `--env-file`
writes that line beside `RUST_LOG=info` to the file it names, at `0600`. Everything else goes to stderr. On a terminal
(stdin and stderr both) and without `--non-interactive`, `init` asks, once the refusals below have found nothing, for
what the flags did not give, in this order: the proxy address, the admin address, and for each that leaves loopback with
no pair, `files` and then the two paths, or `none` for plain HTTP, the default. An empty answer takes the default shown
in brackets, and an address that does not parse is asked again. Then it prints one line per path written with its mode
and owner, where the secret went, and the next command, `phoebis check-config --config {path}`.

`init` exits 1 with one `Error:` line and nothing written when the bootstrap file, the key file or the environment file
already exists, when a `router.sqlite` or `ledger.sqlite` already sits in the data directory
(`{path} exists and a new key cannot open it, so keep the key that sealed it, or name an empty --data-dir`), when input
ends before a question is answered, and when `--owner` runs off root or names no user. A directory or a file it cannot
write exits 1 too, after removing what the run wrote, which the line names, and so does anything at the data
directory's path that is not a directory, with `checking {path}: {reason}`. The check reads through a link, so a link to
a directory is used as the data directory and a dangling link refuses. A usage error, such as `--proxy-cert` without
`--proxy-key` or an address that does not parse, exits 2.

Under `--container` the bootstrap file's directory is the compose directory: `init` writes `storage.key` and
`phoebis.env` beside the file, the container's paths inside it (`/run/secrets/router-storage-key`,
`/var/lib/phoebis/router.sqlite`, `/var/lib/phoebis/ledger.sqlite`, `/var/lib/phoebis/api_prices.yaml` and
`/var/lib/phoebis/subscription_costs.yaml`), both listeners on every interface with no `tls` line, and
`metrics_listen: 0.0.0.0:9464`. It asks nothing and creates no data directory, since `/var/lib/phoebis` is the volume
inside the container. The listen and TLS flags still apply, and `--data-dir`, `--env-file` and `--key-file` are usage
errors beside it. The summary ends with the steps that remain, the key file's owner and mode and the image tag in
`compose.yaml`, and with `docker compose run --rm phoebis check-config` as the next command, since the key's path in the
file is the container's.

`status` probes the proxy listener as `health` does, reads the admin API with each call under the `--timeout` deadline,
and prints one screen of readiness, pools, accounts and live pins, or one JSON object with `--json`:
[Is it up?](operations.md#is-it-up) shows the screen, reads it line by line and gives the exit codes. After a refused
secret or a throttled login the reads that follow print its line without calling again, so a run with a wrong secret
counts one failed login toward the throttle ([admin](#admin)).

The commands that call the admin API, `status` and `tls reload`, send the admin secret as `Authorization: Bearer`, one
connection per call, and share five flags:

| Flag | Default | Effect |
| --- | --- | --- |
| `--ca PATH` | the system roots | The CA bundle trusted in place of the system roots, for a pair a private CA signed. |
| `--json` | off | Prints the answer as JSON on stdout. |
| `--secret-env NAME` | `admin.secret_env` | The variable the admin secret is read from. Unset, the call is refused. |
| `--timeout SECONDS` | `10` | Each call's deadline, the connect and the TLS handshake included. |
| `--url URL` | from `admin.listen` and `admin.tls` | The admin listener's base, `http://` or `https://` and a host. |

`--url` defaults to the scheme `admin.tls` gives and the address of `admin.listen`, on the loopback of its family when
the listener is bound on every interface, as `health` reaches the proxy. With `--url` and `--secret-env` both given the
file is not read, which [Is it up?](operations.md#is-it-up) describes for a router on another machine, and without both,
the file at `--config` must load. Every account, pool and key is created and edited in the console.

`backup` copies the registry and the ledger into the directory `--to` names, each through SQLite's online backup API as
one consistent snapshot while the router serves or while it is stopped, writes `manifest.json` last and leaves the key
file out, and its flags, its lines and its refusals are on [Backup](operations.md#backup).

## Environment variables

The bootstrap file names the admin secret's environment variable and never holds a secret. `phoebis` resolves it at
startup and refuses to start when it is unset or empty, with `environment variable {name} is unset or empty` and nothing
else. A value shorter than 16 characters refuses with `environment variable {name} holds a secret shorter than 16
characters`, the example file's own placeholder refuses with `environment variable {name} still holds the example
placeholder`, and a value with a character outside printable ASCII or a leading or trailing space refuses with
`environment variable {name} holds a secret outside the allowed shape (printable ASCII, no surrounding spaces)`.

| Variable | Named by | Holds |
| --- | --- | --- |
| `PHOEBIS_CONFIG` | `phoebis` itself, when `--config` is absent | The path of the bootstrap file. |
| `PHOEBIS_ADMIN_SECRET` | `admin.secret_env` | The admin login, which reaches every admin route. |

Pick your own name: `PHOEBIS_ADMIN_SECRET` is the example file's, and `init --secret-env` writes another. `RUST_LOG`
sets the log filter and defaults to `info`. Nothing else is named here: an account's upstream credential and the keys
downstream clients present belong to the registry, not the bootstrap file.

`init --env-file` writes the file the service reads with a generated secret, and without it `init` prints the secret
once.

## The bootstrap file

```yaml title="deploy/config.example.yaml"
# Bootstrap settings only. The admin secret is named here and read from the environment at startup, so this
# file can be committed and read by anyone who may read the deployment.
admin:
  listen: 127.0.0.1:8081
  # tls: {cert_file: ..., key_file: ...} names your certificate pair and the listener terminates TLS.
  # Without tls the listener serves plain HTTP on the address named, loopback or not.
  secret_env: PHOEBIS_ADMIN_SECRET
# features:
#   listen: 0.0.0.0:8443  # the listener for the claude.ai features, always TLS under the router's own CA
keepalive:
  # Process-wide limits only. Whether a pool's sessions are probed, and for how long, are pool settings in
  # the registry. The largest retained recipe follows proxy.max_request_bytes, the relay's request body bound.
  max_retained_bytes: 268435456
proxy:
  listen: 127.0.0.1:8317
  # tls: {cert_file: ..., key_file: ...} names your certificate pair and the listener terminates TLS.
  # Without tls the listener serves plain HTTP on the address named, loopback or not.
  # The bounds on the request bodies the proxy holds, at their defaults. Uncomment a key to change it.
  # body_budget_bytes: 536870912
  # body_read_seconds: 60
  # key_body_reads: 128
  # max_request_bytes: 33554432
  # Signs a restored cch=00000 with this build's formula. Leave it off until a server rejects the placeholder.
  # sign_cch_placeholder: false
  # How the edited body leaves: auto mirrors the client, on always gzip, off always identity.
  # upstream_gzip: auto
routing:
  # How often every enabled account's model listing is read again.
  catalog_refresh_seconds: 21600
  # How long an account is cooled when the upstream rejected it without saying for how long, 1 to 3600.
  cooldown_fallback_seconds: 60
  # How long an account whose upstream answered 404 for a model is skipped for that model.
  model_rejection_seconds: 3600
  # How long the proxy listener waits at startup for the model listings and the usage snapshots.
  startup_fetch_seconds: 30
sessions:
  expire_after_seconds: 86400
  # The most agent records and the most sessions that hold a slot at once, read once at start.
  max_agents: 1024
  max_sessions: 128
  # The relay's memory of a session's previous request id and prompt id: entries held in process, how long one
  # may go unseen before it leaves memory, and how long its on-disk copy is kept.
  memory_cap: 65536
  memory_idle_seconds: 86400
  memory_retention_days: 30
  # How long a session no companion reports stays listed in the Sessions view past its newest pin use, its last request
  # or its companion's end. The lesser of the timeout and the session expiry applies.
  non_companion_idle_seconds: 3600
stats:
  # Overrides the shipped price table. A missing file is fine.
  api_prices_path: ./api_prices.yaml
  # The longest a key or pool may keep captured bodies, 1 to 168.
  body_capture_max_hours: 72
  ledger_path: ./data/ledger.sqlite
  # Prometheus text format at /metrics. Bind it where your scraper can reach it.
  metrics_listen: 127.0.0.1:9464
  # The requests with their attempts, the kept-warm marks, the events, the decisions, the quota and extra-usage
  # samples, and the registry's settled refresh attempts and change log are purged past this. 0 keeps them forever.
  retention_days: 365
  # The hourly rollups and the account rollups are purged past this. 0 keeps them forever, otherwise at least
  # retention_days.
  rollup_retention_days: 365
  # Overrides the shipped subscription cost model. A missing file is fine.
  subscription_costs_path: ./subscription_costs.yaml
storage:
  encryption_key_file: /run/secrets/router-storage-key
  path: ./data/router.sqlite
upstream:
  # Both unset by default: extra trust anchors for a lab upstream, and one address every host resolves to.
  # extra_ca_pem: ./lab-ca.pem
  # resolve_override: 127.0.0.1:8443
usage:
  # Optional: how many days before an unused limit reset ends the Dashboard and the Quota card warn, 0 for never.
  limit_reset_warning_days: 5
  # Optional: how many accounts' usage and profile fetches run at once.
  refresh_concurrency: 2
```

The quoted file is the text `init` fills in, and `init` rewrites every path for the shape it writes, so the container
key path above never reaches a host install. An unknown key anywhere refuses the file: every block denies unknown
fields, and the message is `the configuration is not valid YAML`, which a syntax error also gets, with the cause under
it. Each top-level block below lists its keys' defaults, what they accept, and the message a refused value gets.

### admin

Required.

Phoebis Router is built for one operator: one admin secret reaches every route, and the console is that person's own.
Every mutation through the admin API, a create, an edit, a delete or a login, is written to the log as one `info` line,
`admin mutation`, naming its method, route, status, entity, id, credential and session id, never its body or a secret.
The host's journal is the trail.

| Key | Default | Accepts |
| --- | --- | --- |
| `listen` | required | a socket address |
| `secret_env` | required | the variable holding the admin secret |
| `tls` | unset, plain HTTP | `{cert_file, key_file}` |

The `tls` key is optional on every address. Without it the listener serves plain HTTP and issues the cookie without
`Secure`, which costs nothing behind an SSH tunnel and sends the admin secret at login and the session cookie on every
call in the clear on a network address. A certificate pair terminates TLS on the listener on any address and the cookie
carries `Secure`. A front that terminates TLS forwards to a loopback listener over plain HTTP and passes `Host` through
unchanged beside `X-Forwarded-Proto: https`, and the cookie is then issued with `Secure` as well.

The Origin check is the cookie's. A browser attaches `Origin` to every cross-site write, so on a mutation that presents
a cookie or no credential, a presented `Origin` must name the request's own `Host`, with the `https://` scheme where
the listener terminates TLS or a front's `X-Forwarded-Proto: https` says the browser's leg does. One that does not is
refused with `403 origin is not allowed`. A request with no `Origin` (curl, a subcommand, the health probe) is admitted,
and a request that presents a bearer admin secret skips the check, since a browser cannot attach `Authorization` to a
cross-site request without a preflight the admin API never answers. The login is checked too, as the one mutation an
unauthenticated caller can reach, and it ignores a bearer.

A wrong secret answers `401 secret does not match`. Five failed logins within a minute refuse every login after them
with `429 too many failed logins, try again in a minute` until the minute passes. A session ends after 15 minutes idle
or 12 hours after it was opened, whichever comes first. A login with "Stay logged in" keeps its session for 30 days from
its last read, across a restart, until a secret rotation or a logout of a kept login, which ends every kept login.

Refused with:

- `listen`: a YAML error
- `secret_env`: `environment variable {name} is unset or empty`, and the lines for a value shorter than 16
  characters, the example file's placeholder or another shape ([environment variables](#environment-variables))
- `tls`: `admin.tls: must be a mapping with cert_file and key_file` under `the configuration is not valid YAML` for a
  word in the key's place, the key with nothing beneath it, a third key, a repeated key, or a mapping that lacks one
  of the two ([TLS on the listeners](#tls-on-the-listeners))

A file from an earlier build can still carry `admin.origin`, `admin.plain_http` or `usage.cache_ttl_seconds`, and the
loader refuses each at start as an unknown key.

### features

Optional, and off without the block.

| Key | Default | Accepts |
| --- | --- | --- |
| `listen` | required in the block | a socket address |

With the block the router binds a fourth listener at `listen`, beside the proxy, the admin and the metrics listener,
which serves the calls of the [claude.ai features](claude-ai-features.md) and nothing else. It always terminates TLS
under the router's own CA and takes no `tls` key ([TLS on the listeners](#tls-on-the-listeners)). These pages give it
port 8443, as in `listen: 0.0.0.0:8443`.

With the block the router also holds a CA of its own for the claude.ai features: two files in the data directory, the
directory of `storage.path`, `feature-ca.pem` for the certificate and `feature-ca-key.pem` for its private key, both
written once at `0600` by the first start that finds neither. The CA carries a critical name constraint that permits
`api.anthropic.com` and `mcp-proxy.anthropic.com` with the names beneath them and excludes each IP address, so a client
that trusts it accepts its certificates for those two hosts and for no other name. The router never replaces it. A new
CA comes from removing both files before a start, and it has a new fingerprint.

The proxy listener serves the CA's certificate at `GET /v0/features/ca.pem` without a key
([What the proxy accepts](client.md#what-the-proxy-accepts)), and on a router without the block that path answers the
unknown route's `404`. The fingerprint is the SHA-256 of the bytes the download serves in lowercase hex, which
`sha256sum` (or `shasum -a 256` on macOS) prints for the downloaded file. `check-config` prints it in its `features`
line, and the first start logs it with the path in its `feature CA created` line.

Refused with:

- `features`: `features: must be a mapping with listen` under `the configuration is not valid YAML` for the key with
  nothing beneath it or a word in its place
- `listen`: a YAML error

### keepalive

Defaults whole. This is a process-wide limit read once at start, so a change takes a restart, and the per-pool tuning is
on the [Keepalive](keepalive.md) page. The bound refuses with `keepalive.max_retained_bytes must be between {min} and
{max}, not {value}`.

| Key | Default | Accepts |
| --- | --- | --- |
| `max_retained_bytes` | `268435456` | `proxy.max_request_bytes` (the body bound a recipe follows) to 536870912 |

Four probes run at once, and that count is not a key.

### proxy

Required: `listen`, a socket address, refused with a YAML error. `tls` names what the listener serves
([TLS on the listeners](#tls-on-the-listeners)). Four keys bound the request bodies the proxy holds,
`sign_cch_placeholder` decides whether a restored billing placeholder is signed, and `upstream_gzip` how the edited body
leaves toward the upstream. All six are tunable in the file for power users and have no console control.

| Key | Default | Accepts |
| --- | --- | --- |
| `body_budget_bytes` | `536870912` | `max_request_bytes` to 2^40 |
| `body_read_seconds` | `60` | 1 to 599 |
| `key_body_reads` | `128` | 1 to 4096 |
| `max_request_bytes` | `33554432` | 1024 to 33554432 |
| `sign_cch_placeholder` | `false` | `true` or `false` |
| `tls` | unset, plain HTTP | `{cert_file, key_file}` |
| `upstream_gzip` | `auto` | `auto`, `on` or `off` |

Refused with:

- `body_budget_bytes`: `proxy.body_budget_bytes must be at least proxy.max_request_bytes ({minimum}), not {value}`,
  and `proxy.body_budget_bytes must be at most 1099511627776, not {value}` past 2^40
- `body_read_seconds`: `proxy.body_read_seconds must be between 1 and 599, not {value}`
- `key_body_reads`: `proxy.key_body_reads must be between 1 and 4096, not {value}`
- `max_request_bytes`: `proxy.max_request_bytes must be between 1024 and 33554432, not {value}`
- `sign_cch_placeholder`: a YAML error for anything but a boolean
- `tls`: `proxy.tls: must be a mapping with cert_file and key_file` under `the configuration is not valid YAML` for a
  word in the key's place, the key with nothing beneath it, a third key, a repeated key, or a mapping that lacks one
  of the two ([TLS on the listeners](#tls-on-the-listeners))
- `upstream_gzip`: `proxy.upstream_gzip: must be auto, on or off, not {value}` under
  `the configuration is not valid YAML`

`max_request_bytes` is the per-body bound, the Messages API's own request limit, so the router never refuses what the
upstream accepts, and a value above it is refused as pointless. `body_budget_bytes` is the bytes of bodies the whole
process holds at once, with each body's parsed tree while the relay holds it, sized for a household whose keys carry
agent tools that fan out (about 500 one-MiB bodies, or fifteen at the per-body bound, since each body needs room for its
tree while the relay parses it), and a request that would pass it beside the bodies in flight is refused with the
router's own `503 body_budget_exhausted`. A body whose bytes and parsed tree are above the whole budget can never fit,
so it is refused for good with `413 body_over_budget`. A body at the per-body bound with 1,000,000 elements needs
396,554,432 bytes, a little over 378 MiB, so a budget set lower refuses such a body for good. `key_body_reads` bounds
the bodies one key has arriving at once, and a request past it is refused `429 key_body_reads` with `Retry-After: 1`.
`body_read_seconds` is how long a body may take to arrive after authentication, under the stream's idle bound of 600
seconds, and a body that takes longer is answered `408 request_timeout`.

`sign_cch_placeholder` signs a restored `cch=00000` with this build's formula instead of sending the placeholder, off
until a server rejection of the placeholder proves it necessary.

The router accepts a request body sent with `content-encoding: gzip` and inflates it before reading it, so the inflated
size is what the per-body bound and `body_budget_bytes` measure. Every other request encoding is refused with
`415 unsupported_content_encoding`. `upstream_gzip` says how the edited body leaves toward the upstream: `auto` sends
gzip when the client sent gzip and identity otherwise, `on` always gzip, `off` always identity, and the
`content-encoding` and `content-length` toward the upstream follow the bytes sent.

### routing

Defaults whole.

| Key | Default | Accepts |
| --- | --- | --- |
| `catalog_refresh_seconds` | `21600` | 60 to 604800 |
| `cooldown_fallback_seconds` | `60` | 1 to 3600 |
| `model_rejection_seconds` | `3600` | 1 to 604800 |
| `startup_fetch_seconds` | `30` | 1 to 60 |

Refused with:

- `catalog_refresh_seconds`: `routing.catalog_refresh_seconds must be between 60 and 604800, not {value}`
- `cooldown_fallback_seconds`: `routing.cooldown_fallback_seconds must be between 1 and 3600, not {value}`
- `model_rejection_seconds`: `routing.model_rejection_seconds must be between 1 and 604800, not {value}`
- `startup_fetch_seconds`: `routing.startup_fetch_seconds must be between 1 and 60, not {value}`

The block holds these four keys and nothing else: a key it does not list is refused at start and by `check-config` as an
unknown field, named in the error. How a cooldown and the failover use the timings is on
[Cooldowns and failover](routing.md#cooldowns-and-failover).

### sessions

Defaults whole. `expire_after_seconds` is how long a session may go without a request before it leaves the Sessions
view, taking its pin and any keepalive recipe with it, and `non_companion_idle_seconds` how much sooner a session no
companion reports leaves the view, past the latest of its newest pin use, its last request and, where its companion
ended the session, that end, its pin staying to the expiry. `expire_after_seconds` is also how long the registry keeps
the facts that a companion mod reports of the session itself (its branch, its paths, its repository and its title),
counted from the session's last report. The three `memory_` keys bound the relay's memory of a session's previous
request id and prompt id, the record behind `cc_prev_req` ([client](client.md#the-environment-block)).

`max_agents` and `max_sessions` are read once at start, so a change takes a restart. `max_sessions` counts the sessions
that hold a slot, from a session's first request until its end, so neither a registered session that has sent nothing
nor an ended one holds one, though an ended session's entry stays until the session expiry sweeps it. The mod's lease is
90 seconds, and it is not a key.

| Key | Default | Accepts |
| --- | --- | --- |
| `expire_after_seconds` | `86400` | at least 60 |
| `max_agents` | `1024` | 1 to 16384 |
| `max_sessions` | `128` | 1 to 4096 |
| `memory_cap` | `65536` | 1 to 1048576 entries held in process, and past it the least recently seen entry leaves memory |
| `memory_idle_seconds` | `86400` | 60 to 31536000, how long an entry may go unseen before it leaves memory |
| `memory_retention_days` | `30` | 1 to 3650, how long the on-disk copy of an entry is kept after its last request |
| `non_companion_idle_seconds` | `3600` | 60 to 31536000, how long a session no companion reports stays listed past its newest pin use, its last request or its companion's end, and how long a line's pin stays listed on such a row past its last use. The lesser of the timeout and `expire_after_seconds` applies. |

Refused with:

- `expire_after_seconds`: `sessions.expire_after_seconds must be at least 60`
- `max_agents`: `sessions.max_agents must be between 1 and 16384, not {value}`
- `max_sessions`: `sessions.max_sessions must be between 1 and 4096, not {value}`
- `memory_cap`: `sessions.memory_cap must be between 1 and 1048576, not {value}`
- `memory_idle_seconds`: `sessions.memory_idle_seconds must be between 60 and 31536000, not {value}`
- `memory_retention_days`: `sessions.memory_retention_days must be between 1 and 3650, not {value}`
- `non_companion_idle_seconds`: `sessions.non_companion_idle_seconds must be between 60 and 31536000, not {value}`

### stats

Defaults whole.

| Key | Default | Accepts |
| --- | --- | --- |
| `api_prices_path` | `./api_prices.yaml` | a path, and a missing file is fine |
| `body_capture_max_hours` | `72` | 1 to 168 |
| `ledger_path` | `./data/ledger.sqlite` | a file path with a parent directory, not the same file as `storage.path` |
| `metrics_listen` | `127.0.0.1:9464` | a socket address |
| `retention_days` | `365` | 0 for forever, else a number of days, which covers the decisions, the events, the feature calls, the kept-warm marks, the quota and extra-usage samples, the registry's settled refresh attempts and change log, and the requests with their attempts. It is also how long the registry keeps the facts that a companion mod reports of an agent (its description, its name and its type), counted from the last report of the agent's session. |
| `rollup_retention_days` | `365` | covers the hourly rollups and the account rollups: 0 for forever, else a number of days of at least `retention_days` |
| `subscription_costs_path` | `./subscription_costs.yaml` | a path, and a missing file is fine |

Refused with:

- `api_prices_path`: `loading the price table from {path}` with the rule under it, `pricing table: {rule}`, for a
  malformed file, a repeated key or a rule the table below gives, and with the read error under it for a file that is
  there and cannot be read
- `body_capture_max_hours`: `stats.body_capture_max_hours must be between 1 and 168, not {value}`
- `ledger_path`: `stats.ledger_path {path} must name a database file inside a directory`, and
  `stats.ledger_path and storage.path must name different files, not {path}` when the two paths match
- `metrics_listen`: a YAML error
- `retention_days`: nothing
- `rollup_retention_days`: `stats.rollup_retention_days must be 0 or at least stats.retention_days
  ({retention_days}), not {value}`
- `subscription_costs_path`: `loading the subscription cost model from {path}` with the rule under it,
  `plan table: {rule}`, in the same cases, and with the read error under it for a file that is there and cannot be read

The rules under either line, after `pricing table: ` for the price table and `plan table: ` for the subscription cost
model:

| Rule | File | Refused for |
| --- | --- | --- |
| `revisions: at least one entry` | both | an empty list |
| ``revisions[{i}].effective: a date in YYYY-MM-DD form or an instant in YYYY-MM-DDTHH:MM:SSZ form, not `{text}` `` | both | an entry's `effective` in neither form |
| `revisions: two entries in force from the same instant, {first} and {second}` | both | two entries on one instant, whichever form each wrote |
| ``unknown field `{key}`, expected `revisions` at line {n} column {n}`` | both | a top-level key other than `revisions` |
| `revision {date}: model {model}, cell {cell} must be a finite non-negative number` | price table | a cell of that entry that is negative, `NaN` or infinite |
| `revision {date}: model {model}, cell long_context.over must be a whole number of tokens above zero` | price table | a threshold of that entry at zero |
| `revision {date}: {key}: {rule}` | cost model | a key of that entry the cost model refuses, such as a weight at zero |
| `revision {date}: tier_order: a list of distinct tier names` | cost model | an empty `tier_order`, or one naming a tier twice |
| `revision {date}: model_tier.{model}: {tier} is not in tier_order` | cost model | a model tier `tier_order` does not list |
| `revision {date}: model_tier.{model}: a dated id, where the file keys undated ids` | cost model | a dated `model_tier` key |
| `revision {date}: model_tier: no tier for {model}` | cost model | a `model_weight` or `overrides` key `model_tier` does not name |

### storage

Required.

| Key | Accepts |
| --- | --- |
| `encryption_key_file` | a file holding 32 raw bytes or 64 hex characters, outside the database it unseals |
| `path` | a file path with a parent directory |

Refused with:

- `encryption_key_file`: checked when the store opens, not when the file loads, with `storage key file must hold
  exactly 32 raw bytes or 64 hex characters, with at most one trailing newline` for another shape and
  `the storage key does not match the registry at {path}` for a key the registry was not sealed under
- `path`: `storage.path {path} must name a database file inside a directory`

### upstream

Defaults whole and is rarely set.

| Key | Default | What it does |
| --- | --- | --- |
| `extra_ca_pem` | unset | Extra trust anchors for a lab upstream not signed by a public authority, every certificate of the file. The start refuses a file that holds none. |
| `resolve_override` | unset | Sends every host to this address instead of DNS. |

### usage

Defaults whole. The poll and the refresh read the endpoint under the account's hold and answer the reading they made.
`refresh_concurrency` is how many accounts' usage, profile and limit reset reads may run at once.
`limit_reset_warning_days` is how many days before an unused limit reset ends the Dashboard and the Quota card warn, and
0 turns the warning off.

| Key | Default | Accepts |
| --- | --- | --- |
| `limit_reset_warning_days` | `5` | 0 to 30, 0 for never |
| `refresh_concurrency` | `2` | 1 to 16 |

Refused with:

- `limit_reset_warning_days`: `usage.limit_reset_warning_days must be between 0 and 30, not {value}`
- `refresh_concurrency`: `usage.refresh_concurrency must be between 1 and 16, not {value}`

The checks above run in a fixed order at load, not block by block: `usage.refresh_concurrency`,
`usage.limit_reset_warning_days`, `storage.path`, the `proxy` bounds, `routing.model_rejection_seconds`,
`routing.cooldown_fallback_seconds`, the `keepalive` bound, `routing.startup_fetch_seconds`,
`routing.catalog_refresh_seconds`, the seven `sessions` keys, `stats.body_capture_max_hours`,
`stats.rollup_retention_days`, `stats.ledger_path`, and last whether `stats.ledger_path` and `storage.path` name the
same file. The first failure is the one reported, so fix the file top to bottom to find the next one.

`stats.ledger_path` and `storage.path` cannot name the same file because two schemas in one file open quietly and lose
the ledger's columns: the ledger's tables would open inside the registry file, read its schema version, skip every
ledger migration, and leave `requests` without the columns the writer inserts.

## TLS on the listeners

The proxy and the admin listener each take one `tls` key under their block, a mapping with `cert_file` and `key_file`
naming your certificate pair, and the listener then terminates TLS itself:

```yaml title="config.yaml"
proxy:
  listen: 0.0.0.0:8317
  tls:
    cert_file: /etc/phoebis/proxy.pem
    key_file: /etc/phoebis/proxy.key
```

Without the key a listener serves plain HTTP on the address named, loopback or not, and no address requires the key. A
plain listener on a network address sends in the clear what a certificate would cover: on the admin listener the admin
secret at login and the session cookie on every call, on the proxy the client's key on every request. That is your
trust decision for that network, and the file records it by naming no pair. Both listeners speak HTTP/1.1 only, and
the handshake steers a client that offers HTTP/2 to HTTP/1.1.

`cert_file` is PEM with the leaf first and any intermediates after it, and `key_file` is the matching private key in
PEM. Under the service unit, keep both under `/etc/phoebis`: the certificate at `0644 root:root`, the key at
`0640 root:phoebis` or `0600 phoebis:phoebis`. In the container, mount them read-only beside the configuration, with
the key at `0400` owned by uid 65532. A Tailscale certificate, a certificate from a LAN CA, or one an ACME client
renews all serve.

With `features.listen` set the router creates one CA for the two feature hosts and issues their certificates
([features](#features)), and it generates no certificate for its own name, which stays yours through a pair on a
listener or a front that terminates TLS. A certificate from a public CA needs nothing on the clients. A certificate
from a private CA (a LAN CA, a Tailscale certificate's issuer, a CA you run) needs the clients to trust that CA, through
`NODE_EXTRA_CA_CERTS=<path>` beside `ANTHROPIC_BASE_URL` in the client's
[environment block](client.md#the-environment-block). `phoebis status` and `phoebis tls reload` take the same file
through `--ca`.

A start with a `features` block reads the feature CA's two files as it reads a listener's pair and refuses what it
refuses there, with the lines the list below gives for a pair under `features.ca` in place of `proxy.tls`, such as
`features.ca.key_file {path}: does not match the certificate in {cert_file}`. It refuses one file without the other
too, with `features.ca: {path} stands without {path}: restore the missing file, or remove both for a new CA`, and
creates nothing, since a new CA written over half of an old one would change the fingerprint. Four more lines are the
CA's own. A certificate file that holds a second certificate gets
`features.ca.cert_file {path}: holds more than one certificate`, since the download serves the file whole and each
launcher trusts it whole. A certificate under another subject than the one the router gives its CA gets
`features.ca.cert_file {path}: holds no CA this router made`, since each certificate the router issued would name an
issuer the served CA does not carry, and so does a certificate that carries no key identifier, which each CA the router
makes carries. A certificate whose name constraint is not the one the router writes gets
`features.ca.cert_file {path}: holds a CA without the name constraint this router writes`, since a launcher is told
that the file binds its trust to the two feature hosts. A key the router cannot sign with gets
`features.ca.key_file {path}: holds a key that cannot sign: {error}`. A first start that cannot write one of the two
files refuses with `features.ca {path}: {OS error}`. `check-config` reads the two files the same way with the block
alone. A key file with a wider mode or another owner draws a warning from the start and from `check-config` and
refuses nothing.

The start reads the pair once, before it binds a port, and a renewal puts new files at both paths and asks the running
router to re-read every listener's pair with `phoebis tls reload`, as [Upgrades and renewals](operations.md#upgrades)
walks it. [Certificates from step-ca, renewed by acme.sh](operations.md#step-ca) is a worked example, from a CA of your
own to the renewal. The dashboard raises `certificate_expiring` and `certificate_reload_failed` for a listener's pair,
which [Conditions and gauges](statistics.md#conditions-and-gauges) describes.

`check-config` reads the pair the way the start does, and both refuse it with one line naming the key and the path:

- `proxy.tls.cert_file {path}: {OS error}` or `proxy.tls.key_file {path}: {OS error}` for a file it cannot read
- `proxy.tls.cert_file {path}: holds no PEM certificate`
- `proxy.tls.cert_file {path}: holds a PEM certificate block that does not parse`, the leaf's or an intermediate's
- `proxy.tls.key_file {path}: holds no PEM private key`
- `proxy.tls.key_file {path}: does not match the certificate in {cert_file}`
- `proxy.tls.cert_file {path}: expired on {date}`
- `proxy.tls.cert_file {path}: not valid before {date}`
- `proxy.tls: {error}` for a pair the TLS library refuses for another reason, with its own text

The admin listener's lines carry `admin.tls` in place of `proxy.tls`. The start logs `serving TLS` for the proxy and
the admin listener where each terminates TLS, with its leaf's names and `not_after`, and the `listening` line names
each listener's scheme.

The scheme in a caller's URL has to match the listener. A plain-HTTP request to a listener that terminates TLS ends at
the handshake with no HTTP answer, and the router logs it at debug only. An `https://` URL against a plain listener
fails in the client's own handshake, since the listener answers no TLS.

The metrics listener has no `tls` key and serves plain HTTP.

The feature listener has no `tls` key either and always terminates TLS, on certificates the router issues itself under
the feature CA ([features](#features)): one for `api.anthropic.com` and one for `mcp-proxy.anthropic.com`, each valid
for seven days, with a key that lives in memory alone. The router issues the next certificate of a name about a day
before the current one ends and serves it from the next connection on, so the listener needs no renewal step and no
`phoebis tls reload`, which leaves it out. The handshake's server name picks the certificate, and a handshake that names
another host or none ends without one, which the router logs at debug, as it logs each failed handshake. The listener
speaks HTTP/1.1 only, as the others do. Its start lines are `serving TLS` with `listener="features"`, the two names,
the CA's fingerprint and its end, and `features=https://{addr}` in the `listening` line.

### Port 443 for a DNS redirect

A client that reaches the feature listener through a DNS redirect, with both feature hosts resolving to the router,
connects to port 443 of that address. A client behind `phoebis-proxy` needs none of this, since the proxy's upstream
names the listener's own port. Neither does a client whose start on Linux takes a hosts file of its own, since a relay
on the client's machine passes port 443 of loopback to the listener's own port ([A relay on
loopback](claude-ai-features.md#a-relay-on-loopback)). The `phoebis` user of the service unit cannot bind a port below
1024, and the shipped unit stays as it is. One road is a drop-in for the unit that grants the capability, with
`features.listen` on port 443:

```ini title="A drop-in for the service unit"
# /etc/systemd/system/phoebis.service.d/features-port.conf
[Service]
AmbientCapabilities=CAP_NET_BIND_SERVICE
```

The other is a rule of the host's firewall that maps port 443 of that address to the listener's port, 8443 on these
pages. In the container the listener binds its port inside the container, and `deploy/compose.yaml` carries the line
that publishes it as port 443 of the host, `- "443:8443"`, commented out under its published ports.

Where another program holds port 443 of that address, it passes a connection through to the listener by its TLS name
without ending the TLS. The router alone holds the certificates for the two names, and a request's head has to reach
it in the client's spelling.

### phoebis-proxy

A client in the "Base URL and proxy" setup reaches the feature listener through `phoebis-proxy`, a tunnel proxy in a
repository of its own, [phoebis-project/phoebis-proxy](https://github.com/phoebis-project/phoebis-proxy), whose pages on
this site hold its [install](../proxy/install.md) and its [settings](../proxy/settings.md). Its image is
`ghcr.io/phoebis-project/phoebis-proxy`. `deploy/compose.yaml` carries the proxy as a commented service behind the
router's: the upstream `phoebis:8443`, which is the router's service name with the feature listener's port, an allow
list for the network the clients come from, and the proxy's port 8318, published. The proxy reaches the feature listener
inside the compose network, so the router's configuration holds the `features` block and the compose file publishes no
port of that listener. [Base URL and proxy](claude-ai-features.md#base-url-and-proxy) says where else the proxy runs and
what a client sets for it.

## Storage

`storage.path` names the database file itself rather than a directory, and `phoebis` uses the name as given. It creates
the parent directory at mode 0700 when it is missing, the database at mode 0600, and a `lock` file beside the database
that keeps a second process off the same data, refusing one with
`the data directory is locked by another process`.

`check-config` and `backup` take no lock. `recover-key` takes it, since it rewrites the registry, and refuses a
running router with `the registry is locked, stop the service first`.

`storage.encryption_key_file` points at a separate file holding 32 raw bytes or 64 hex characters. It seals every
credential in the database, so keep it outside any backup that carries the database, which is useless without it.
`phoebis backup` leaves the key out and says so.

The registry file also holds the relay's session memory: for each key, conversation (the client's session id), and
agent, the previous request id the relay restores into a Claude Code request's attribution line as `cc_prev_req`, and
the prompt id the client's header last carried. The in-process copy is the one the relay reads. Changes reach the file
every five seconds, at shutdown, and as soon as the records evicted from memory before they were written reach
`sessions.memory_cap` (that overlay is bounded by the cap too, and a record it sheds because a write hung is counted in
the log). A session missing from memory after a restart or past `sessions.memory_cap` is read back from the file on its
next request, and rows older than `sessions.memory_retention_days` are dropped once an hour ([sessions](#sessions)). A
request takes its revision fence from the router's in-memory snapshot and its credential from a cache of each account's
current credential. The cache reads the registry file again when an account is paired, a credential rotates, or the
registry reloads, and a credential the cache serves never waits behind the session memory's writes.

`stats.ledger_path` names a second SQLite file, the statistics ledger, under the same rules: the name is used as given,
the parent directory is created at mode 0700 and the file at 0600. It runs in WAL mode with `synchronous = NORMAL`, so
a power loss may lose the last transactions before a checkpoint. A request's facts are never dropped: a request record
waits for room in the ledger's channel, so a ledger writer that stalls holds the request path rather than losing
request facts. Only captured bodies stay best-effort, since a stream may outrun its recorder, and a `ledger_gap` event
records what was lost. Captured bodies live in this file, which makes it as sensitive as the registry even though it
holds no credential, so give it the same care.

`phoebis backup` takes the snapshot, and [Backup](operations.md#backup) says how and how to restore it.

## Compiled-in limits

Rows in alphabetical order of the limit's name. None of these has a bootstrap key.

| Limit | Value |
| --- | --- |
| Admin login throttle | Five failed logins per minute, then `too many failed logins, try again in a minute`. |
| Admin session lifetime | 15 minutes idle, 12 hours absolute. With "Stay logged in", 30 days from the last read, across a restart, until a secret rotation or a kept logout. |
| Body elements | 1,000,000 elements in a request body, counted before the parse. Above it the body is refused `body_shape`. |
| Chunk framing | 1 KiB per chunk-size line and 16 KiB of trailers from an upstream. |
| Connection cap | 4,096 on the proxy listener and on the feature listener, 256 on the admin listener, 64 on the metrics listener. |
| Cooldown booking | Seven days ahead of each announcement, and a later announcement books the next stretch. |
| Decoded output | 1 MiB per stream chunk, 64 KiB for an inspected or buffered body, 4 MiB for a control-plane answer. |
| Header read timeout | 30 seconds for a request head on each listener, the feature listener included, after the TLS handshake's own 30. |
| Keepalive ticker | One scheduling pass per second, and a probe answer is read up to 256 KiB. |
| Ledger assembler | 16,384 requests in flight, 64 MiB captured, oldest or largest evicted as `ledger_gap`. |
| Ledger channel | 4,096 records, `ledger_behind` at 2,048 queued, and an event or sample past it drops as a `ledger_gap`, but for the event of a limit reset's use, which waits for room. |
| Name resolution | 5 seconds per lookup of an upstream or catalog host. |
| Parsed tree bytes | 363 bytes per body element, counted against `proxy.body_budget_bytes` from before the parse until the relay drops the tree. |
| Relay waits | 600 seconds for a response head, 600 seconds between stream events. |
| Shutdown budget | 35 seconds for the whole stop, shared by its phases in order, with up to 30 of them for the drain of in-flight requests. A phase still running when it is spent is ended. |
| Stream hold before commit | 30 seconds or 64 KiB, whichever comes first. |
| TLS handshake | Bounded by the header read timeout, 30 seconds. |
| Upstream connection pool | 8 idle connections per account and host pair, discarded after 60 seconds idle. |
| User agent | 256 bytes kept on a request row, and a longer value is cut. |

A connection past a listener's cap waits in the kernel's backlog until one closes.
