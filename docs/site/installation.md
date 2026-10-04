# Installation and the bootstrap file

The router is one binary, `phoebis`, built from this repository, and one YAML file that names its listeners, its two
database files, and the environment variable that holds its secret. This page walks the build, the service install, the
commands, and every setting the file accepts, with its default, its bounds, and the message a bad value gets.

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
install -d -m 0700 ~/.config/phoebis
[ -e ~/.config/phoebis/storage.key ] ||
  (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > ~/.config/phoebis/storage.key)
sed -e "s#/run/secrets/router-storage-key#$HOME/.config/phoebis/storage.key#" \
  -e 's/plain_http: false/plain_http: true/' -e '/^  origin:/d' \
  deploy/config.example.yaml > ~/.config/phoebis/config.yaml
export PHOEBIS_CONFIG=$HOME/.config/phoebis/config.yaml
export PHOEBIS_ADMIN_SECRET=$(openssl rand -base64 36)
target/release/phoebis
```

The key is created once and never replaced: every stored credential is sealed under it and there is no re-key.
`PHOEBIS_CONFIG` names the file for every `phoebis` call in that shell ([commands](#commands)).

## Install as a service

```sh title="Install the binary, the unit, and the key file"
sudo install -m 0755 target/release/phoebis /usr/local/bin/phoebis
sudo useradd --system --home /var/lib/phoebis --shell /usr/sbin/nologin phoebis
sudo install -d -m 0700 -o phoebis -g phoebis /var/lib/phoebis
sudo install -d -m 0755 /etc/phoebis
sudo install -m 0644 deploy/phoebis.service /etc/systemd/system/phoebis.service
sudo install -m 0600 deploy/phoebis.env.example /etc/phoebis/phoebis.env
sudo install -m 0600 -o phoebis -g phoebis /dev/null /etc/phoebis/storage.key
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' | sudo tee /etc/phoebis/storage.key >/dev/null
sudo install -m 0644 deploy/config.example.yaml /etc/phoebis/config.yaml
sudo install -m 0644 crates/ledger/pricing.yaml /etc/phoebis/pricing.yaml
sudo systemctl daemon-reload
```

Edit `/etc/phoebis/phoebis.env` and replace the secret with a long random string. The key file is created empty at
0600 before the key is written, rather than through a redirect, because a `tee` under `umask 022` would leave it
world-readable between the write and a later `chmod`. The key seals every credential in
`router.sqlite`, so it stays out of any backup that carries the database.

`deploy/phoebis.service` runs the process as the `phoebis` user with `EnvironmentFile=/etc/phoebis/phoebis.env`;
`phoebis` never reads the environment file itself, systemd hands the values over. `TimeoutStopSec=40` covers the
30-second drain, and `ProtectSystem=strict` with `StateDirectory=phoebis` is the hardening around it.

Edit `/etc/phoebis/config.yaml` next. The installed file's paths are relative and its key file is
`/run/secrets/router-storage-key`, so point `storage.encryption_key_file` at `/etc/phoebis/storage.key`,
`storage.path` and `stats.ledger_path` under `/var/lib/phoebis/`, and `stats.pricing_path` at
`/etc/phoebis/pricing.yaml`. Set `admin.origin`, or `admin.plain_http: true` on a loopback listener behind an SSH
tunnel. Then validate it before enabling the service:

```sh
sudo bash -c 'set -a; . /etc/phoebis/phoebis.env; set +a; exec phoebis check-config --config /etc/phoebis/config.yaml'
```

`check-config` reads `admin.secret_env` from the process environment when it is set and prints
`warning: environment variable PHOEBIS_ADMIN_SECRET is unset or empty` when it is not, so sourcing the env file is
optional for it; the service and `phoebis health` still refuse an unset secret. Once it is clean,
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
install -m 0644 deploy/config.container.yaml config.yaml
install -m 0600 deploy/phoebis.env.example phoebis.env
sudo install -m 0400 -o 65532 -g 65532 /dev/null storage.key
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' | sudo tee storage.key >/dev/null
```

Edit `phoebis.env` and replace the secret with a long random string; both it and `storage.key` hold secrets and
stay out of version control. The key file is owned by uid 65532 at mode `0400` because Compose
mounts a file secret with the host file's owner and mode, and that uid is the one the process runs as. Set
`image:` in `compose.yaml` to the version you want, then:

```sh title="Start, watch, and stop"
docker compose up -d
docker compose ps            # "healthy" once phoebis health reads the router as ready
docker compose logs -f phoebis
docker compose down          # SIGTERM, then up to 40 seconds for the 30-second drain
```

The health check runs `phoebis health`, which loads and validates the configuration, secrets included, from the
container's own environment and then asks the proxy listener whether the router is ready: a file it cannot read, a value
it refuses, an unset secret variable, or a router loop, ledger consumer or store that died therefore reads as
`unhealthy` rather than as a passing check, with the reason in `docker inspect`.

A ledger writer that stops draining fills its queue. Once the queue is half full (2,048 of 4,096 records) the router
refuses new inference, token-count and keepalive probe requests with its own 503 `ledger_behind` and `Retry-After: 5`
rather than accepting requests it cannot record, so the proxy listener's connection slots stay free and
`phoebis health` keeps answering; a ledger consumer that stopped outright reads `unhealthy` through the readiness
answer itself. A request refused while the queue is that full has its row recorded only if the ledger's bus queue
still has room; one it had none for counts under `phoebis_bus_dropped_total{subscriber="ledger"}`.

The console answers at `http://localhost:8081`: the example publishes the admin port on the host's loopback alone,
so it is reached from the host's own browser or through an SSH tunnel whose local end is loopback, and nobody else.
`config.yaml` sets `admin.plain_http: true` with no `origin`, so the cookie is issued without `Secure` and any
hostname that reaches the port logs in. Publishing the port on a LAN address is one edit to the `ports` line
(`"8081:8081"`), and then the admin secret at login and the session cookie on every call cross that LAN in the
clear; that is the same trust decision the proxy port's publication already makes. For TLS, set `plain_http:
false`, set `origin` to the front's `https://` URL, terminate TLS in front of the published port, and open exactly
the hostname and port `origin` names: a login from any other origin is refused with `403`.

The named volume `phoebis-data` takes the image's ownership of `/var/lib/phoebis` on first use; a bind mount in its
place needs `chown 65532` on the host directory first. A price table dropped at `/var/lib/phoebis/pricing.yaml` on the
volume is read at the next start, as `stats.pricing_path` in the container configuration names it. Pulling the image
needs `docker login ghcr.io` with a token carrying `read:packages` while the repository is private. Upgrading is on the
[operations page](operations.md#upgrades).

## Commands

| Command | Effect |
| --- | --- |
| `phoebis --config ./config.yaml` | Serves the three listeners until Ctrl-C or SIGTERM, then drains up to 30 seconds. |
| `phoebis check-config --config ./config.yaml` | Validates the file, the key file and the registry's key canary. |
| `phoebis health --config ./config.yaml` | Probes the proxy listener's readiness: exit 0 when ready, 1 otherwise. |
| `phoebis --version` | Prints the version. |

`--config` defaults to `./config.yaml`; when the flag is absent and `PHOEBIS_CONFIG` is set, every subcommand reads
that path instead, and the flag wins when both are given. `check-config` takes no lock, runs no migration and opens
no socket; it reads the key file and the registry's canary read-only, so it is safe to run against a production file
while the router is up. It prints, in order:

1. `{config path} is valid`
2. `proxy listens on {config.proxy.listen}`
3. `admin listens on {config.admin.listen}`
4. `metrics listens on {config.stats.metrics_listen}`
5. `proxy: body budget {n} bytes, body read {n}s, key body reads {n}, max request {n} bytes, sign cch placeholder
   {on|off}, upstream gzip {auto|on|off}`
6. `profile {the build's profile}`
7. `routing: cooldown_fallback {n}s, model_rejection {n}s, catalog refresh {n}s, startup fetch {n}s`
8. `ledger at {config.stats.ledger_path}`
9. `retention {n days|forever}, rollups {n days|forever}`
10. `keepalive: {n} sessions, {n} agents, {n} MiB retained`
11. the registry line, `registry at {config.storage.path} does not exist yet; the first start creates it` when the
    registry is not there yet, or `registry at {config.storage.path} has no key canary yet; a sealed row opens under
    the key, and the next start seals one` for a registry from before the canary
12. `warning: environment variable {name} is unset or empty`, when the secret variable is unset

Further warning lines name a storage key file, data directory or database whose mode or owner is wider than the router
expects; the router starts either way and logs the same warnings.

On shutdown, Ctrl-C or SIGTERM, the router drains in-flight requests for up to 30 seconds and then ends whatever is
still running. That 30 seconds also covers the keepalive shutdown ahead of it, so a slow keepalive shutdown leaves the
listeners whatever is left of the one budget rather than a fresh 30 seconds of their own.

`health` loads the file the way the service does, so it needs the same environment. It connects to `proxy.listen`, or to
the loopback address of the same family when the listener is bound on every interface, sends `HEAD /api/ready`, and
prints `proxy answers on {addr}` on a `200` or one line saying why not (`proxy on {addr} answered 503, not 200` when the
router loop, the ledger consumer or the store is dead); the whole probe, connect included, is bounded by three seconds.
`/api/hello` stays the liveness answer the client itself probes. The container image runs `health` as its health check
every 30 seconds ([the container](#run-the-container)).

## Environment variables

The bootstrap file names the admin secret's environment variable and never holds a secret. `phoebis` resolves it at
startup and refuses to start when it is unset or empty, with `environment variable {name} is unset or empty` and nothing
else. A value shorter than 16 characters refuses with `environment variable {name} holds a secret shorter than 16
characters`, and the example file's own placeholder refuses with `environment variable {name} still holds the example
placeholder`.

| Variable | Named by | Holds |
| --- | --- | --- |
| `PHOEBIS_CONFIG` | `phoebis` itself, when `--config` is absent | The path of the bootstrap file. |
| `PHOEBIS_ADMIN_SECRET` | `admin.secret_env` | The admin login, which reaches every admin route. |

Pick your own name; `PHOEBIS_ADMIN_SECRET` is what the example file happens to use. `RUST_LOG` sets the log filter
and defaults to `info`.
Nothing else is named here: an account's upstream credential and the keys downstream clients present belong to
the registry, not the bootstrap file.

## The bootstrap file

```yaml title="deploy/config.example.yaml"
# Bootstrap settings only. The admin secret is named here and read from the environment at startup, so this
# file can be committed and read by anyone who may read the deployment.
admin:
  listen: 127.0.0.1:8081
  origin: https://phoebis.example
  # With plain_http the session cookie is served without Secure and origin is optional, on any address. Left
  # false here, so origin is required and TLS is terminated in front.
  plain_http: false
  secret_env: PHOEBIS_ADMIN_SECRET
keepalive:
  # Process-wide limits only. Whether a pool's sessions are probed, and the probe tuning, are pool settings in
  # the registry. The largest retained recipe follows proxy.max_request_bytes, the relay's request body bound.
  max_agents: 1024
  max_retained_bytes: 268435456
  max_sessions: 128
proxy:
  listen: 127.0.0.1:8317
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
  # The longest a key or pool may keep captured bodies, 1 to 168.
  body_capture_max_hours: 72
  ledger_path: ./data/ledger.sqlite
  # Prometheus text format at /metrics; bind it where your scraper can reach it.
  metrics_listen: 127.0.0.1:9464
  # Overrides the shipped price table; a missing file is fine.
  pricing_path: ./pricing.yaml
  # Raw requests, attempts, events, and the quota and extra-usage samples are purged past this; 0 keeps them forever.
  retention_days: 90
  # Hourly rollups are purged past this; 0 keeps them forever, otherwise at least retention_days.
  rollup_retention_days: 400
storage:
  encryption_key_file: /run/secrets/router-storage-key
  path: ./data/router.sqlite
upstream:
  # Both unset by default: an extra trust anchor for a lab upstream, and one address every host resolves to.
  # extra_ca_pem: ./lab-ca.pem
  # resolve_override: 127.0.0.1:8443
usage:
  # Both keys are optional: how long a cached usage snapshot answers a manual refresh, and how many accounts'
  # usage and profile fetches run at once.
  cache_ttl_seconds: 60
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
| `origin` | unset | the origin every mutation's `Origin` header must equal; required unless `plain_http` |
| `plain_http` | `false` | `true` serves the console over plain HTTP on any address |
| `secret_env` | required | the variable holding the admin secret |

With `plain_http: true` the session cookie is issued without `Secure` and `origin` may be left unset. On a
loopback listener behind an SSH tunnel that costs nothing. On any other address the admin secret at login and
the session cookie on every call cross the network in the clear, so publishing the admin port on a LAN is the
operator's trust decision, as it already is for the proxy listener. An `origin` set under the flag, such as
`http://phoebis.lan:8081`, is still compared against every mutation's `Origin` header and every WebSocket upgrade.
For TLS, keep the flag off, set `origin` to the front's `https://` URL, and terminate TLS in front of the
listener; a browser returns the `Secure` cookie to `http://localhost` as well, so the flag-off shape also works
from the host's own browser when `origin` names `http://localhost:8081`.

Refused with:

- `listen`: a YAML error
- `origin`: `admin.origin is required unless admin.plain_http is set`
- `secret_env`: `environment variable {name} is unset or empty`

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
the session expiry sweeps it. The companion's lease is 90 seconds and four probes run at once; neither is a key.

### proxy

Required: `listen`, a socket address, refused with a YAML error. Four keys bound the request bodies the proxy holds;
`sign_cch_placeholder` decides whether a restored billing placeholder is signed, and `upstream_gzip` how the edited body
leaves toward the upstream. All six are tunable in the file for power users and have no console control.

| Key | Default | Accepts |
| --- | --- | --- |
| `body_budget_bytes` | `536870912` | `max_request_bytes` to 2^40 |
| `body_read_seconds` | `60` | 1 to 599 |
| `key_body_reads` | `128` | 1 to 4096 |
| `max_request_bytes` | `33554432` | 1024 to 33554432 |
| `sign_cch_placeholder` | `false` | `true` or `false` |
| `upstream_gzip` | `auto` | `auto`, `on` or `off` |

Refused with:

- `body_budget_bytes`: `proxy.body_budget_bytes must be at least proxy.max_request_bytes ({minimum}), not {value}`;
  and `proxy.body_budget_bytes must be at most 1099511627776, not {value}` past 2^40
- `body_read_seconds`: `proxy.body_read_seconds must be between 1 and 599, not {value}`
- `key_body_reads`: `proxy.key_body_reads must be between 1 and 4096, not {value}`
- `max_request_bytes`: `proxy.max_request_bytes must be between 1024 and 33554432, not {value}`
- `sign_cch_placeholder`: a YAML error for anything but a boolean
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
| `body_capture_max_hours` | `72` | 1 to 168 |
| `ledger_path` | `./data/ledger.sqlite` | a file path with a parent directory, not the same file as `storage.path` |
| `metrics_listen` | `127.0.0.1:9464` | a socket address |
| `pricing_path` | `./pricing.yaml` | a path; a missing file is fine |
| `retention_days` | `90` | 0 for forever, otherwise any number of days; also covers the quota and extra-usage samples |
| `rollup_retention_days` | `400` | 0 for forever, otherwise at least `retention_days` |

Refused with:

- `body_capture_max_hours`: `stats.body_capture_max_hours must be between 1 and 168, not {value}`
- `ledger_path`: `stats.ledger_path {path} must name a database file inside a directory`; and
  `stats.ledger_path and storage.path must name different files, not {path}` when the two paths match
- `metrics_listen`: a YAML error
- `pricing_path`: nothing
- `retention_days`: nothing
- `rollup_retention_days`: `stats.rollup_retention_days must be 0 or at least stats.retention_days
  ({retention_days}), not {value}`

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

Defaults whole. `cache_ttl_seconds` is how long a cached usage snapshot answers a manual refresh before the endpoint is
read again; the poll reads every five minutes whatever the cache holds. `refresh_concurrency` is how many accounts'
usage and profile fetches may run at once.

| Key | Default | Accepts |
| --- | --- | --- |
| `cache_ttl_seconds` | `60` | 1 to 3600 |
| `refresh_concurrency` | `2` | 1 to 16 |

Refused with:

- `cache_ttl_seconds`: `usage.cache_ttl_seconds must be between 1 and 3600, not {value}`
- `refresh_concurrency`: `usage.refresh_concurrency must be between 1 and 16, not {value}`

The checks above run in a fixed order at load, not block by block: `admin`'s loopback-or-origin rule, then
`usage.cache_ttl_seconds`, `usage.refresh_concurrency`, `storage.path`, the `proxy` bounds,
`routing.model_rejection_seconds`, `routing.cooldown_fallback_seconds`, the `keepalive` bounds,
`routing.startup_fetch_seconds`, `routing.catalog_refresh_seconds`, the four `sessions` keys,
`stats.body_capture_max_hours`, `stats.rollup_retention_days`, `stats.ledger_path`, and last whether
`stats.ledger_path` and `storage.path` name the same file. The first failure is the one reported, so fix the file top
to bottom to find the next one.

`stats.ledger_path` and `storage.path` cannot name the same file because two schemas in one file open quietly
and lose the ledger's columns: the ledger's tables would open inside the registry file, read its schema
version, skip every ledger migration, and leave `requests` without the columns the writer inserts.

## Storage

`storage.path` names the database file itself rather than a directory, and `phoebis` uses the name as given. It
creates the parent directory at mode 0700 when it is missing, the database at mode 0600, and a `lock` file
beside the database that keeps a second process off the same data.

`storage.encryption_key_file` points at a separate file holding 32 raw bytes or 64 hex characters. It seals
every credential in the database, so keep it outside any backup that carries the database, which is useless
without it.

The registry file also holds the relay's session memory: for each key, conversation (the client's session id), and
agent, the previous request id and the prompt id the
relay restores into a Claude Code request's attribution line. The in-process copy is the one the relay reads; changes
reach the file every five seconds, at shutdown, and as soon as the records evicted from memory before they were written
reach `sessions.memory_cap` (that overlay is bounded by the cap too; a record it sheds because a write hung is counted
in the log), a session missing from memory after a restart or past `sessions.memory_cap` is read back from the file on
its next request, and rows older than `sessions.memory_retention_days` are dropped once an hour ([sessions](#sessions)).
A request takes its revision fence from the router's in-memory snapshot and its credential from a cache of each
account's current credential. The cache reads the registry file again when an account is paired, a credential rotates,
or the registry reloads; a request the cache serves never waits behind the session memory's writes.

`stats.ledger_path` names a second SQLite file, the statistics ledger, under the same rules: the name is used
as given, the parent directory is created at mode 0700 and the file at 0600. It runs in WAL mode with
`synchronous = NORMAL`, so a power loss may lose the last transactions before a checkpoint. A request's facts
are never dropped: a request record waits for room in the ledger's channel, so a ledger writer that stalls
holds the request path rather than losing request facts; only captured bodies stay best-effort, since a stream
may outrun its recorder, and a `ledger_gap` event records what was lost. Captured bodies live in this file,
which makes it as sensitive as the registry even though it holds no credential, so give it the same care.

No script in this repository backs these files up; [Operations](operations.md) says how.

![The Settings view: the downstream client block, the price table, Appearance and About
](assets/settings-light.png#only-light)
![The Settings view: the downstream client block, the price table, Appearance and About
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
| Header read timeout | 30 seconds for a connection to send its request head, on all three listeners. |
| Keepalive ticker | One scheduling pass per second; a probe answer is read up to 256 KiB. |
| Ledger assembler | 16,384 requests in flight, 64 MiB captured, oldest or largest evicted as `ledger_gap`. |
| Ledger channel | 4,096 records; `ledger_behind` at 2,048 queued; an event or sample past it drops as a `ledger_gap`. |
| Live feed | 30 minutes or 2,048 finished requests kept; a client behind by 1,024 messages is disconnected. |
| Name resolution | 5 seconds per lookup of an upstream or catalog host. |
| Relay waits | 600 seconds for a response head, 600 seconds between stream events. |
| Stream hold before commit | 30 seconds or 64 KiB, whichever comes first. |
| Upstream connection pool | 8 idle connections per account and host pair, discarded after 60 seconds idle. |
| User agent | 256 bytes kept on a request row and a live entry; a longer value is cut. |

A connection past a listener's cap waits in the kernel's backlog until one closes.
