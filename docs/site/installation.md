# Installation and the bootstrap file

The router is one binary, `phoebis`, built from this repository, and one YAML file that names its listeners, its two
database files, and the environment variables that hold its secrets. This page walks the build, the service
install, the three commands, and every setting the file accepts, with its default, its bounds, and the message a
bad value gets.

## Build

```sh
cargo build --release
```

The binary lands at `target/release/phoebis`. The embedded console needs Node 20 or newer and npm on the path: the
build script runs `npm ci` and `npm run build` in `web/` whenever the bundle is missing or older than its
sources. `PHOEBIS_SKIP_UI_BUILD=1` builds the binary without it, and the admin listener then serves a placeholder
page in place of the console.

The toolchain is pinned in `rust-toolchain.toml` to 1.96.1, and BoringSSL builds from source, which needs cmake,
clang, and libclang.

## Build and run

```sh
cargo build --release
install -d -m 0700 ~/.config/phoebis
[ -e ~/.config/phoebis/storage.key ] ||
  (umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > ~/.config/phoebis/storage.key)
sed -e "s#/run/secrets/router-storage-key#$HOME/.config/phoebis/storage.key#" \
  -e 's/dev_loopback_http: false/dev_loopback_http: true/' -e '/^  origin:/d' \
  deploy/config.example.yaml > config.yaml
export ROUTER_ADMIN_SECRET=$(openssl rand -base64 36) ROUTER_VIEWER_SECRET=$(openssl rand -base64 36)
target/release/phoebis --config config.yaml
```

The key is created once and never replaced: every stored credential is sealed under it and there is no re-key.

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

Edit `/etc/phoebis/phoebis.env` and replace both secrets with long random strings that differ. The key file is created
empty at 0600 before the key is written, rather than through a redirect, because a `tee` under `umask 022` would
leave it world-readable between the write and a later `chmod`. The key seals every credential in
`router.sqlite`, so it stays out of any backup that carries the database.

`deploy/phoebis.service` runs the process as the `phoebis` user with `EnvironmentFile=/etc/phoebis/phoebis.env`; `phoebis` never
reads the environment file itself, systemd hands the values over. `TimeoutStopSec=40` covers the 30-second
drain, and `ProtectSystem=strict` with `StateDirectory=phoebis` is the hardening around it.

Edit `/etc/phoebis/config.yaml` next. The installed file's paths are relative and its key file is
`/run/secrets/router-storage-key`, so point `storage.encryption_key_file` at `/etc/phoebis/storage.key`,
`storage.path` and `stats.ledger_path` under `/var/lib/phoebis/`, and `stats.pricing_path` at
`/etc/phoebis/pricing.yaml`. Set `admin.origin`, or `admin.dev_loopback_http: true` on a loopback listener behind an
SSH tunnel. Then validate it before enabling the service:

```sh
sudo bash -c 'set -a; . /etc/phoebis/phoebis.env; set +a; exec phoebis check-config --config /etc/phoebis/config.yaml'
```

`check-config` resolves `admin.secret_env` and `admin.viewer_secret_env` from the process environment like every
other invocation, so the env file has to be sourced into this one shell first; plain
`sudo phoebis check-config --config /etc/phoebis/config.yaml` fails with `environment variable ROUTER_ADMIN_SECRET is
unset or empty`. Once it is clean, `sudo systemctl enable --now phoebis`.

## Commands

| Command | Effect |
| --- | --- |
| `phoebis --config ./config.yaml` | Serves the three listeners until Ctrl-C or SIGTERM, then drains for up to 30 seconds. |
| `phoebis check-config --config ./config.yaml` | Validates the file, prints the listeners and routing, and exits. |
| `phoebis --version` | Prints the version. |

`--config` defaults to `./config.yaml`. `check-config` opens neither the store nor a socket, so it is safe to
run against a production file while the router is up. It prints, in order:

1. `{config path} is valid`
2. `proxy listens on {config.proxy.listen}`
3. `admin listens on {config.admin.listen}`
4. `metrics listens on {config.stats.metrics_listen}`
5. `profile {config.compatibility.profile}`
6. `routing: max_attempts {n}, cooldown_fallback {n}s, catalog sync every {n}s`
7. `ledger at {config.stats.ledger_path}`
8. `retention {n} days, samples {n} days, quota poll every {n}s`
9. `keepalive: lease {n}s, {n} sessions, {n} agents, {n} workers, {n} MiB retained, oauth prewarm
   {verified|unverified}`

On shutdown, Ctrl-C or SIGTERM, the router drains in-flight requests for up to 30 seconds.

## Environment variables

The bootstrap file names environment variables and never holds a secret. `phoebis` resolves them at startup and
refuses to start when one is unset or empty, with `environment variable {name} is unset or empty` and nothing
else. The two admin secrets must differ: a viewer secret that equals the admin one would silently turn the
read-only login into a full one, and `phoebis` refuses to start with `{first} and {second} resolve to the same
secret`.

| Variable in the example file | Named by | Holds |
| --- | --- | --- |
| `ROUTER_ADMIN_SECRET` | `admin.secret_env` | The admin login, which reaches every admin route. |
| `ROUTER_VIEWER_SECRET` | `admin.viewer_secret_env` | The read-only admin login. |

Pick your own names; those two are what the example file happens to use. `admin.viewer_secret_env` is the one
line you may delete outright, for no read-only login. `RUST_LOG` sets the log filter and defaults to `info`.
Nothing else is named here: an account's upstream credential and the keys downstream clients present belong to
the registry, not the bootstrap file.

## The bootstrap file

```yaml title="deploy/config.example.yaml"
# Bootstrap settings only. Every secret is named here and read from the environment at startup, so this file
# can be committed and read by anyone who may read the deployment.
admin:
  # With dev_loopback_http the session cookie is served without Secure over cleartext, which only a loopback
  # listener makes defensible. Left false here, so origin is required and TLS is terminated in front.
  dev_loopback_http: false
  listen: 127.0.0.1:8081
  origin: https://phoebis.example
  secret_env: ROUTER_ADMIN_SECRET
  viewer_secret_env: ROUTER_VIEWER_SECRET
compatibility:
  outbound_gzip: false
  profile: claude-code-2.1.269-linux-x64
  usage_entrypoint: cli
keepalive:
  # Process-wide limits only. Whether a pool's sessions are probed, and the probe tuning, are pool settings in
  # the registry; probes on oauth accounts stay off until the expiry-crossing experiment has passed for the
  # combinations in use and oauth_prewarm_verified is set.
  lease_seconds: 90
  max_agents: 1024
  max_request_bytes: 8388608
  max_retained_bytes: 268435456
  max_sessions: 128
  max_workers: 4
  oauth_prewarm_verified: false
proxy:
  listen: 127.0.0.1:8317
quota:
  # How often every OAuth account's usage endpoint is polled for a quota sample, at least 900 s. Response
  # headers sample active accounts for free; the poll fills idle gaps and is the only source of extra-usage
  # credit totals.
  poll_interval_seconds: 3600
  sample_retention_days: 30
routing:
  # How often the router re-discovers each account's served models, jittered so accounts do not sync
  # together.
  catalog_sync_interval_seconds: 86400
  cooldown_fallback_seconds: 60
  # Upstream attempts per client request across distinct accounts, 1 to 8.
  max_attempts: 3
sessions:
  expire_after_seconds: 86400
stats:
  # The longest a key or pool may keep captured bodies, 1 to 168.
  body_capture_max_hours: 72
  ledger_path: ./data/ledger.sqlite
  # Prometheus text format at /metrics; bind it where your scraper can reach it.
  metrics_listen: 127.0.0.1:9464
  # Overrides the shipped price table until an import is confirmed through the admin API; a missing file is
  # fine.
  pricing_path: ./pricing.yaml
  # Raw requests, attempts, and events are purged past this; hourly rollups are kept for 400 days.
  retention_days: 90
storage:
  encryption_key_file: /run/secrets/router-storage-key
  path: ./data/router.sqlite
usage:
  cache_ttl_seconds: 60
  refresh_concurrency: 2
```

An unknown key anywhere refuses the file: every struct denies unknown fields, and the message is `the
configuration is not valid YAML`, which a syntax error also gets. Each top-level block below lists its keys'
defaults, what they accept, and the message a refused value gets.

### admin

Required.

| Key | Default | Accepts |
| --- | --- | --- |
| `dev_loopback_http` | `false` | `true` only when `listen` is a loopback address |
| `listen` | required | a socket address |
| `origin` | unset | the origin every mutation's `Origin` header must equal; required unless `dev_loopback_http` |
| `secret_env` | required | the variable holding the admin secret |
| `viewer_secret_env` | unset | the variable holding the read-only secret |

Refused with:

- `dev_loopback_http`: `admin.dev_loopback_http serves the session cookie over cleartext, so admin.listen must
  be a loopback address, not {listen}`
- `listen`: a YAML error
- `origin`: `admin.origin is required unless admin.dev_loopback_http is set`
- `secret_env`: `environment variable {name} is unset or empty`
- `viewer_secret_env`: the same message, plus `{first} and {second} resolve to the same secret` when it equals
  the admin secret

### compatibility

Required, and all three fields are required. Only `profile` is consumed today; the other two are validated so a
file written for a later build is not accepted with the wrong meaning.

| Key | Accepts |
| --- | --- |
| `outbound_gzip` | `false` |
| `profile` | `claude-code-2.1.269-linux-x64`, the one profile this build carries |
| `usage_entrypoint` | `cli` or `sdk-cli` |

Refused with:

- `outbound_gzip`: `compatibility.outbound_gzip must be false: the native gzip gate is not implemented`
- `profile`: `compatibility.profile {name} is not a profile this build carries`
- `usage_entrypoint`: `compatibility.usage_entrypoint must be cli or sdk-cli, not {value}`

### keepalive

Defaults whole. These are process-wide limits read once at start, so a change takes a restart; the per-pool
tuning is on the [Keepalive](keepalive.md) page. Every bound below refuses with `keepalive.{field} must be
between {min} and {max}, not {value}`.

| Key | Default | Accepts |
| --- | --- | --- |
| `lease_seconds` | `90` | 10 to 3600 |
| `max_agents` | `1024` | 1 to 16384 |
| `max_request_bytes` | `8388608` | 1024 to 8388608 |
| `max_retained_bytes` | `268435456` | 1024 to 536870912, and at least `max_request_bytes` |
| `max_sessions` | `128` | 1 to 4096 |
| `max_workers` | `4` | 1 to 64 |
| `oauth_prewarm_verified` | `false` | `true` once the expiry-crossing experiment has passed for combinations in use |

`max_retained_bytes` below `max_request_bytes` refuses with `keepalive.max_retained_bytes must be at least
keepalive.max_request_bytes` instead of the range message. Until `oauth_prewarm_verified` is set, no probe runs
on an `oauth` account.

### proxy

Required: `listen`, a socket address, refused with a YAML error.

### quota

Defaults whole.

| Key | Default | Accepts |
| --- | --- | --- |
| `poll_interval_seconds` | `3600` | at least 900 |
| `sample_retention_days` | `30` | at least 1 |

Refused with:

- `poll_interval_seconds`: `quota.poll_interval_seconds must be at least 900`
- `sample_retention_days`: `quota.sample_retention_days must be at least 1`

### routing

Defaults whole.

| Key | Default | Accepts |
| --- | --- | --- |
| `catalog_sync_interval_seconds` | `86400` | at least 300 |
| `cooldown_fallback_seconds` | `60` | any number of seconds |
| `max_attempts` | `3` | 1 to 8 |

Refused with:

- `catalog_sync_interval_seconds`: `routing.catalog_sync_interval_seconds must be at least 300`
- `cooldown_fallback_seconds`: nothing
- `max_attempts`: `routing.max_attempts must be between 1 and 8, not {value}`

### sessions

Defaults whole: `expire_after_seconds` is how long a session may go without a request before it leaves the
Sessions view, taking its pin and any keepalive recipe with it; default `86400`, at least 60, refused with
`sessions.expire_after_seconds must be at least 60`.

### stats

Defaults whole.

| Key | Default | Accepts |
| --- | --- | --- |
| `body_capture_max_hours` | `72` | 1 to 168 |
| `ledger_path` | `./data/ledger.sqlite` | a file path with a parent directory, not the same file as `storage.path` |
| `metrics_listen` | `127.0.0.1:9464` | a socket address |
| `pricing_path` | `./pricing.yaml` | a path; a missing file is fine |
| `retention_days` | `90` | at least 1 |

Refused with:

- `body_capture_max_hours`: `stats.body_capture_max_hours must be between 1 and 168, not {value}`
- `ledger_path`: `stats.ledger_path {path} must name a database file inside a directory`; and
  `stats.ledger_path and storage.path must name different files, not {path}` when the two paths match
- `metrics_listen`: a YAML error
- `pricing_path`: nothing
- `retention_days`: `stats.retention_days must be at least 1`

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

Required: `cache_ttl_seconds` is how long a cached usage snapshot is served before the next request triggers a
fresh fetch; `refresh_concurrency` is how many accounts' usage and profile fetches may run at once, at least 1,
refused with `usage.refresh_concurrency must be at least 1`.

The checks above run in a fixed order at load, not block by block: `compatibility`'s three fields, then
`admin`'s loopback-or-origin rule, then `usage.refresh_concurrency`, `storage.path`, `routing.max_attempts`, the
`keepalive` bounds, `routing.catalog_sync_interval_seconds`, `sessions.expire_after_seconds`,
`stats.body_capture_max_hours`, `quota.poll_interval_seconds`, `stats.retention_days`,
`quota.sample_retention_days`, `stats.ledger_path`, and last whether `stats.ledger_path` and `storage.path` name
the same file. The first failure is the one reported, so fix the file top to bottom to find the next one.

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

`stats.ledger_path` names a second SQLite file, the statistics ledger, under the same rules: the name is used
as given, the parent directory is created at mode 0700 and the file at 0600. It runs in WAL mode with
`synchronous = NORMAL`, so a crash loses at most the last transactions before a checkpoint. The ledger is best
effort by design: records reach it over a bounded channel, a full channel drops them, and a `ledger_gap` event
records how many were lost. Captured bodies live in this file, which makes it as sensitive as the registry even
though it holds no credential, so give it the same care.

No script in this repository backs these files up; [Operations](operations.md) says how.

![The Settings view: the effective bootstrap values, the listeners, retention, and the price table
](assets/settings-light.png#only-light)
![The Settings view: the effective bootstrap values, the listeners, retention, and the price table
](assets/settings-dark.png#only-dark)

## Compiled-in limits

Rows in alphabetical order of the limit's name. None of these has a bootstrap key.

| Limit | Value |
| --- | --- |
| Admin login throttle | Five failed logins per minute, then `too many failed logins, try again in a minute`. |
| Admin session lifetime | 15 minutes idle, 12 hours absolute. |
| Drain window | 30 seconds for requests still in flight on shutdown. |
| Keepalive ticker | One scheduling pass per second; a probe answer is read up to 256 KiB. |
| Ledger channel | 4,096 records; a full channel drops and records a `ledger_gap` event. |
| Live feed | 30 minutes or 2,048 finished requests kept; a client behind by 1,024 messages is disconnected. |
| Relay waits | 600 seconds for a response head, 600 seconds between stream events, a 32 MiB request cap. |
| Stream hold before commit | 30 seconds or 64 KiB, whichever comes first. |
| Upstream connection pool | 8 idle connections per account and host pair, discarded after 60 seconds idle. |
