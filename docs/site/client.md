# The downstream client

Claude Code reaches the router through a handful of environment variables, two required and the rest optional.
This page gives the block, says what each line changes in the client's behaviour, and describes what the proxy
listener accepts from the client and what it refuses.

## The environment block

```sh
ANTHROPIC_BASE_URL=https://<router host>
CLAUDE_CODE_OAUTH_TOKEN=<key secret>
_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
DISABLE_TELEMETRY=1
```

### ANTHROPIC_BASE_URL

Points Claude Code at the router's proxy listener, through the reverse proxy that terminates TLS when there is
one. Every model request and the keepalive companion's reports go here. Plain HTTP off loopback is allowed, and
that is what the companion needs `CC_KEEPALIVE_ALLOW_PLAINTEXT=1` for (below).

### CLAUDE_CODE_OAUTH_TOKEN

Holds the router key's secret, the one shown once at creation or rotation. Put it here rather than in
`ANTHROPIC_API_KEY`: the client gates three behaviours on a subscriber check that an API key never passes, the
one-hour cache TTL for the main conversation, the parsing of the unified rate-limit headers on a 429, and the
automatic continue after a usage-limit reset. An `ANTHROPIC_API_KEY` gives all three up.

### `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`

Optional, and no longer needed for the wire shape. Set to `1`, it switches the client to the full first-party
request shape and switches off gateway model discovery. Without it the client sends the reduced shape, and the
router restores, toward `api.anthropic.com`, the fields the reduced shape lacks: the `cch` billing hash,
`x-client-request-id`, `cc_prev_req`, and `cc_prompt_id`, each only when absent, and the whole attribution line
when `CLAUDE_CODE_ATTRIBUTION_HEADER` switched it off. Two things are not restored, because the router cannot:
the `advanced-tool-use-2025-11-20` beta (the client has to run its tool search; `ENABLE_TOOL_SEARCH=true`
selects it) and the global cache scope on the static system block. The restoration applies to a request whose
`User-Agent` is `claude-cli/` and to the client's own inference requests;
a keepalive probe replays a request as the client sent it. The variable is an internal one that a
later client build may drop.

### DISABLE_TELEMETRY

Optional. Set to `1`, it switches off the client's event logger, which otherwise posts telemetry batches to
`api.anthropic.com` with the credential it was given, behind the router the key secret.
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` does the same and also stops update checks and the bootstrap lookup. The
policy and settings lookups the client makes at start still go to `api.anthropic.com` with the secret under either
setting; they fail there and the client tolerates that.

### For the keepalive companion

`CC_KEEPALIVE_ALLOW_PLAINTEXT=1` is needed on a LAN where the router speaks plain HTTP off loopback: the
companion refuses to report session identifiers over `http://` to a non-loopback host without it.

`CC_KEEPALIVE_MANAGEMENT_URL` and `CC_KEEPALIVE_TOKEN` are not needed; the companion defaults them to
`ANTHROPIC_BASE_URL` and `CLAUDE_CODE_OAUTH_TOKEN`.

The optional owner binding, `CC_KEEPALIVE_OWNER_ID` paired with `ANTHROPIC_CUSTOM_HEADERS="X-Keepalive-Owner-Id:
$CC_KEEPALIVE_OWNER_ID"`, hardens what the router already fences: cache warming is fenced by the companion's own
registration, and this pair binds each request to the process that registered, so a second process resuming the same
session id cannot inherit its recipes. When both sides name an owner they must agree. It needs a wrapper script, since
the settings file's environment block holds only static values.

`CLAUDE_CODE_PROMPT_CACHE_TTL=5m` is worth setting for Fable 5.1 sessions in a pool with cache warming on: at a
0.025x read price, a five-minute entry kept warm by probes stays cheaper than a one-hour entry. Leave it unset
without warming. Through a pool with a cache TTL policy the variable has no effect; the pool decides. See
[Keepalive](keepalive.md).

## What the proxy accepts

| Method | Path | Credential | Answer |
| --- | --- | --- | --- |
| HEAD | `/api/hello` | none | `200 OK` |
| POST | `/v1/messages` | router key | relayed to the selected account |
| POST | `/v1/messages/count_tokens` | router key | relayed, same failover loop |
| GET | `/v1/usage` | router key | that key's own usage for the last 24 hours and 7 days, JSON or HTML by `Accept` |
| GET | `/v1/models` | none | `404` |
| GET | `/v1/models/{id}` | none | `404` |
| GET | `/v0/resource/plugins/cache-keepalive/events` | router key | the companion's event report |
| GET | `/v0/resource/plugins/cache-keepalive/snapshot` | router key | the companion's engine snapshot |
| any other method on a known path | | none | `405` |
| any unmatched path | | none | `404` |

The client probes `/api/hello` before it sends anything, and ignores the answer. The two `/v1/models` routes
refuse on purpose: answering them would let a client pick a model from an account's entitlements rather than
from its own configuration. Every refusal above, including the wrong-method and unknown-path answers, carries an
Anthropic-shaped error body. The two keepalive companion routes are covered on the [Keepalive](keepalive.md)
page.

The key is presented as exactly one of `Authorization: Bearer <secret>` or `x-api-key: <secret>`. None, more
than one, or a malformed header is refused, each in an Anthropic-shaped error body:

- more than one credential header was presented
- no credential header was presented
- the credential header is malformed
- the presented key is not in the registry

A well-formed but unrecognized key is refused too, again in an Anthropic-shaped error body:

- the key has expired
- the key is disabled
- the key is not in the registry
- the key was revoked
- the key's pool does not exist
- the key's pool is disabled

## Your key's usage

`GET /v1/usage` on the proxy listener answers the usage of the key that presents it, and of no other: requests, tokens,
and the estimated cost over the last 24 hours and the last 7 days, per model, with the key's label and pool. The figures
are the router's hourly rollups under the key, probes left out, the same rollups the operator's analytics read. Each
window is summed from whole hourly buckets, so it starts at the top of an hour: the `from` the answer reports is that
hour, up to an hour earlier than the window's name, and the figures cover it all. A key that is the first holder of its
id reads the whole window, its own first hour included. Where a deleted key used the same id before this one, the window
starts no earlier than the first whole hour after this key was created, so nothing the earlier holder spent is counted.
Nothing about the upstream accounts is answered, and the secret never is.

```sh title="The document"
curl -sS -H "Authorization: Bearer $CLAUDE_CODE_OAUTH_TOKEN" https://<router host>/v1/usage | jq .windows.\"24h\"
```

With `Accept` preferring `text/html` (listed alone, or weighted above `application/json` as a browser sends
it), the same route answers one self-contained page, so a browser extension or a `curl -H 'Accept: text/html'`
saved to a file shows the figures without the console and without an admin login. A wrong or missing key is
refused exactly as `/v1/messages` refuses it, in the Anthropic error shape.

## Headers and transport

Request headers travel as the client sent them, in the client's own order and spelling. The router drops the
hop-by-hop and forwarding headers, puts the account's credential where the client put the key, and adjusts
`anthropic-beta` to what the account may claim. It owns `Host` and `Content-Length`, and adds nothing for a
header the client left out, so any Claude Code version relays on its own measured shape rather than a guessed
one.

Remote Control is unavailable behind the router. The client reports that `ANTHROPIC_BASE_URL` does not point at
`api.anthropic.com`, and states that the override does not apply to Remote Control.

The proxy listener speaks HTTP/1.1 and no HTTP/2: a header's order and spelling do not survive an h2 frame, and
the request would reach the upstream in a shape the client never sent.
