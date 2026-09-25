# The downstream client

Claude Code reaches the router through a handful of environment variables, two required and the rest optional.
This page gives the block, says what each line changes in the client's behaviour, and describes what the proxy
listener accepts from the client and what it refuses.

## The environment block

```sh
ANTHROPIC_BASE_URL=https://<router host>
CLAUDE_CODE_OAUTH_TOKEN=<key secret>
_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
CLAUDE_CODE_GATEWAY_HINT_HEADERS=1
DISABLE_TELEMETRY=1
```

### ANTHROPIC_BASE_URL

Points Claude Code at the router's proxy listener, through the reverse proxy that terminates TLS when there is
one. Every model request and the keepalive companion's reports go here. Plain HTTP is accepted on any address, by
the router and by the companion alike.

### CLAUDE_CODE_OAUTH_TOKEN

Holds the router key's secret, the one shown once at creation or rotation. Put it here rather than in
`ANTHROPIC_API_KEY`: the client gates three behaviours on a subscriber check that an API key never passes, the
one-hour cache TTL for the main conversation, the parsing of the unified rate-limit headers on a 429, and the
automatic continue after a usage-limit reset. An `ANTHROPIC_API_KEY` gives all three up.

### `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`

Optional, and worth setting. Under it the client sends the full first-party shape: the attribution line with every
field, `x-client-request-id`, and the gateway hint headers, all by default, so the router restores nothing and the
request log reads the turn origin and the hints as the client sent them. It also stops the client reading the router's
models listing, which it otherwise uses to size its context window by the id. Without it the client sends the reduced
shape, and the router restores, toward `api.anthropic.com`, the fields the reduced shape lacks on the attribution line
the client sent: the `cch` billing hash, `x-client-request-id`, `cc_prev_req`, and `cc_prompt_id`, each only when
absent. A request that carries no attribution line, as under `CLAUDE_CODE_ATTRIBUTION_HEADER=0`, is relayed as sent: the
router never writes a line the client did not. The auto-mode classifier requests are the client's own: they keep the
attribution block, the router fills them like any other line, and they carry the `auxiliary` request class in
`x-claude-code-request-class`. The router reads the header on every request: a main, subagent or compaction class keeps
the session's pin warm and re-arms its keepalive recipe, and any other class does neither. The previous request id and
the prompt id come from the router's session memory, which survives a restart: they are kept on disk for
`sessions.memory_retention_days` ([installation](installation.md#sessions)). Three things are not restored, because the
router cannot: the `advanced-tool-use-2025-11-20` beta (the client has to run its tool search; `ENABLE_TOOL_SEARCH=true`
selects it), the global cache scope on the static system block, and `cc_turn_origin`, who started the turn, which the
router records when the line carries it and never writes. The restoration applies to a request whose `User-Agent` is
`claude-cli/` followed by a version the client itself would spell (digits and dots, optionally `-` and a prerelease
suffix of letters, digits, dots, and hyphens) and to the client's own inference requests;
a keepalive probe replays a request as the client sent it. Two self-checks guard the
restoration against a client build the router was not pinned to: a stamped `cch` hash the router cannot reproduce, and a
`cc_version` suffix that does not reproduce from the prompt and the client's version. Either refuses the request as
client drift before anything is sent, and the request log names which (`hash` or `version_suffix`). The variable is an
internal one that a later client build may drop.

The router also reads two cumulative boundary-hash chains off every request's body: a static chain over the
tools array and the system blocks, and a message chain with one boundary per entry of `messages`, both computed
on the client's own bytes with every `cache_control` marker removed. Neither chain gates the request; both ride
with it on the observation and the selection, for the session's pin to compare its own hashes against a later
request's and tell a continuation from a restart, and for the keepalive guard.

### CLAUDE_CODE_GATEWAY_HINT_HEADERS

Optional, and worth setting in the reduced shape; under `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1` the headers
arrive by default. Set to `1`, it makes the client send its gateway hint headers on a custom base URL:
`x-claude-code-agent-type` (the subagent's type on its own turns), `x-claude-code-compaction` (on the summarization
request), `x-claude-code-context-compacted` (once, on the first request after a compaction), and
`x-claude-code-prev-tool-durations` (the tool calls of the turn before and their milliseconds). The router reads them
for the request log and the analytics and nothing else: the row carries each value as sent, the tool durations as
their sum, and the drawer shows them ([statistics](statistics.md#requests)). Every header is optional; a request
without one is relayed the same way. `0` stops them on every connection.

### DISABLE_TELEMETRY

Optional. Set to `1`, it switches off the client's event logger, which otherwise posts telemetry batches to
`api.anthropic.com` with the credential it was given, behind the router the key secret.
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` does the same and also stops update checks and the bootstrap lookup. The
policy and settings lookups the client makes at start still go to `api.anthropic.com` with the secret under either
setting; they fail there and the client tolerates that.

### For the keepalive companion

`CC_KEEPALIVE_MANAGEMENT_URL` and `CC_KEEPALIVE_TOKEN` are not needed; the companion defaults them to
`ANTHROPIC_BASE_URL` and `CLAUDE_CODE_OAUTH_TOKEN`.

`CLAUDE_CODE_PROMPT_CACHE_TTL=5m` is worth setting for Fable 5.1 sessions in a pool with cache warming on: at a
0.025x read price, a five-minute entry kept warm by probes stays cheaper than a one-hour entry. Leave it unset
without warming. Through a pool with a cache TTL policy the variable has no effect; the pool decides. See
[Keepalive](keepalive.md).

### Optional: bind a session to its process

The optional owner binding, `CC_KEEPALIVE_OWNER_ID` paired with `ANTHROPIC_CUSTOM_HEADERS="X-Keepalive-Owner-Id:
$CC_KEEPALIVE_OWNER_ID"`, hardens what the router already fences: cache warming is fenced by the companion's own
registration, and this pair binds each request to the process that registered, so a second process resuming the same
session id cannot inherit its recipes. When both sides name an owner they must agree. It needs a wrapper script, since
the settings file's environment block holds only static values.

## What the proxy accepts

| Method | Path | Credential | Answer |
| --- | --- | --- | --- |
| HEAD | `/api/hello` | none | `200 OK` |
| POST | `/v1/messages` | router key | relayed to the selected account |
| POST | `/v1/messages/count_tokens` | router key | relayed, same failover loop; its markers take the pool's TTL |
| GET | `/v1/usage` | router key | that key's own usage for the last 24 hours and 7 days, JSON or HTML by `Accept` |
| GET | `/v1/models` | router key | the models the key may route to, with a `[1m]` twin per long-context model |
| GET | `/v1/models/{*id}` | router key | one row of that listing, or `404` |
| GET | `/v0/resource/plugins/cache-keepalive/events` | router key | the companion's event report |
| GET | `/v0/resource/plugins/cache-keepalive/snapshot` | router key | the companion's engine snapshot |
| any other method on a known path | | none | `405` |
| any unmatched path | | none | `404` |

The client probes `/api/hello` before it sends anything, and ignores the answer. The two `/v1/models` routes
answer the [models listing](#the-models-listing) below. Every refusal above, including the wrong-method and
unknown-path answers, carries an Anthropic-shaped error body. The two keepalive companion routes are covered on
the [Keepalive](keepalive.md) page; each session they report belongs to the key generation that registered it,
so a key id created again reaches none of the sessions its previous holder left behind.

A token count (`POST /v1/messages/count_tokens`) is relayed through the same selection and failover loop as a
message, and the `cache_control` markers it carries take the pool's cache TTL policy the way a message's do:
each one is rewritten to the policy's TTL, the extended-TTL beta travels with a body the rewrite made an hour,
and the `cch` stamp is recomputed where one is present. A count that carries no marker travels as sent, and the
top-level member a message with no marker is given is not added to it. Nothing else in the body is added to a
count either: the client sends no billing line or account metadata on one, and the cache-diagnosis member has
no place in it.

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
- the key was replaced under its id (a keepalive probe whose key was deleted and created again under the same
  id)
- the key was revoked
- the key's pool does not exist
- the key's pool is disabled

A request body whose top-level object carries `model`, `metadata` or `stream` more than once, raw or escaped, is
refused before anything reaches the upstream: `400` with the error code `body_shape` and a message naming the
member. A body whose top-level object nests deeper than the relay reads is refused `400` with the error code
`invalid_body`.

A request body that does not finish arriving within the compiled-in wait after authentication is refused `408`
with the error code `request_timeout` and the message `the request body did not arrive in time`.

A caller that stops sending before its body is complete is refused `400` with the error code
`request_incomplete` and the message `the request body ended before it was complete`.

## Your key's usage

`GET /v1/usage` on the proxy listener answers the usage of the key that presents it, and of no other: requests, tokens,
and the estimated cost over the last 24 hours and the last 7 days, per model, with the key's label and pool. The figures
are the router's hourly rollups under the key, probes left out, the same rollups the operator's analytics read. Each
window is summed from whole hourly buckets, so it starts at the top of an hour: the `from` the answer reports is that
hour, up to an hour earlier than the window's name, and the figures cover it all. The figures are this key's own from
its creation: a deleted key's usage stays under the key it belonged to, so a later key under the same id never reads it,
and each window starts where its name says. Nothing about the upstream accounts is answered, and the secret never is.

```sh title="The document"
curl -sS -H "Authorization: Bearer $CLAUDE_CODE_OAUTH_TOKEN" https://<router host>/v1/usage | jq .windows.\"24h\"
```

With `Accept` preferring `text/html` (listed alone, or weighted above `application/json` as a browser sends
it), the same route answers one self-contained page, so a browser extension or a `curl -H 'Accept: text/html'`
saved to a file shows the figures without the console and without an admin login. A wrong or missing key is
refused exactly as `/v1/messages` refuses it, in the Anthropic error shape.

## The models listing

`GET /v1/models` on the proxy listener answers the key that presents it with the models it may route to: every enabled
upstream of the key's pool is asked for its catalog through the router's cache ([routing](routing.md#catalogs)),
the same cache the router derives a model's dated id from, so a request naming an undated id is routed,
priced and recorded under the newest dated snapshot the listing holds for it; the pool's and the key's [model
rules](routing.md#model-rules) are applied to each id, one row is kept per id (the first upstream in priority
order that lists it), and the rows are sorted by id. The shape is the API's own: `data[]` of `id`, `type`,
`display_name`, `created_at`, `max_input_tokens`, and `max_tokens` as the upstream reports them, with `first_id`,
`last_id`, and `has_more`; `limit` (1 to 1000, default 20), `after_id`, and `before_id` page it, and a `limit`
outside its range is refused `400 invalid_limit`. An upstream whose fetch failed contributes nothing until the
next fetch, and a model that is cooling down on an upstream still lists. `GET /v1/models/{id}` answers the one row,
or `404 not_found_error` with the message `model: {id}`; an id with a slash may be sent as is or percent-encoded.

Beside every model whose `max_input_tokens` is above 200,000 the listing carries a twin whose id ends in `[1m]`
and whose display name ends in " (1M Context)", with the same figures. Claude Code behind a gateway sizes its
context window by the id, 200K unless it ends in `[1m]`, so the twin is what lets it use the window the
upstream reports; a request that names the twin is routed as the model itself and the suffix never reaches the
upstream. The twins are on by default and switched off per key with `long_context_twins`.

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
