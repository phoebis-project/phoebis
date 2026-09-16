# phoebis

A local relay for Claude Code. A downstream client points at `phoebis` with a key the router issued; `phoebis` puts the request
on the wire to Anthropic, under credentials it holds itself. The `phoebis` process serves
two listeners: the proxy that relays `/v1/messages`, and the admin API that pairs accounts and reports usage.

## Build

```sh
cargo build --release
```

The binary is `target/release/phoebis`.

## Commands

| Command | Effect |
| --- | --- |
| `phoebis --config ./config.yaml` | Serves both listeners until Ctrl-C, then drains for up to 30 seconds. |
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

### Not yet configurable

Five groups of limits are compiled into the binary, and the file has no key for any of them:

| Limit | Value |
| --- | --- |
| Admin session lifetime | 15 minutes idle, 12 hours absolute. |
| Drain window on shutdown | 30 seconds for requests still in flight. |
| Relay waits | 600 seconds for a response head, 600 seconds between stream events, 32 MiB request cap. |
| Stream hold before commit | 30 seconds or 64 KiB, whichever comes first. |
| Upstream pool | 8 idle connections per account and host pair, discarded after 60 seconds idle. |

## Downstream client

```sh
ANTHROPIC_BASE_URL=https://<router host>
CLAUDE_CODE_OAUTH_TOKEN=<key secret>
_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
```

The third line is optional and switches the client to the full first-party request shape: two extra betas,
`x-client-request-id`, the `cch` billing header, `cc_prompt_id`, `diagnostics`, and the global cache scope. Without it
the client sends the reduced shape and `phoebis` relays that as it arrives. The variable is an internal one that a later
client build may drop.

Remote Control is unavailable behind the router with or without that override. The client reports that
`ANTHROPIC_BASE_URL` does not point at `api.anthropic.com`, and it states that the override does not apply to Remote
Control.

The proxy listener speaks plain HTTP, as the admin listener does: your reverse proxy terminates TLS in front of it,
and `ANTHROPIC_BASE_URL` points at that reverse proxy.

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
is eligible when it is enabled, ready, off cooldown, serves the requested model, and has not already been tried by
this request. Inside that band, `priority_random` picks at random and `least_utilized` picks the account with the most
five-hour headroom left.

A request that carries a session stays on the account that served it first, for as long as the pool's
`affinity_ttl_seconds`, so a conversation does not move mid-flight. `GET /admin/sessions` lists the live pins, and a
release drops one.

When nothing in the pool can serve the request, the answer says whether waiting helps. With every account rate
limited, `phoebis` answers 429 with `retry-after` and the unified rate-limit headers, shaped as a single rate-limited
account would answer, so the client's automatic continue arms on it. For anything only an operator can clear, such as
a pool whose accounts are all paused, it answers 503.

## Status

Nothing in this build has talked to Anthropic yet. The wire shapes come from a capture of Claude Code 2.1.269 on Linux
x64, and every test runs against a loopback fake upstream that replays those fixtures. A first live run against
`api.anthropic.com` is still ahead, and until it happens, treat the relayed shape as verified against the capture only.
