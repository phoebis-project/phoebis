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
| `phoebis check-config --config ./config.yaml` | Validates the file, prints its accounts and routes, and exits. |
| `phoebis --version` | Prints the version. |

`--config` defaults to `./config.yaml`. Copy `deploy/config.example.yaml` and edit it. `check-config` opens neither the
store nor a socket, so it is safe to run against a production file while the router is up.

## Environment variables

The configuration file names environment variables; it never holds a secret. `phoebis` resolves them at startup and refuses
to start when one is unset or empty, naming the variable and nothing else. Every route secret and both admin secrets
have to differ from each other: two routes sharing a secret would collapse into one key-table entry, and an admin
secret that doubles as a route secret would turn an inference key into an admin login.

| Variable in the example file | Named by | Holds |
| --- | --- | --- |
| `ROUTER_ADMIN_SECRET` | `admin.secret_env` | The admin login, which reaches every admin route. |
| `ROUTER_CONSOLE_KEY` | `accounts[].secret_env` | The Anthropic API key of an `api_key` account. |
| `ROUTER_DESKTOP_KEY` | `routes[].secret_env` | The key a downstream client presents to the proxy. |
| `ROUTER_VIEWER_SECRET` | `admin.viewer_secret_env` | The read-only admin login. Optional. |

Pick your own names; those four are what the example file happens to use. `RUST_LOG` sets the log filter and defaults
to `info`. The storage encryption key is a file rather than a variable: `storage.encryption_key_file` points at 32 raw
bytes or 64 hex characters, and the database beside it is useless without that file.

## Downstream client

```sh
ANTHROPIC_BASE_URL=https://<router host>
CLAUDE_CODE_OAUTH_TOKEN=<route secret>
_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
```

The third line is optional and switches the client to the full first-party request shape: two extra betas,
`x-client-request-id`, the `cch` billing header, `cc_prompt_id`, `diagnostics`, and the global cache scope. Without it
the client sends the reduced shape and `phoebis` relays that as it arrives. The variable is an internal one that a later
client build may drop.

Remote Control is unavailable behind the router with or without that override. The client reports that
`ANTHROPIC_BASE_URL` does not point at `api.anthropic.com`, and it states that the override does not apply to Remote
Control.

Put the route secret in `CLAUDE_CODE_OAUTH_TOKEN`, not in `ANTHROPIC_API_KEY`. The client gates three behaviors on a
subscriber check that an API key never passes: the one-hour cache TTL for the main conversation, the parsing of the
unified rate-limit headers on a 429, and the automatic continue after a usage-limit reset. A downstream
`ANTHROPIC_API_KEY` gives all three up.

## Pairing an account

An `api_key` account needs no pairing; its key comes from the environment at startup. An `oauth` account is paired
through the admin API, which hands the operator an authorization URL and takes the result back.

Every admin mutation carries the `Origin` header the file configures in `admin.origin`, except under
`dev_loopback_http`, where `phoebis` pins no origin. Log in first and keep the cookie:

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

Open the `authorization_url` in a browser and approve. The provider sends the browser to the redirect URI with a code;
paste that whole URL back:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions/$ID/callback \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"url":"https://phoebis.example/callback?code=...&state=..."}'
```

`phoebis` exchanges the code and answers with the upstream account and organization it reached. Check that they are the
ones you meant to pair, then commit the credentials:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/oauth/sessions/$ID/confirm \
  -H 'origin: https://phoebis.example'
```

`GET /admin/accounts` lists the slots and their states, and `GET /admin/accounts/{id}/usage` reports the account's
usage windows.

## Status

Nothing in this build has talked to Anthropic yet. The wire shapes come from a capture of Claude Code 2.1.269 on Linux
x64, and every test runs against a loopback fake upstream that replays those fixtures. A first live run against
`api.anthropic.com` is still ahead, and until it happens, treat the relayed shape as verified against the capture only.
