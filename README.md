# phoebis

A local relay for Claude Code. A downstream client points at `phoebis` with a key the router issued; `phoebis` puts the request
on the wire to Anthropic, under credentials it holds itself. The `phoebis` process serves
three listeners: the proxy that relays `/v1/messages`, the admin API that pairs accounts and reports usage and
statistics, and a metrics endpoint in Prometheus text format.

## Build

```sh
cargo build --release
```

The binary is `target/release/phoebis`. The build embeds the web UI, which needs Node 20 or newer and npm on the path: the
build script runs `npm ci` and `npm run build` in `web/` whenever the bundle is missing or older than its sources.
`PHOEBIS_SKIP_UI_BUILD=1` builds the binary without it, and the admin listener then serves a placeholder page in place of
the console.

## Commands

| Command | Effect |
| --- | --- |
| `phoebis --config ./config.yaml` | Serves the three listeners until Ctrl-C or SIGTERM, then drains for up to 30 seconds. |
| `phoebis check-config --config ./config.yaml` | Validates the file, prints the listeners and routing, and exits. |
| `phoebis --version` | Prints the version. |

`--config` defaults to `./config.yaml`. Copy `deploy/config.example.yaml` and edit it. `check-config` opens neither the
store nor a socket, so it is safe to run against a production file while the router is up.

## Environment variables

The configuration file names environment variables; it never holds a secret. `phoebis` resolves them at startup and refuses
to start when one is unset or empty, naming the variable and nothing else. The two admin secrets have to differ: a
viewer secret that equals the admin one would turn the read-only login into a full one without saying so.

| Variable in the example file | Named by | Holds |
| --- | --- | --- |
| `ROUTER_ADMIN_SECRET` | `admin.secret_env` | The admin login, which reaches every admin route. |
| `ROUTER_VIEWER_SECRET` | `admin.viewer_secret_env` | The read-only admin login. |

Pick your own names; those two are what the example file happens to use. Both are required as the example file stands,
including the viewer secret: `admin.viewer_secret_env` is the one line you can delete outright if you want no read-only
login. `RUST_LOG` sets the log filter and defaults to `info`.

Nothing else is named here: an account's upstream credential and the keys downstream clients present belong to the
registry, which you build through the admin API and which the store keeps sealed. See "Registry" below.

### Storage

`storage.path` names the database file itself rather than a directory, and `phoebis` uses the name as given. It creates the
parent directory at mode 0700 when it is missing, the database at mode 0600, and a `lock` file beside the database that
keeps a second process off the same data. A path that names no file, or no directory to put it in, is rejected at
startup, and the resolved path is logged when the store opens.

`storage.encryption_key_file` points at a separate file holding 32 raw bytes or 64 hex characters. It seals every
credential in the database, so keep it outside any backup that carries the database, which is useless without it.

`stats.ledger_path` names a second SQLite file, the statistics ledger, under the same rules: the name is used as
given, the parent directory is created at mode 0700 and the file at 0600, and a path that names no file, or no
directory to put it in, is rejected at startup. It runs in WAL mode with `synchronous = NORMAL`, so a crash loses at
most the last transactions before a checkpoint. The ledger is best effort by design: records reach it over a bounded
channel, a full channel drops them, and a `ledger_gap` event records how many were lost. Captured bodies live in this
file, which makes it as sensitive as the registry even though it holds no credential, so give it the same care.

### Not yet configurable

Eight groups of limits are compiled into the binary, and the file has no key for any of them:

| Limit | Value |
| --- | --- |
| Admin session lifetime | 15 minutes idle, 12 hours absolute. |
| Drain window on shutdown | 30 seconds for requests still in flight. |
| Keepalive ticker | One scheduling pass per second; probe answers read up to 256 KiB. |
| Ledger channel | 4,096 records; a full channel drops. |
| Live feed | 30 minutes or 2,048 finished requests kept; a client behind by 1,024 messages is disconnected. |
| Relay waits | 600 seconds for a response head, 600 seconds between stream events, 32 MiB request cap. |
| Stream hold before commit | 30 seconds or 64 KiB, whichever comes first. |
| Upstream pool | 8 idle connections per account and host pair, discarded after 60 seconds idle. |

## Downstream client

```sh
ANTHROPIC_BASE_URL=https://<router host>
CLAUDE_CODE_OAUTH_TOKEN=<key secret>
_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
DISABLE_TELEMETRY=1
```

The third line is optional and switches the client to the full first-party request shape: two extra betas,
`x-client-request-id`, the `cch` billing header, `cc_prompt_id`, `diagnostics`, and the global cache scope. Without it
the client sends the reduced shape and `phoebis` relays that as it arrives. The variable is an internal one that a later
client build may drop.

The fourth line is optional too. The client's event logger posts its telemetry batches to `api.anthropic.com` whatever
`ANTHROPIC_BASE_URL` says, and it authenticates them with the credential it was given, which behind the router is your
key secret, so the secret leaves your network on every send. `DISABLE_TELEMETRY=1` switches the logger off;
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` does the same and also stops update checks and the bootstrap lookup. The
policy and settings lookups the client makes at start still go to `api.anthropic.com` with the secret under either
setting; they fail there and the client tolerates that.

Request headers travel as the client sent them, in its own order and spelling: `phoebis` drops the hop-by-hop and
forwarding headers, puts the account's credential where the client put yours, adjusts `anthropic-beta` to what the
account may claim, and owns `Host` and `Content-Length`. Nothing is added for a header the client left out, so any
Claude Code version relays on its own shape rather than on the measured one.

Remote Control is unavailable behind the router with or without that override. The client reports that
`ANTHROPIC_BASE_URL` does not point at `api.anthropic.com`, and it states that the override does not apply to Remote
Control.

The proxy listener speaks plain HTTP/1.1, as the admin listener does: your reverse proxy terminates TLS in front
of it, and `ANTHROPIC_BASE_URL` points at that reverse proxy. It does not accept HTTP/2, because a header's
order and spelling do not survive an h2 frame and the request would reach the upstream in a shape the client
never sent.

Put the key secret in `CLAUDE_CODE_OAUTH_TOKEN`, not in `ANTHROPIC_API_KEY`. The client gates three behaviors on a
subscriber check that an API key never passes: the one-hour cache TTL for the main conversation, the parsing of the
unified rate-limit headers on a 429, and the automatic continue after a usage-limit reset. A downstream
`ANTHROPIC_API_KEY` gives all three up.

## Pairing an account

An `api_key` account needs no pairing; its key is stored when the account is created. An `oauth` account is paired
through the admin API, which hands the operator an authorization URL and takes the result back.

Whenever `admin.origin` is set, every admin mutation has to carry a matching `Origin` header, and `phoebis` refuses the
request otherwise. The loopback development mode is the only configuration allowed to leave `origin` unset, and there
no origin is checked. Log in first and keep the cookie:

```sh
curl -sS -c jar.txt -X POST https://phoebis.example/admin/session \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"secret":"'"$ROUTER_ADMIN_SECRET"'"}'
```

Start a pairing session for the account slot. The answer carries an `id` and an `authorization_url`; `login` selects
`claude_ai` (the default) or `console`:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"account_id":"business","login":"claude_ai"}'
```

Open the `authorization_url` in a browser and sign in. The provider sends the browser to the loopback redirect the
session registered, `http://localhost:<port>/callback?code=...&state=...`, which is the same redirect the desktop
client uses. Nothing listens on that port, so the page fails to load; the URL in the address bar is still complete.
Paste it back (the answer to the start request names the exact `redirect_uri`):

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions/$ID/callback \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"url":"http://localhost:<port>/callback?code=...&state=..."}'
```

The code alone works as well, with the session state in its own field; the provider appends the state after a `#`
in the code it issues, and that suffix may stay:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions/$ID/callback \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"code":"<code>#<state>","state":"<state>"}'
```

`phoebis` parses the pasted URL and never fetches it; a URL whose scheme, host, port, or path differs from the
session's redirect is refused, as is a state that differs from the session's.

`phoebis` exchanges the code and answers with the upstream account and organization it reached. Check that they are the
ones you meant to pair, then commit the credentials:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions/$ID/confirm \
  -H 'origin: https://phoebis.example'
```

`GET /admin/accounts` lists the slots and their states, and `GET /admin/accounts/{id}/usage` reports the account's
usage windows.

## Registry

Accounts, pools, and keys live in the store, and the admin API is where you manage them. Log in as above and keep the
cookie; every example below reuses it.

An `api_key` account carries its upstream key. The answer
carries the account as stored, never the key:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/accounts \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"id":"vendor","kind":"api_key","label":"Vendor","secret":"<upstream key>"}'
```

Creating the account also discovers the models it serves: `phoebis` asks the host for its model list with the key just
given. A key the host rejects refuses the creation. A host that serves no model list leaves the account ready, with
the error recorded and no models; `GET /admin/accounts/{id}/models` lists what discovery found, and a `PUT` on the
same path replaces the list, which is how you add such a host's models by hand. Discovery repeats every
`routing.catalog_sync_interval_seconds`.

A pool holds one or more accounts, each with a priority:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/pools \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"id":"main","label":"Main","strategy":"priority_random","members":[{"account_id":"vendor","priority":0}]}'
```

A downstream client presents a key, and a key routes into exactly one pool. Its secret is in the answer to the
creation and nowhere else, so store it when it appears; `POST /admin/keys/{id}/rotate` issues a new secret under the
same id and retires the old one:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/keys \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"id":"desktop","label":"Desktop","pool_id":"main"}'
```

## Routing

A pool picks the account for each request from its lowest priority band that still has an eligible account: an account
is eligible when it is enabled, ready, off cooldown, serves the requested model, and has not already been tried by this
request. Inside that band, `earliest_refresh` picks the account whose weekly window, the fable window for a fable model,
resets soonest, `least_utilized` picks the account with the most five-hour headroom left, and `priority_random` picks at
random. A pool that sets `select_below_utilization` keeps every account whose five-hour reading has reached that
threshold out of the selection set, for as long as one account is still under it.

A request that carries a session stays on the account that served it first, for as long as the pool's
`affinity_ttl_seconds`, so a conversation does not move mid-flight. `GET /admin/sessions` lists the live pins, and a
release drops one.

When nothing in the pool can serve the request, the answer says whether waiting helps. With every account rate
limited, `phoebis` answers 429 with `retry-after` and the unified rate-limit headers, shaped as a single rate-limited
account would answer, so the client's automatic continue arms on it. For anything only an operator can clear, such as
a pool whose accounts are all paused, it answers 503.

## Keepalive

A prompt cache entry expires five minutes, or one hour, after its last use. With `keepalive.enabled` on a pool, `phoebis`
keeps a replayable copy of every eligible request a session sends (adaptive or disabled thinking, no forced tool
choice, no structured output, no strict tools) and, shortly before the entry expires, sends a probe with `max_tokens`
0 and `stream` false through the same failover loop a real request takes: it follows the session's pin, renews it,
and migrates on a 429. A session that comes back after the gap then reads the cache it would otherwise have paid to
rebuild. Probes cost cache reads at list prices, and the ledger records what each kept-warm request saved against
that, per pool and per hour; `GET /admin/keepalive/overview` shows the net, which can go negative.

Enabling keepalive needs `affinity_ttl_seconds` of at least 300 on the pool, because a probe can only keep a session
warm on the account it is pinned to. Probes on `oauth` accounts stay off until `keepalive.oauth_prewarm_verified` is
set in the bootstrap file, which is the operator's statement that the expiry-crossing experiment passed for the
combinations in use. A key with `keepalive_opt_out` is never retained. The probe tuning (lead and jitter per TTL,
retry backoff, the idle limits, the cold-probe limit, the daily cost cap, the net-negative rule) is per pool under
`keepalive`; `GET /admin/pools` shows the full policy with defaults filled.

The engine learns which sessions are alive from the `cc-cache-keepalive` companion, a Claude Code hook plugin that
reports the session lifecycle to two routes on the proxy listener,
`GET /v0/resource/plugins/cache-keepalive/events` and `GET /v0/resource/plugins/cache-keepalive/snapshot`, under the
same key the client sends for model traffic. A session is scoped to the key that reports it. With the companion's
lease lapsed the session is probed under the pool's idle limits until they run out; a `session_end`, a replacement
registration, the session expiry, or a release through `POST /admin/sessions/{key}/{session}/release` discards its
recipes at once. `GET /admin/sessions` shows the companion state and the next probe per session, and
`GET /admin/keepalive/recipes` one row per retained recipe with why it is held.

The `keepalive` block of the bootstrap file holds the process-wide limits alone (the lease, the record and byte
bounds, the worker cap) and is read at start; a change to it takes a restart.

## Statistics

Every relayed request lands in the ledger at `stats.ledger_path`: one row for the request the client sent, with the
key, the model, the outcome, and the timings, and one row per upstream attempt it took, with the account, the outcome,
the token counts, and the time to first byte. Finished requests are folded into hourly rollups, and those rollups are
what the analytics routes report, so a long range costs one row per hour instead of one per request.

Quota samples arrive from two sources. Every response head of an OAuth account carries the unified rate-limit headers,
which cost nothing to read and cover whichever accounts are in use; the poll of the usage endpoint, every
`quota.poll_interval_seconds`, fills the gaps left by idle accounts and is the only source of extra-usage credit
totals.

Every attempt is stamped with a cost estimate at list prices per million tokens. It is an estimate rather than a bill:
tier and batch modifiers are not known here, and subscription quota is the rate-limit windows, never this number. The
shipped price table is used unless `stats.pricing_path` names a file, and a price import confirmed through the admin
API wins over both. An attempt on a model the table does not price carries no estimate.

Body capture is off until a key or a pool asks for it. A capture holds the request and response bodies of that key or
pool in the ledger for the hours it names, and each captured body is dropped when those hours run out.
`stats.body_capture_max_hours` is the ceiling on what a key or pool may ask for, because the bodies are prompts and
how long they are kept is the operator's call rather than the caller's.

Requests, attempts, and events are purged once they are older than `stats.retention_days`, quota samples once they are
older than `quota.sample_retention_days`, and the hourly rollups are kept for 400 days.

### Metrics

`stats.metrics_listen` serves `/metrics` in Prometheus text format and nothing else. It speaks plain HTTP and asks for
no credential, so bind it where only your scraper can reach it.

| Metric | Type | Labels |
| --- | --- | --- |
| `phoebis_account_cooldown_seconds` | gauge | `account`, `scope` |
| `phoebis_account_overage_in_use` | gauge | `account` |
| `phoebis_account_ratelimit_utilization` | gauge | `account`, `window` |
| `phoebis_attempts_total` | counter | `account`, `model`, `outcome` |
| `phoebis_bus_dropped_total` | counter | `subscriber` |
| `phoebis_bus_subscriber_panics_total` | counter | `subscriber` |
| `phoebis_keepalive_agents` | gauge | `state` |
| `phoebis_keepalive_probes_total` | counter | `account`, `outcome` |
| `phoebis_keepalive_saved_usd_total` | counter | `pool` |
| `phoebis_ledger_dropped_total` | counter | `table` |
| `phoebis_pins_active` | gauge | `pool` |
| `phoebis_pricing_unknown_model_total` | counter | none |
| `phoebis_requests_total` | counter | `key`, `kind`, `outcome` |
| `phoebis_tokens_total` | counter | `account`, `kind`, `model` |
| `phoebis_upstream_ttfb_seconds` | histogram | `model` |

The `model` label is bounded by the price table: a model the table prices keeps its id and every other model is
reported as `other`, so a downstream caller cannot grow the label set by naming models.

### Live feed

`GET /admin/live` upgrades to a WebSocket that reports requests while they run, and `GET /admin/live/snapshot` returns
the same entries for a past window. Both need an admin session like every other admin route. A session logged in with
the viewer secret sees no request or response preview on the WebSocket, and the snapshot carries no preview whatever
the role.

## Status

Nothing in this build has talked to Anthropic yet. The wire shapes come from a capture of Claude Code 2.1.269 on Linux
x64, and every test runs against a loopback fake upstream that replays those fixtures. A first live run against
`api.anthropic.com` is still ahead, and until it happens, treat the relayed shape as verified against the capture only.
