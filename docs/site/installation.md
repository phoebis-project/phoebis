# Installation and the bootstrap file

The router is one binary, `phoebis`, built from this repository, and one YAML file that names its listeners, its two
database files, and the environment variable that holds its secret. This page walks the build, the service install, the
commands, and every setting the file accepts, with its default, its bounds, and the message a bad value gets.

## Download a release

Every release carries a static binary for `x86_64` and `aarch64` Linux, which runs on any Linux of its architecture
with no shared library, glibc version or distribution in the way:

```sh
release=https://github.com/daystrom-labs/phoebis/releases/download/<version>
curl -LO "$release/phoebis-<version>-x86_64-unknown-linux-musl.tar.gz"
curl -LO "$release/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf phoebis-<version>-x86_64-unknown-linux-musl.tar.gz
sudo install -m 0755 phoebis-<version>-x86_64-unknown-linux-musl/phoebis /usr/local/bin/phoebis
```

While the repository is private, downloading a release asset needs a token with `repo` scope, as the [operations
page](operations.md#releasing) says of `read:packages` for the image. GitHub answers the `curl -LO` lines above with
`404` while the repository is private, token or not, so fetch the archive and `SHA256SUMS` in their place with the
GitHub CLI: `GH_TOKEN=<token> gh release download <version> --repo daystrom-labs/phoebis --pattern
'phoebis-<version>-x86_64-unknown-linux-musl.tar.gz' --pattern SHA256SUMS`.

On an `aarch64` host the archive is `phoebis-<version>-aarch64-unknown-linux-musl.tar.gz`. Each archive holds `phoebis`
and `LICENSE`, and `SHA256SUMS` lists both, so `--ignore-missing` checks the one you downloaded. An operator who builds
from source follows [Build](#build) instead.

## Build

```sh
cargo build --release
```

The binary lands at `target/release/phoebis`. The embedded console needs Node 20.19 or a later 20, or 22.12 or later,
and npm on the path: the build script runs `npm ci` and `npm run build` in `web/` whenever the bundle is missing or
older than its sources. `PHOEBIS_SKIP_UI_BUILD=1` builds the binary without it, and the admin listener then serves a
placeholder page in place of the console.

The toolchain is pinned in `rust-toolchain.toml` to 1.96.1, and BoringSSL builds from source, which needs cmake,
clang, and libclang.

## Build and run

```sh
cargo build --release
target/release/phoebis init --config ~/.config/phoebis/config.yaml
export PHOEBIS_CONFIG=$HOME/.config/phoebis/config.yaml
export PHOEBIS_ADMIN_SECRET=<the printed secret>
target/release/phoebis
```

`init` writes the bootstrap file, the storage key beside it and an empty `data/` directory, and prints the admin secret
once as `PHOEBIS_ADMIN_SECRET=<secret>` ([commands](#commands)). The key is created once and never replaced: every
stored credential is sealed under it and there is no re-key. `PHOEBIS_CONFIG` names the file for every `phoebis` call in
that shell. `init` refuses to replace a key, and refuses a data directory that already holds a registry.

## Install as a service

```sh title="Install the binary, the unit, and the bootstrap files"
sudo install -m 0755 target/release/phoebis /usr/local/bin/phoebis
sudo useradd --system --home /var/lib/phoebis --shell /usr/sbin/nologin phoebis
sudo install -d -m 0755 /etc/phoebis
sudo install -m 0644 deploy/phoebis.service /etc/systemd/system/phoebis.service
sudo install -m 0644 crates/ledger/api_prices.yaml /etc/phoebis/api_prices.yaml
sudo phoebis init --config /etc/phoebis/config.yaml --data-dir /var/lib/phoebis \
  --env-file /etc/phoebis/phoebis.env --owner phoebis
sudo systemctl daemon-reload
```

Where the proxy serves the LAN, add `--proxy-listen 0.0.0.0:8317` to the `init` line, and `--proxy-cert` with
`--proxy-key` where a pair you hold terminates TLS. Without them the listener serves plain HTTP there and the client's
key crosses the LAN in the clear. `init` creates the key file at `0600` in one open, so no moment exists in which the
key is readable by anyone else, and the environment file at `0600` with the secret it generated. The bootstrap file and
the environment file are root's; the key file and the data directory are owned by `phoebis`. The key seals every
credential in `router.sqlite`, so it stays out of any backup that carries the database.

`deploy/phoebis.service` runs the process as the `phoebis` user with `EnvironmentFile=/etc/phoebis/phoebis.env`;
`phoebis` never reads the environment file itself, systemd hands the values over. `TimeoutStopSec=40` covers the
30-second drain, and `ProtectSystem=strict` with `StateDirectory=phoebis` is the hardening around it.

The file `init` wrote names your listeners. To reach the console or the proxy from another machine, pass
`--admin-listen` or `--proxy-listen`, with `--admin-cert` and `--admin-key` or `--proxy-cert` and `--proxy-key` for TLS,
or edit the `tls` key as [TLS on the listeners](#tls-on-the-listeners) says. It names
`/etc/phoebis/subscription_costs.yaml` as `stats.subscription_costs_path`, beside the price table; leave that file
uninstalled: the shipped model is the plan meter's current measurement and a release updates it, and a copy there would
shadow every later release's constants until it was copied again. Then validate it before enabling the service:

```sh
sudo bash -c 'set -a; . /etc/phoebis/phoebis.env; set +a; exec phoebis check-config --config /etc/phoebis/config.yaml'
```

`check-config` reads `admin.secret_env` from the process environment when it is set and prints
`warning: environment variable PHOEBIS_ADMIN_SECRET is unset or empty` when it is not, so sourcing the env file is
optional for it; the service and `phoebis health` still refuse an unset secret. The secret is printable ASCII without
surrounding spaces, at least 16 characters, and not the example file's placeholder. The start, `check-config`,
`health`, `status` and `tls reload` refuse another shape with the same line. Once it is clean,
`sudo systemctl enable --now phoebis`.

## Run the container

Every release publishes `ghcr.io/daystrom-labs/phoebis:<version>` for `linux/amd64` and `linux/arm64`. The
image holds the binary alone: no shell, no configuration, no key, and no price table (the shipped one is compiled
in). It runs as uid 65532, reads `/etc/phoebis/config.yaml` (`PHOEBIS_CONFIG` is set in the image, so `phoebis` needs no
flag), keeps both databases under `/var/lib/phoebis`, and probes its own proxy listener with `phoebis health` every 30
seconds, which Docker reports as the container's health.

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
docker run --rm -it -u "$(id -u)" -v "$PWD:/work" ghcr.io/daystrom-labs/phoebis:<version> \
  init --container --config /work/config.yaml
```

The key file is owned by uid 65532 at mode `0400` because Compose mounts a file secret with the host file's owner and
mode, and that uid is the one the process runs as. Set `image:` in `compose.yaml` to the version you want, then:

```sh title="Start, watch, and stop"
docker compose up -d
docker compose ps            # "healthy" once phoebis health reads the router as ready
docker compose logs -f phoebis
docker compose down          # SIGTERM, then up to 40 seconds for the 30-second drain
```

The health check runs `phoebis health`, which loads and validates the configuration, secrets included, from the
container's own environment and then asks the proxy listener whether the router is ready: a file it cannot read, a value
it refuses, an unset secret variable, or a router loop, ledger consumer or store that died therefore reads as
`unhealthy` rather than as a passing check, with the reason in `docker inspect`. `health` speaks TLS to a proxy listener
that terminates it.

A ledger writer that stops draining fills its queue. Once the queue is half full (2,048 of 4,096 records) the router
refuses new inference, token-count and keepalive probe requests with its own 503 `ledger_behind` and `Retry-After: 5`
rather than accepting requests it cannot record, so the proxy listener's connection slots stay free and
`phoebis health` keeps answering; a ledger consumer that stopped outright reads `unhealthy` through the readiness
answer itself. A request refused while the queue is that full has its row recorded only if the ledger's bus queue
still has room; one it had none for counts under `phoebis_bus_dropped_total{subscriber="ledger"}`.

The console answers at `http://localhost:8081`: the example publishes the admin port on the host's loopback alone, so it
is reached from the host's own browser or through an SSH tunnel whose local end is loopback, and nobody else.
`config.yaml` names no `tls` pair for the admin listener, so it serves plain HTTP, the cookie is issued without `Secure`
and any hostname that reaches the port logs in. Publishing the port on a LAN address is one edit to the `ports` line
(`"8081:8081"`), and then the admin secret at login and the session cookie on every call cross that LAN in the clear;
that is the same trust decision the proxy port's publication already makes. For TLS, mount a certificate pair read-only
beside the configuration and name the mounted paths under `tls`, the certificate world-readable and the key at `0400`
owned by uid 65532, and reload a replaced pair with `docker compose exec phoebis phoebis tls reload`. The clients trust
the pair's issuer, through `NODE_EXTRA_CA_CERTS` when it is a private CA. A front can terminate TLS instead: it forwards
to the plain listener and passes `Host` through unchanged beside `X-Forwarded-Proto: https`, so the cookie is issued
with `Secure` and the Origin check sees the front's hostname in both headers and requires the `https://` scheme in
`Origin`, as it does on a listener that terminates TLS itself.

The named volume `phoebis-data` takes the image's ownership of `/var/lib/phoebis` on first use; a bind mount in its
place needs `chown 65532` on the host directory first. A price table dropped at `/var/lib/phoebis/api_prices.yaml` on
the volume is read at the next start, as `stats.api_prices_path` in the container configuration names it, and a
subscription cost model dropped at `/var/lib/phoebis/subscription_costs.yaml` the same. Pulling the image needs
`docker login ghcr.io` with a token carrying `read:packages` while the repository is private. Upgrading is on the
[operations page](operations.md#upgrades).

## Commands

| Command | Effect |
| --- | --- |
| `phoebis --config ./config.yaml` | Serves the three listeners until Ctrl-C or SIGTERM, then drains up to 30 seconds. |
| `phoebis init --config ./config.yaml` | Writes the bootstrap file, the storage key and the admin secret, once. |
| `phoebis check-config --config ./config.yaml` | Validates the file, key file, canary, schemas and certificate pairs. |
| `phoebis health --config ./config.yaml` | Probes the proxy listener's readiness: exit 0 when ready, 1 otherwise. |
| `phoebis status --config ./config.yaml` | Prints readiness, pools, accounts and live pins: exit 0 when ready. |
| `phoebis backup --to <dir>` | Copies both databases into `<dir>` with a manifest; exit 0 once it is written. |
| `phoebis tls reload --config ./config.yaml` | Re-reads every listener's certificate pair: exit 0 once each reloaded. |
| `phoebis --version` | Prints the version. |

`--config` defaults to `./config.yaml`; when the flag is absent and `PHOEBIS_CONFIG` is set, every subcommand reads
that path instead, and the flag wins when both are given. `check-config` takes no lock, runs no migration and opens
no socket; it reads the key file, the registry's canary, the registry's and the ledger's schema versions and each
listener's certificate pair read-only, so it is safe to run against a production file while the router is up. It
prints, in order:

1. `{config path} is valid`
2. `proxy listens on {config.proxy.listen} over plain HTTP`, or `proxy listens on {addr} over TLS (certificate
   {cert_file}, expires {date})` for a certificate pair ([TLS on the listeners](#tls-on-the-listeners))
3. the admin line in the same two shapes, with `admin` and `config.admin.listen`
4. `metrics listens on {config.stats.metrics_listen}`
5. `proxy: body budget {n} bytes, body read {n}s, key body reads {n}, max request {n} bytes, sign cch placeholder
   {on|off}, upstream gzip {auto|on|off}`
6. `profile {the build's profile}`
7. `routing: cooldown_fallback {n}s, model_rejection {n}s, catalog refresh {n}s, startup fetch {n}s`
8. `ledger at {config.stats.ledger_path}`
9. `retention {n days|forever}, rollups {n days|forever}`
10. `keepalive: {n} sessions, {n} agents, {n} MiB retained`
11. the registry line, `registry at {config.storage.path} does not exist yet, and the first start creates it` when the
    registry is not there yet, or `registry at {config.storage.path} has no key canary yet. A sealed row opens under
    the key, and the next start seals one` for a registry from before the canary
12. `warning: environment variable {name} is unset or empty`, when the secret variable is unset

Further warning lines name a storage key file, data directory, database or TLS key file whose mode or owner is wider
than the router expects, and a certificate that expires within 30 days (`warning: proxy.tls.cert_file {path} expires in
{n} days ({date})`); the router starts either way and logs the same warnings. A TLS key owned by root and readable by
its group alone, such as `0640 root:phoebis`, earns no warning from a process that runs as root or holds that group.

A registry or a ledger whose schema is newer than the binary reads fails the check with exit 1 and `the registry at
{config.storage.path} is at schema {n}, newer than the {m} this build reads`, or the same line naming `the ledger at
{config.stats.ledger_path}`, since the start refuses either file the same way.

A registry that holds an account in more than one pool fails the check with exit 1 and the start's line for each such
account, `account {id} is a member of pools {pools}, and an account belongs to one pool`, since the start refuses to
migrate it.

A secret variable set to a value the start refuses fails the check with exit 1 and the start's line: `environment
variable {name} holds a secret shorter than 16 characters`, `environment variable {name} still holds the example
placeholder`, or `environment variable {name} holds a secret outside the allowed shape (printable ASCII, no surrounding
spaces)`.

An unreadable, mismatched, expired or not-yet-valid certificate pair fails the check with exit 1 and the line the start
prints, naming the key and the path ([TLS on the listeners](#tls-on-the-listeners)).

On shutdown, Ctrl-C or SIGTERM, the router drains in-flight requests for up to 30 seconds and then ends whatever is
still running. That 30 seconds also covers the keepalive shutdown ahead of it, so a slow keepalive shutdown leaves the
listeners whatever is left of the one budget rather than a fresh 30 seconds of their own. After the listeners drain, a
credential refresh still in flight is waited for up to its own 30-second timeout, so a token the upstream already
rotated is stored rather than lost to the next start's quarantine.

`health` loads the file the way the service does, so it needs the same environment. It connects to `proxy.listen`, or to
the loopback address of the same family when the listener is bound on every interface, sends `HEAD /api/ready`, over TLS
when `proxy.tls` terminates it, without checking the certificate (the probe dials its own process and asks whether
`/api/ready` answers, not who answers), and prints `proxy answers on {addr}` on a `200` or one line saying why not
(`proxy on {addr} answered 503, not 200` when the router loop, the ledger consumer or the store is dead); the whole
probe, connect included, is bounded by three seconds. `/api/hello` stays the liveness answer the client itself probes.
The container image runs `health` as its health check every 30 seconds ([the container](#run-the-container)).

`init` writes a first installation and refuses to touch anything that exists. It writes, in this order, the storage key
file (64 hex characters and a newline, created at `0600` in one open), the environment file when `--env-file` names one
(`0600`, `<NAME>=<secret>` and `RUST_LOG=info`), and the bootstrap file at `--config` (`0644`); it creates the data
directory and every missing parent of those paths at `0700`. It opens no database, so the first start creates both and
seals the key canary. The bootstrap file is [the example](#the-bootstrap-file) with the two listeners,
`admin.secret_env`, the key file, both databases under the data directory, `stats.api_prices_path` (`api_prices.yaml`
beside the bootstrap file) and `stats.subscription_costs_path` (`subscription_costs.yaml` beside it) filled in, every
path absolute and every other line as the example spells it. The admin secret is 32 random bytes in base64url, 43
characters, and never lands in the bootstrap file. `init` loads the written file the way `check-config` does before it
answers.

| Flag | Default | Effect |
| --- | --- | --- |
| `--admin-cert PATH`, `--admin-key PATH` | unset | An operator's pair as `admin.tls`, both paths made absolute. |
| `--admin-listen ADDR` | `127.0.0.1:8081` | The admin listener's address. |
| `--container` | off | Writes the compose directory's shape, below. |
| `--data-dir PATH` | `data` beside the file | Holds `router.sqlite` and `ledger.sqlite`. |
| `--env-file PATH` | unset | Writes the admin secret there rather than printing it. |
| `--key-file PATH` | `storage.key` beside the file | The storage key file. |
| `--non-interactive` | off | Asks nothing, even on a terminal. |
| `--owner USER` | unset | Gives that user the key file and each directory `init` created for the service; needs root. |
| `--proxy-cert PATH`, `--proxy-key PATH` | unset | An operator's pair as `proxy.tls`, both paths made absolute. |
| `--proxy-listen ADDR` | `127.0.0.1:8317` | The proxy listener's address. |
| `--secret-env NAME` | `PHOEBIS_ADMIN_SECRET` | The admin secret's variable, written as `admin.secret_env`. |

`init` reads neither file of a pair; `check-config` and the start check it. A pair on a loopback address is written as
given, and a listener with no pair gets no `tls` line on any address. `--owner` hands over the key file, the data
directory when `init` created it, and every parent `init` created on the way to the key file, the data directory or the
bootstrap file, all to the user and its primary group, so the service can traverse them. A directory that existed keeps
its owner. The bootstrap file, the environment file and a parent created only for the environment file stay the
caller's, since systemd reads the environment file as root.

A pair is the operator's own. Under the service unit it lives under `/etc/phoebis`, the certificate at `0644` and the
key at `0640 root:phoebis`, and in the container shape it is mounted read-only beside the configuration, the key at
`0400` owned by uid 65532. Its leaf names every host name and address the clients dial. The start reads the pair once,
as [TLS on the listeners](#tls-on-the-listeners) says.

Without `--env-file`, stdout carries one line, `PHOEBIS_ADMIN_SECRET=<secret>` under the default name, and the secret is
printed nowhere else, so `(umask 077; phoebis init --non-interactive > phoebis.env)` writes a usable environment file
that only its owner reads; a bare redirect under the usual umask leaves the secret readable by others. `--env-file`
writes that line beside `RUST_LOG=info` to the file it names, at `0600`. Everything else goes to stderr. On a terminal
(stdin and stderr both) and without `--non-interactive`, `init` asks, once the refusals below have found nothing, for
what the flags did not give, in this order: the proxy address, the admin address, and for each that leaves loopback with
no pair, `files` and then the two paths, or `none` for plain HTTP, the default. An empty answer takes the default shown
in brackets, and an address that does not parse is asked again. Then it prints one line per path written with its mode
and owner, where the secret went, and the next command, `phoebis check-config --config <path>`.

`init` exits 1 with one `Error:` line and nothing written when the bootstrap file, the key file or the environment file
already exists, when a `router.sqlite` or `ledger.sqlite` already sits in the data directory
(`{path} exists and a new key cannot open it, so keep the key that sealed it, or name an empty --data-dir`), when input
ends before a question is answered, and when `--owner` runs off root or names no user. A directory or a file it cannot
write exits 1 too, after removing what the run wrote, which the line names. A usage error, such as `--proxy-cert`
without `--proxy-key` or an address that does not parse, exits 2.

Under `--container` the bootstrap file's directory is the compose directory: `init` writes `storage.key` and
`phoebis.env` beside the file, the container's paths inside it (`/run/secrets/router-storage-key`,
`/var/lib/phoebis/router.sqlite`, `/var/lib/phoebis/ledger.sqlite`, `/var/lib/phoebis/api_prices.yaml` and
`/var/lib/phoebis/subscription_costs.yaml`), both listeners on every interface with no `tls` line, and
`metrics_listen: 0.0.0.0:9464`. It asks nothing and creates no data directory, since `/var/lib/phoebis` is the volume
inside the container. The listen and TLS flags still apply; `--data-dir`, `--env-file` and `--key-file` are usage errors
beside it. The summary ends with the steps that remain, the key file's owner and mode and the image tag in
`compose.yaml`, and with `docker compose run --rm phoebis check-config` as the next command, since the key's path in the
file is the container's.

`status` probes the proxy listener, reads the admin API and prints one screen of readiness, pools, accounts and live
pins ([Is it up?](operations.md#is-it-up) shows the screen and reads it line by line). It probes `proxy.listen` as
`health` does, then reads `GET /admin/system`, `/admin/attention`, `/admin/pools`, `/admin/accounts` and
`/admin/sessions`, each call under the `--timeout` deadline; a read that fails prints one line in its section, and the
rest of the screen still prints. No line passes 80 columns, and `--all` prints every pin and every account in no pool
rather than the first 20 of each. `--json` prints one object instead: `health` (`ready`, `addr` and `reason`, or `null`
when no probe ran), the five answers verbatim (`null` for a read that failed) and `errors`, each failed read's reason
under its name. Neither output carries the admin secret. `status` exits 0 when the probe, where it ran, answered `200`
and `/admin/system` answered `ready`; 1 when the proxy does not answer, a component is dead, the secret is refused (`the
admin secret was refused`), the admin listener does not answer, or the file does not load; 2 for a usage error. After a
refused secret or a throttled login the reads that follow print its line without calling again, so a run with a wrong
secret counts one failed login toward the throttle.

The commands that call the admin API, `status` among them, send the admin secret as `Authorization: Bearer`, one
connection per call, and share five flags:

| Flag | Default | Effect |
| --- | --- | --- |
| `--ca PATH` | the system roots | The CA bundle trusted in place of the system roots, for a pair a private CA signed. |
| `--json` | off | Prints the answer as JSON on stdout. |
| `--secret-env NAME` | `admin.secret_env` | The variable the admin secret is read from; unset, the call is refused. |
| `--timeout SECONDS` | `10` | Each call's deadline, the connect and the TLS handshake included. |
| `--url URL` | from `admin.listen` and `admin.tls` | The admin listener's base, `http://` or `https://` and a host. |

`--url` defaults to the scheme `admin.tls` gives and the address of `admin.listen`, on the loopback of its family when
the listener is bound on every interface, as `health` reaches the proxy; `--ca` names the CA to trust in place of the
system roots, for a listener whose pair a private CA signed. With `--url` and `--secret-env` both given the file is not
read, so the binary reads a router on another machine: `status` then has no proxy listener to probe, prints `proxy: not
probed, no configuration file names the listener`, and reads readiness from `/admin/system` alone. Without both, the
file at `--config` must load.

`status` and `tls reload` take the shared flags above. Every account, pool and key is created and edited in the console.

`backup` copies the registry and the ledger into a directory, each through SQLite's online backup API as one
consistent snapshot while the router serves or while it is stopped, and writes `manifest.json` last
([Operations](operations.md#backup) shows the container line and the restore). `--to` names the directory, which
`backup` creates with every missing parent at `0700`, or an existing empty one that only its owner can write. It refuses
one that group or other can write or that another user owns, and warns when its mode is wider than `0700` without giving
group or other write access. Without `--to` the directory is `./phoebis-backup-<YYYYMMDD>T<HHMMSS>Z`, stamped in UTC.
Inside go `router.sqlite`, `ledger.sqlite` and `manifest.json`, each at `0600`. The manifest names the key file at
`storage.encryption_key_file`, which `backup` never reads; a key file that does not exist earns a warning and the backup
proceeds. `backup` loads the file the way `check-config` does, without the admin secret, and takes no lock and opens no
socket. Stderr carries one `wrote {path} ({n} bytes)` line per file, the two `quick_check` results, the manifest's path,
and last `the key file {path} is not in this backup. Keep it apart, since the registry copy is sealed under it`;
stdout stays empty unless `--json` prints the manifest there.

`backup` exits 1 with one `Error:` line and nothing written when the file does not load, when `--to` names a file or a
directory that is not empty (`{path} is not an empty directory`), when it names a directory that group or other can
write or that another user owns (`{dir}: group or other can write it, or another user owns it. Nothing was written.`),
or when either database does not exist (`a backup carries both databases, but {path} does not exist`). A copy that
fails, a database another connection holds locked for ten seconds among them, removes every file and directory the run
created and exits 1 with `backup failed: {reason}. Nothing was written to {dir}`. It exits 0 once the manifest is
written, and 2 for a usage error.

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

Pick your own name; `PHOEBIS_ADMIN_SECRET` is what the example file happens to use. `RUST_LOG` sets the log filter
and defaults to `info`.
Nothing else is named here: an account's upstream credential and the keys downstream clients present belong to
the registry, not the bootstrap file.

`init --env-file` writes the file the service reads with a generated secret; without it, `init` prints the secret once.

## The bootstrap file

```yaml title="deploy/config.example.yaml"
# Bootstrap settings only. The admin secret is named here and read from the environment at startup, so this
# file can be committed and read by anyone who may read the deployment.
admin:
  listen: 127.0.0.1:8081
  # tls: {cert_file: ..., key_file: ...} names your certificate pair and the listener terminates TLS.
  # Without tls the listener serves plain HTTP on the address named, loopback or not.
  secret_env: PHOEBIS_ADMIN_SECRET
keepalive:
  # Process-wide limits only. Whether a pool's sessions are probed, and for how long, are pool settings in
  # the registry. The largest retained recipe follows proxy.max_request_bytes, the relay's request body bound.
  max_agents: 1024
  max_retained_bytes: 268435456
  max_sessions: 128
proxy:
  listen: 127.0.0.1:8317
  # tls: {cert_file: ..., key_file: ...} names your certificate pair and the listener terminates TLS.
  # Without tls the listener serves plain HTTP on the address named, loopback or not.
  # The bounds on the request bodies the proxy holds, at their defaults; uncomment a key to change it.
  # body_budget_bytes: 536870912
  # body_read_seconds: 60
  # key_body_reads: 128
  # max_request_bytes: 33554432
  # Signs a restored cch=00000 with this build's formula; off until a server rejects the placeholder.
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
  # The relay's memory of a session's previous request id and prompt id: entries held in process, how long one
  # may go unseen before it leaves memory, and how long its on-disk copy is kept.
  memory_cap: 65536
  memory_idle_seconds: 86400
  memory_retention_days: 30
stats:
  # Overrides the shipped price table; a missing file is fine.
  api_prices_path: ./api_prices.yaml
  # The longest a key or pool may keep captured bodies, 1 to 168.
  body_capture_max_hours: 72
  ledger_path: ./data/ledger.sqlite
  # Prometheus text format at /metrics; bind it where your scraper can reach it.
  metrics_listen: 127.0.0.1:9464
  # Raw requests, attempts, events, and the quota and extra-usage samples are purged past this; 0 keeps them forever.
  retention_days: 90
  # Hourly rollups are purged past this; 0 keeps them forever, otherwise at least retention_days.
  rollup_retention_days: 400
  # Overrides the shipped subscription cost model; a missing file is fine.
  subscription_costs_path: ./subscription_costs.yaml
storage:
  encryption_key_file: /run/secrets/router-storage-key
  path: ./data/router.sqlite
upstream:
  # Both unset by default: an extra trust anchor for a lab upstream, and one address every host resolves to.
  # extra_ca_pem: ./lab-ca.pem
  # resolve_override: 127.0.0.1:8443
usage:
  # Optional: how many accounts' usage and profile fetches run at once.
  refresh_concurrency: 2
```

An unknown key anywhere refuses the file: every struct denies unknown fields, and the message is `the
configuration is not valid YAML`, which a syntax error also gets. Each top-level block below lists its keys'
defaults, what they accept, and the message a refused value gets.

### admin

Required.

Phoebis Router is built for one operator: one admin secret reaches every route, and the console is that person's
own. Every mutation through the admin API, a create, an edit, a delete or a login, is written to the log as one
`info` line naming its method, route, status, entity and session id, never its body or a secret; the host's journal
is the trail.

| Key | Default | Accepts |
| --- | --- | --- |
| `listen` | required | a socket address |
| `secret_env` | required | the variable holding the admin secret |
| `tls` | unset, plain HTTP | `{cert_file, key_file}` |

The `tls` key is optional on every address. Without it the listener serves plain HTTP and issues the cookie without
`Secure`, which costs nothing behind an SSH tunnel and sends the admin secret at login and the session cookie on every
call in the clear on a network address; a front's `X-Forwarded-Proto: https`, below, is the one exception. A certificate
pair terminates TLS on the listener on any address and the cookie carries `Secure`. Every mutation that carries an
`Origin` must name the request's own `Host` with it, and with the `https://` scheme where the listener terminates TLS;
a request with no `Origin` is admitted. A front that terminates TLS forwards to a loopback listener over plain HTTP and
passes `Host` through unchanged beside `X-Forwarded-Proto: https`; the cookie is then issued with `Secure`, and the
Origin check sees the front's hostname in both headers and requires the `https://` scheme in `Origin`, as it does on a
listener that terminates TLS itself.

Refused with:

- `listen`: a YAML error
- `secret_env`: `environment variable {name} is unset or empty`, and the lines for a value shorter than 16
  characters, the example file's placeholder or another shape ([environment variables](#environment-variables))
- `tls`: `admin.tls: must be a mapping with cert_file and key_file` under `the configuration is not valid YAML` for a
  word in the key's place, the key with nothing beneath it, a third key, a repeated key, or a mapping that lacks one
  of the two ([TLS on the listeners](#tls-on-the-listeners))

Removed keys: `admin.origin` has no replacement, the Origin check compares against the request's own `Host` in every
mode; `admin.plain_http: true` has no replacement either: a listener without `tls` serves plain HTTP on any address.

### keepalive

Defaults whole. These are process-wide limits read once at start, so a change takes a restart; the per-pool
tuning is on the [Keepalive](keepalive.md) page. Every bound below refuses with `keepalive.{field} must be
between {min} and {max}, not {value}`.

| Key | Default | Accepts |
| --- | --- | --- |
| `max_agents` | `1024` | 1 to 16384 |
| `max_retained_bytes` | `268435456` | `proxy.max_request_bytes` (the body bound a recipe follows) to 536870912 |
| `max_sessions` | `128` | 1 to 4096 |

`max_sessions` counts live sessions only: an ended session frees its slot at once, though its tombstone stays until
the session expiry sweeps it. The mod's lease is 90 seconds and four probes run at once. Neither is a key.

### proxy

Required: `listen`, a socket address, refused with a YAML error. `tls` names what the listener serves ([TLS on the
listeners](#tls-on-the-listeners)). Four keys bound the request bodies the proxy holds; `sign_cch_placeholder` decides
whether a restored billing placeholder is signed, and `upstream_gzip` how the edited body leaves toward the upstream.
All six are tunable in the file for power users and have no console control.

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

- `body_budget_bytes`: `proxy.body_budget_bytes must be at least proxy.max_request_bytes ({minimum}), not {value}`;
  and `proxy.body_budget_bytes must be at most 1099511627776, not {value}` past 2^40
- `body_read_seconds`: `proxy.body_read_seconds must be between 1 and 599, not {value}`
- `key_body_reads`: `proxy.key_body_reads must be between 1 and 4096, not {value}`
- `max_request_bytes`: `proxy.max_request_bytes must be between 1024 and 33554432, not {value}`
- `sign_cch_placeholder`: a YAML error for anything but a boolean
- `tls`: `proxy.tls: must be a mapping with cert_file and key_file` under `the configuration is not valid YAML` for a
  word in the key's place, the key with nothing beneath it, a third key, a repeated key, or a mapping that lacks one
  of the two ([TLS on the listeners](#tls-on-the-listeners))
- `upstream_gzip`: `proxy.upstream_gzip must be auto, on or off, not {value}`

`max_request_bytes` is the per-body bound, the Messages API's own request limit, so the router never refuses what the
upstream accepts, and a value above it is refused as pointless. `body_budget_bytes` is the bytes of bodies the whole
process holds at once, sized for a household whose keys carry agent tools that fan out (512 one-MiB bodies, or sixteen
at the per-body bound); a request that would pass it is refused with the router's own `503 body_budget_exhausted`. The
budget counts each body as received; the edited copy and the wire copy a pass holds beside it can take up to twice that
again, so size the process's memory at three times `body_budget_bytes`. `key_body_reads` bounds the bodies one key has
arriving at once; a request past it is refused `429 key_body_reads` with `Retry-After: 1`. `body_read_seconds` is how
long a body may take to arrive after authentication, under the stream's idle bound of 600 seconds; a body that takes
longer is answered `408 request_timeout`.

`sign_cch_placeholder` signs a restored `cch=00000` with this build's formula instead of sending the placeholder; off
until a server rejection of the placeholder proves it necessary.

The router accepts a request body sent with `content-encoding: gzip`: it inflates the body before reading it, within
`max_request_bytes`, so the inflated size is what the per-body bound and `body_budget_bytes` measure (the inflated
worst case is reserved against `body_budget_bytes` before the body is inflated); every other request encoding is
refused. `upstream_gzip` says how the edited body leaves toward the upstream: `auto` sends gzip when the client sent
gzip and identity otherwise, `on` always gzip, `off` always identity; the `content-encoding` and `content-length` toward
the upstream follow the bytes actually sent.

### TLS on the listeners

The proxy and the admin listener each take one `tls` key under their block, a mapping with `cert_file` and `key_file`
naming your certificate pair; the listener then terminates TLS itself:

```yaml title="config.yaml"
proxy:
  listen: 0.0.0.0:8317
  tls:
    cert_file: /etc/phoebis/proxy.pem
    key_file: /etc/phoebis/proxy.key
```

Without the key a listener serves plain HTTP on the address named, loopback or not; no address requires the key. A
plain listener on a network address sends in the clear what a certificate would cover: on the admin listener the
admin secret at login and the session cookie on every call, on the proxy the client's key on every request. That is
your trust decision for that network, and the file records it by naming no pair. Both listeners speak HTTP/1.1 only,
and the handshake steers a client that offers HTTP/2 to HTTP/1.1.

`cert_file` is PEM with the leaf first and any intermediates after it; `key_file` is the matching private key in PEM,
PKCS#8 or the traditional RSA and EC forms. Under the service unit, keep both under `/etc/phoebis`: the certificate at
`0644 root:root`, the key at `0640 root:phoebis` or `0600 phoebis:phoebis`. In the container, mount them read-only
beside the configuration, with the key at `0400` owned by uid 65532. A Tailscale certificate, a certificate from a LAN
CA, or one an ACME client renews all serve.

The router generates no certificate. A certificate from a public CA needs nothing on the clients. A certificate from a
private CA (a LAN CA, a Tailscale certificate's issuer, a CA you run) needs the clients to trust that CA: copy its
certificate to each client machine and set `NODE_EXTRA_CA_CERTS=<path>` beside `ANTHROPIC_BASE_URL` in the client's
[environment block](client.md#the-environment-block). The client checks the CA before anything else, so the leaf needs
no pin. `phoebis status` and `phoebis tls reload` take the same file through `--ca`.

The start reads the pair once. A renewal is: replace both files, then `phoebis tls reload`, which asks the running
router to re-read every listener's pair. A pair that fails to load (a key that does not match its certificate, a file
the service user cannot read) keeps the pair the router was serving and the command exits 1 naming the refusal, and the
next reload clears it. A reload that lands between the two writes meets a key mismatch and is retried once both files
are in place. The service user reads the pair at reload as it did at start, so a new pair keeps the modes above.

`phoebis tls reload` takes `--config` and [the shared flags](#commands), `--ca`, `--json`, `--secret-env`, `--timeout`
and `--url`, and posts [the reload route](api.md#post-admintlsreload). It prints one line per listener on stdout:
`proxy: reloaded, certificate {path} expires {date}`, `proxy: not reloaded, {the loader's line}. The router keeps the
pair it was serving.`, or `proxy: plain HTTP, nothing to reload`, and the same for `admin`. `--json` prints the route's
body instead. It exits 0 once every pair reloaded, a plain listener aside, 1 when any pair was refused, the secret was
refused, the admin listener did not answer or the file does not load, and 2 for a usage error.

The dashboard raises two attention items for a listener's pair: `certificate_expiring` while the loaded leaf is within
30 days of its end (past it included), and `certificate_reload_failed` from a reload the loader refused until a later
reload lands or the router restarts. Both read the pair the router serves, so a file replaced on disk and not yet
reloaded changes neither. `check-config` and `status` read the files.

`check-config` and the start refuse a `tls` value that is not the mapping with:

- `proxy.tls: must be a mapping with cert_file and key_file`, under `the configuration is not valid YAML`, for a word
  in the key's place, the key with nothing beneath it, a third key, a repeated key, or a mapping that lacks one of the
  two

The start reads the pair before it binds a port, and `check-config` reads it the same way; both refuse it with one line
naming the key and the path:

- `proxy.tls.cert_file {path}: {OS error}` or `proxy.tls.key_file {path}: {OS error}` for a file it cannot read
- `proxy.tls.cert_file {path}: holds no PEM certificate`
- `proxy.tls.cert_file {path}: holds a PEM certificate block that does not parse`, the leaf's or an intermediate's
- `proxy.tls.key_file {path}: holds no PEM private key`
- `proxy.tls.key_file {path}: does not match the certificate in {cert_file}`
- `proxy.tls.cert_file {path}: expired on {date}`
- `proxy.tls.cert_file {path}: not valid before {date}`

The admin listener's lines carry `admin.tls` in place of `proxy.tls`. The start logs `serving TLS` for each listener
that terminates TLS, with its leaf's names and `not_after`, and the `listening` line names each listener's scheme.

A plain-HTTP request to a listener that terminates TLS gets no HTTP answer: the handshake fails and the connection
closes, so `curl` reports `curl: (35)` and a Node client `ECONNRESET` or `EPROTO`. Put `https://` in the caller's URL.

The metrics listener has no `tls` key and serves plain HTTP.

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

The failover visits every eligible account of the pool once per request, so the pool's size bounds a request's
attempts and no key sets a limit; a key the block does not list is refused at start and by `check-config` as an
unknown field, named in the error.

### sessions

Defaults whole. `expire_after_seconds` is how long a session may go without a request before it leaves the
Sessions view, taking its pin and any keepalive recipe with it. The three `memory_` keys bound the relay's
memory of a session's previous request id and prompt id, the record behind `cc_prev_req` and `cc_prompt_id`
([client](client.md#the-environment-block)).

| Key | Default | Accepts |
| --- | --- | --- |
| `expire_after_seconds` | `86400` | at least 60 |
| `memory_cap` | `65536` | 1 to 1048576 entries held in process; past it the least recently seen entry leaves memory |
| `memory_idle_seconds` | `86400` | 60 to 31536000; how long an entry may go unseen before it leaves memory |
| `memory_retention_days` | `30` | 1 to 3650; how long the on-disk copy of an entry is kept after its last request |

Refused with:

- `expire_after_seconds`: `sessions.expire_after_seconds must be at least 60`
- `memory_cap`: `sessions.memory_cap must be between 1 and 1048576, not {value}`
- `memory_idle_seconds`: `sessions.memory_idle_seconds must be between 60 and 31536000, not {value}`
- `memory_retention_days`: `sessions.memory_retention_days must be between 1 and 3650, not {value}`

### stats

Defaults whole.

| Key | Default | Accepts |
| --- | --- | --- |
| `api_prices_path` | `./api_prices.yaml` | a path; a missing file is fine |
| `body_capture_max_hours` | `72` | 1 to 168 |
| `ledger_path` | `./data/ledger.sqlite` | a file path with a parent directory, not the same file as `storage.path` |
| `metrics_listen` | `127.0.0.1:9464` | a socket address |
| `retention_days` | `90` | 0 for forever, else a number of days; also covers decisions, quota and extra-usage samples |
| `rollup_retention_days` | `400` | 0 for forever, otherwise at least `retention_days` |
| `subscription_costs_path` | `./subscription_costs.yaml` | a path; a missing file is fine |

Refused with:

- `api_prices_path`: `loading the price table from {path}` with the rule under it, `pricing table: {rule}`, for a
  malformed file, a repeated key, a cell that is not a finite non-negative number or a `long_context.over` that is
  not a whole number of tokens above zero, and with the read error under it for a file that is there and cannot be
  read
- `body_capture_max_hours`: `stats.body_capture_max_hours must be between 1 and 168, not {value}`
- `ledger_path`: `stats.ledger_path {path} must name a database file inside a directory`; and
  `stats.ledger_path and storage.path must name different files, not {path}` when the two paths match
- `metrics_listen`: a YAML error
- `retention_days`: nothing
- `rollup_retention_days`: `stats.rollup_retention_days must be 0 or at least stats.retention_days
  ({retention_days}), not {value}`
- `subscription_costs_path`: `loading the subscription cost model from {path}` with the rule under it,
  `plan table: {key}: {rule}`, and with the read error under it for a file that is there and cannot be read

A configuration that names `pricing_path` is refused before anything prints with `the configuration is not valid YAML`,
and the cause serde prints under it: the unknown key under `stats`, the keys it expected, and the line and column
where it sits; `api_prices_path` names the price file in its place.

### storage

Required.

| Key | Accepts |
| --- | --- |
| `encryption_key_file` | a file holding 32 raw bytes or 64 hex characters, outside the database it unseals |
| `path` | a file path with a parent directory |

Refused with:

- `encryption_key_file`: checked when the store opens, not when the file loads
- `path`: `storage.path {path} must name a database file inside a directory`

### upstream

Defaults whole and is rarely set.

| Key | Default | What it does |
| --- | --- | --- |
| `extra_ca_pem` | unset | An extra trust anchor for a lab upstream not signed by a public authority. |
| `resolve_override` | unset | Sends every host to this address instead of DNS. |

### usage

Defaults whole. The poll and the refresh read the endpoint under the account's hold and answer the reading they made.
`refresh_concurrency` is how many accounts' usage and profile fetches may run at once.

| Key | Default | Accepts |
| --- | --- | --- |
| `refresh_concurrency` | `2` | 1 to 16 |

Refused with:

- `refresh_concurrency`: `usage.refresh_concurrency must be between 1 and 16, not {value}`

The checks above run in a fixed order at load, not block by block: `usage.refresh_concurrency`, `storage.path`, the
`proxy` bounds, `routing.model_rejection_seconds`, `routing.cooldown_fallback_seconds`, the `keepalive` bounds,
`routing.startup_fetch_seconds`, `routing.catalog_refresh_seconds`, the four `sessions` keys,
`stats.body_capture_max_hours`, `stats.rollup_retention_days`, `stats.ledger_path`, and last whether `stats.ledger_path`
and `storage.path` name the same file. The first failure is the one reported, so fix the file top to bottom to find the
next one.

`stats.ledger_path` and `storage.path` cannot name the same file because two schemas in one file open quietly
and lose the ledger's columns: the ledger's tables would open inside the registry file, read its schema
version, skip every ledger migration, and leave `requests` without the columns the writer inserts.

## Storage

`storage.path` names the database file itself rather than a directory, and `phoebis` uses the name as given. It
creates the parent directory at mode 0700 when it is missing, the database at mode 0600, and a `lock` file
beside the database that keeps a second process off the same data.

The registry's `lock` file is not taken by `check-config`, `backup` or any subcommand.

`storage.encryption_key_file` points at a separate file holding 32 raw bytes or 64 hex characters. It seals
every credential in the database, so keep it outside any backup that carries the database, which is useless
without it. `phoebis backup` leaves the key out and says so.

The registry file also holds the relay's session memory: for each key, conversation (the client's session id), and
agent, the previous request id and the prompt id the
relay restores into a Claude Code request's attribution line. The in-process copy is the one the relay reads; changes
reach the file every five seconds, at shutdown, and as soon as the records evicted from memory before they were written
reach `sessions.memory_cap` (that overlay is bounded by the cap too; a record it sheds because a write hung is counted
in the log), a session missing from memory after a restart or past `sessions.memory_cap` is read back from the file on
its next request, and rows older than `sessions.memory_retention_days` are dropped once an hour ([sessions](#sessions)).
A request takes its revision fence from the router's in-memory snapshot and its credential from a cache of each
account's current credential. The cache reads the registry file again when an account is paired, a credential rotates,
or the registry reloads; a credential the cache serves never waits behind the session memory's writes.

`stats.ledger_path` names a second SQLite file, the statistics ledger, under the same rules: the name is used
as given, the parent directory is created at mode 0700 and the file at 0600. It runs in WAL mode with
`synchronous = NORMAL`, so a power loss may lose the last transactions before a checkpoint. A request's facts
are never dropped: a request record waits for room in the ledger's channel, so a ledger writer that stalls
holds the request path rather than losing request facts; only captured bodies stay best-effort, since a stream
may outrun its recorder, and a `ledger_gap` event records what was lost. Captured bodies live in this file,
which makes it as sensitive as the registry even though it holds no credential, so give it the same care.

`phoebis backup` takes the snapshot; [Operations](operations.md#backup) says how and how to restore it.

![The Settings view: Appearance, About and the downstream client block
](assets/settings-light.png#only-light)
![The Settings view: Appearance, About and the downstream client block
](assets/settings-dark.png#only-dark)

## Compiled-in limits

Rows in alphabetical order of the limit's name. None of these has a bootstrap key.

| Limit | Value |
| --- | --- |
| Admin login throttle | Five failed logins per minute, then `too many failed logins, try again in a minute`. |
| Admin session lifetime | 15 minutes idle, 12 hours absolute. |
| Chunk framing | 1 KiB per chunk-size line and 16 KiB of trailers from an upstream. |
| Connection cap | 4,096 on the proxy listener, 256 on the admin listener, 64 on the metrics listener. |
| Cooldown booking | Seven days ahead of each announcement; a later announcement books the next stretch. |
| Decoded output | 1 MiB per stream chunk, 64 KiB for an inspected or buffered body, 4 MiB for a control-plane answer. |
| Drain window | 30 seconds for requests still in flight on shutdown; a request still running then is ended. |
| Header read timeout | 30 seconds for a request head on all three listeners, after the TLS handshake's own 30. |
| Keepalive ticker | One scheduling pass per second; a probe answer is read up to 256 KiB. |
| Ledger assembler | 16,384 requests in flight, 64 MiB captured, oldest or largest evicted as `ledger_gap`. |
| Ledger channel | 4,096 records; `ledger_behind` at 2,048 queued; an event or sample past it drops as a `ledger_gap`. |
| Name resolution | 5 seconds per lookup of an upstream or catalog host. |
| Relay waits | 600 seconds for a response head, 600 seconds between stream events. |
| Stream hold before commit | 30 seconds or 64 KiB, whichever comes first. |
| TLS handshake | Bounded by the header read timeout, 30 seconds. |
| Upstream connection pool | 8 idle connections per account and host pair, discarded after 60 seconds idle. |
| User agent | 256 bytes kept on a request row; a longer value is cut. |

A connection past a listener's cap waits in the kernel's backlog until one closes.
