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
CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1
```

### ANTHROPIC_BASE_URL

Points Claude Code at the router's proxy listener, through the reverse proxy that terminates TLS when there is
one. Every model request and the keepalive companion's reports go here. Plain HTTP is accepted on any address, by
the router and by the companion alike.

### CLAUDE_CODE_OAUTH_TOKEN

Holds the router key's secret, the one shown once at creation or rotation. Put it here rather than in
`ANTHROPIC_API_KEY`: the client gates three behaviours on a subscriber check that a credential in `ANTHROPIC_API_KEY`
never passes, the one-hour cache TTL for the main conversation, the parsing of the unified rate-limit headers on a 429,
and the automatic continue after a usage-limit reset. An `ANTHROPIC_API_KEY` gives all three up.

### `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`

Optional, and worth setting. Under it the client sends the full first-party shape: the attribution line with every
field, `x-client-request-id`, and the gateway hint headers, all by default, so the router restores nothing and the
request log reads the turn origin and the hints as the client sent them. It also stops the client reading the router's
models listing, which it otherwise uses to size its context window by the id. Without it the client sends the reduced
shape, and the router restores, toward `api.anthropic.com`, the fields the reduced shape lacks on the attribution line
the client sent: the `cch` billing hash, `x-client-request-id`, `cc_prev_req`, and `cc_prompt_id`, each only when
absent. A request that carries no attribution line, as under `CLAUDE_CODE_ATTRIBUTION_HEADER=0`, is relayed as sent: the
router never writes a line the client did not. The auto-mode classifier requests are the client's own: they keep the
attribution block and the router fills them like any other line; in first-party mode only they carry the
`auto-mode-classifier-2026-07-16` beta and the `auxiliary` request class in `x-claude-code-request-class`, and behind a
plain base URL neither appears, so the router records and pins them as ordinary requests of their session ([Auto mode
behind the router](#auto-mode-behind-the-router)). The router reads the class header whenever the client sends it; the
other hints are under [CLAUDE_CODE_GATEWAY_HINT_HEADERS](#claude_code_gateway_hint_headers). A main, subagent or
compaction class keeps the session's pin warm and re-arms its keepalive recipe, and any other class does neither. The
previous request id and the prompt id come from the router's session memory, keyed on the client's own session id,
and the memory survives a restart: it is kept on disk for
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
for the request log and the analytics: the row carries each value as sent, the tool durations as their sum, and the
drawer shows them ([statistics](statistics.md#requests)). `x-claude-code-context-compacted` also reaches routing: the
session's pin reads cold on the request that carries it, so a move that waits for a cold cache may take the session and
a shedding round orders it first ([Moves](routing.md#moves)); the other three change nothing about where a request
lands. Every header is optional; a request without one is relayed the same way. `0` stops them on every connection.

### DISABLE_TELEMETRY

Optional. Set to `1`, it switches off the client's event logger, which otherwise posts telemetry batches to
`api.anthropic.com` with the credential it was given, behind the router the key secret.
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` does the same and also stops update checks and the bootstrap lookup. The
policy and settings lookups the client makes at start still go to `api.anthropic.com` with the secret under either
setting; they fail there and the client tolerates that.

### CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK

Optional; needed to use fast mode behind the router. The client checks whether fast mode is available for the
organization by asking `api.anthropic.com` directly with the session's credential, ignoring `ANTHROPIC_BASE_URL`;
with a router key that check fails as a connectivity error and fast mode stays off. With this variable set the
client skips the check and sends `speed: "fast"` when you switch fast mode on. The router reads the speed from
the request and the answer: a fast request is served on an account whose fast lane is open, priced in dollars
from usage credits, and moved to a sibling when an account's fast lane cools (see
[Cooldowns and failover](routing.md#cooldowns-and-failover)); it draws no plan window.

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

## Auto mode behind the router

Claude Code in auto mode reviews each tool use before it runs. Behind the router, with or without
`_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL` and in `-p` mode alike, the client asks the upstream to do that review on
the model's own turn: every session post carries a `safeguards` member near the end of its body (the machine's
permission rules with their literal text, the working and home directories, the trusted directories, the git state
and the user identity) and the `dangerous-tool-use-2026-09-03` beta, and the answer's `message_delta` carries one
verdict per tool use. A Max account answered a verdict for every reviewed tool use through a gateway that relays the
member unchanged, as the router does with the setting on; an Anthropic-key account has not been measured. A session post
that asks takes two to four seconds against one to two for one that does not, and the client's own classifier request
about 1.3 seconds.

What the router does with that request is the pool's **Support server-side auto mode** setting, on by default,
which a key can override ([routing](routing.md#pools)):

- On, the router passes the member, the beta and the verdict through unchanged, and the request log and the drawer
  record the request as a server-side check ([statistics](statistics.md#requests)).
- Off, the router drops the member and the beta from every request of the key and restamps a first-party body's
  billing hash. The upstream then evaluates nothing, and one completed answer without a verdict latches the client
  for the rest of its process, subagents included, across `/clear` and `/compact`: from then on it sends one
  classifier request of its own per reviewed tool use, on its Sonnet default rather than the session's model, about
  52,000 cached input tokens each, billed to the account's windows and absent from the client's own cost line, while
  the beta stays in its header and the router keeps dropping it. The first session post under the off state still
  asks, so a session that starts there costs one asking round. The request log records each request whose member the
  router dropped as `stripped`, and the client's classifier requests as ordinary requests of their session
  ([statistics](statistics.md#requests)).

`CLAUDE_CODE_AUTO_MODE_SERVER=0` in the client's environment is the client-side equivalent of the off state, with no
first asking round; `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` does the same in first-party mode.

The client's own classifier runs in two stages: a stage-1 request that stops at `</block>` and answers `<block>no` or
`<block>yes`, and, after a `yes` or an unreadable answer, a stage-2 request with no stop sequence that reasons in
`<thinking>` before its `<block>` answer, with an optional `<reason>` and `<category>`. A stage-1 block followed by a
refused stage 2 stands as a block; both stage 2 requests measured through that gateway were refused with the category
`reasoning_extraction`, by it or by the upstream; a direct run settles which. These requests carry no beta of their own
behind a plain base URL; in first-party mode they add `auto-mode-classifier-2026-07-16` and the `auxiliary` request
class. Their bodies carry no `stream` and no `tools`, a security-monitor system block, and `stop_sequences` at stage 1.
They carry the session's own id and pin like any request, on the Sonnet default, so a session on another model takes a
second pin beside its own.

**The notice.** The client can show a one-line notice that its classifier requests are billed because the gateway
does not support the server-side check. A Pro, Max or Team login never sees it; a key or token login sees it once per
process after the latch, naming the router's `host:port`, and acknowledges it for 24 hours. The text, from the
client's own strings, since no run through the router has shown it: "We're changing auto mode to no longer charge
for classifier requests in Claude Code. However, this session isn't eligible because your requests go through
`<host>`, which isn't compatible with this update. Nothing breaks: auto mode keeps working, and its classifier
requests are billed as before. To fix it and access the new version of auto mode, ask your gateway to implement:
`<url>`".

The router never adds the member or the beta to a request that did not carry them, and never answers a verdict itself; a
body it cannot cut under an off setting, or whose billing stamp it cannot restamp after the cut, is refused as [What the
proxy accepts](#what-the-proxy-accepts) says.

## What the proxy accepts

| Method | Path | Credential | Answer |
| --- | --- | --- | --- |
| HEAD | `/api/hello` | none | `200 OK` |
| HEAD | `/api/ready` | none | `200` while the router loop, the ledger consumer and the store are alive, else `503` |
| GET | `/api/ready` | none | the same status, with `{"ready": true}` or `{"ready": false, "dead": [...]}` |
| POST | `/v1/messages` | router key | relayed to the selected account |
| POST | `/v1/messages/count_tokens` | router key | relayed, same failover loop; travels as sent |
| GET | `/v1/usage` | router key | that key's own usage for the last 24 hours and 7 days, JSON or HTML by `Accept` |
| GET | `/v1/models` | router key | the models the key may route to, with a `[1m]` twin per long-context model |
| GET | `/v1/models/{*id}` | router key | one row of that listing, or `404` |
| GET | `/v0/resource/plugins/cache-keepalive/events` | router key | the companion's event report |
| GET | `/v0/resource/plugins/cache-keepalive/snapshot` | router key | the companion's engine snapshot |
| any other method on a known path | | none | `405` |
| any unmatched path | | none | `404` |

The query string of a request to either Messages route travels as the client sent it, `?beta=true` included; the router
routes on the path alone.

The client probes `/api/hello` before it sends anything, and ignores the answer. `phoebis health` and the image's health
check read `/api/ready` instead: `/api/hello` and `/api/ready` differ when the router loop, the ledger consumer or the
store died while the listener stays up, and `dead` names each one that died as `ledger`, `router` or `store`. The two
`/v1/models` routes answer the [models listing](#the-models-listing) below. Every refusal above, including the
wrong-method and unknown-path answers, carries an Anthropic-shaped error body; the not-ready `503` of `/api/ready`
reports the router's state and carries the readiness body instead. The two keepalive companion routes are covered on the
[Keepalive](keepalive.md) page; each session they report belongs to the key generation that registered it, so a key id
created again reaches none of the sessions its previous holder left behind.

pool's members are unusable for the moment, and the answer when an account's authentication failure is the request's
last word: the router never forwards an upstream `401`, and the answer names no account and no reason.

A token count (`POST /v1/messages/count_tokens`) is relayed through the same selection and failover loop as a
message and travels as the client sent it under every policy: the markers it carries keep their TTLs, no
member is added to a marker-less one, no beta follows it, and nothing else in the body is added (the client
sends no billing line or account metadata on one, and the cache-diagnosis member has no place in it).

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
- the key was replaced under its id
- the key was revoked
- the key's pool does not exist
- the key's pool is disabled

The same answer meets a request or a keepalive probe whose key was deleted, or deleted and created again under its
id, between admission and routing.

A request body whose top-level object carries `model`, `metadata` or `stream` more than once, raw or escaped, whose
`safeguards` member cannot be cut while its key's server-side auto mode is off (carried twice, or spelled with an
escape), whose `cache_control` marker the pool's cache TTL policy cannot reach (a key on the marker's path spelled with
an escape), or whose billing stamp the router cannot restamp after an edit (a key on its path, the billing line's header
name or its `cch=` spelled with an escape, or a key on its path carried more than once), is refused before anything
reaches the upstream: `400` with the error code `body_shape` and a message naming the member. A body whose top-level
object nests deeper than the relay reads is refused `400` with the error code `invalid_body`.

A request body that does not finish arriving within `proxy.body_read_seconds` (60 by default) after authentication is
refused `408` with the error code `request_timeout` and the message `the request body did not arrive in time`.

A caller that stops sending before its body is complete is refused `400` with the error code
`request_incomplete` and the message `the request body ended before it was complete`.

A Messages request on a key that already has its `in_flight_cap` of requests in flight is refused before it reaches any
account: `429` with the error type `rate_limit_error`, the error code `key_in_flight_cap`, and a `Retry-After` of the
pool's `retry_in_place_seconds`, one second when that is 0. A request counts from its admission until its response ends,
a stream for its whole length. A `count_tokens` request takes no slot on the key or on the account and is never held or
refused for want of one.

A request body larger than `proxy.max_request_bytes` (32 MiB by default), declared or arriving, is refused `413` with
the error code `request_too_large`. A request whose body would take the router past `proxy.body_budget_bytes` of bodies
in flight is refused `503` `api_error` with the error code `body_budget_exhausted` and the message `the router holds as
many request bytes as it allows`, with no `Retry-After`; a request past the key's bound on bodies arriving at once
(`proxy.key_body_reads`, or the key's own `in_flight_cap` when set) is refused `429 rate_limit_error` `key_body_reads`
with `Retry-After: 1`. Each of these, the key cap's `429` included, is refused before its body is read and observed
without one.

### How a request is identified

A conversation is identified by the `x-claude-code-session-id` header.
A key's `affinity_source` can fix one of these instead of the router walking the whole
list. Every one of these hints is optional: a client that sends none of them is still relayed.
A client that forks a session under a new id, whose messages
continue an earlier conversation of the same key, keeps the new id as that session's own identity; the fork is
recorded beside it, visible in the console, without moving the new session off the account its own history names.

### The per-turn effort form

On a model the client marks for it, the client sends `per-turn-control-2026-07-01` on `anthropic-beta` and
carries the effort of each turn in a `role: "system"` entry appended to `messages`, beside the top-level
`output_config.effort`. The router relays both as sent and never rewrites one form into the other. It reads the
last such entry's level as the request's effort, treats the turn before the entry as the user's turn, and treats
the entry as one more message of the conversation, so an effort change under this form keeps the session's cache
lane; on a model the client sends no such entry for, the effort is part of the cache lane and a change of it
reads the session's prefix as cold (see [Selection](routing.md#selection)).

## Your key's usage

`GET /v1/usage` on the proxy listener answers the usage of the key that presents it, and of no other: requests, tokens,
and the estimated cost over the last 24 hours and the last 7 days, per model, with the key's label and pool. The figures
are the router's hourly rollups under the key, probes left out, the same rollups the operator's analytics read. Each
window is summed from whole hourly buckets, so it starts at the top of an hour: the `from` the answer reports is that
hour, up to an hour earlier than the window's name, and the figures cover it all. The figures are this key's own from
its creation: a deleted key's usage stays under the key it belonged to, so a later key under the same id never reads it,
and each window starts where its name says. Nothing about the accounts is answered, and the secret never is.

```sh title="The document"
curl -sS -H "Authorization: Bearer $CLAUDE_CODE_OAUTH_TOKEN" https://<router host>/v1/usage | jq .windows.\"24h\"
```

With `Accept` preferring `text/html` (listed alone, or weighted above `application/json` as a browser sends
it), the same route answers one self-contained page, so a browser extension or a `curl -H 'Accept: text/html'`
saved to a file shows the figures without the console and without an admin login. A wrong or missing key is
refused exactly as `/v1/messages` refuses it, in the Anthropic error shape.

## The models listing

`GET /v1/models` on the proxy listener answers the key that presents it with the models it may route to: every enabled
account of the key's pool is asked for its catalog through the router's cache ([routing](routing.md#catalogs)),
the same cache the router derives a model's dated id from, so a request naming an undated id is routed,
priced and recorded under the newest dated snapshot the listing holds for it; the pool's and the key's [model
rules](routing.md#model-rules) are applied to each id, one row is kept per id (the first account in priority
order that lists it), and the rows are sorted by id. The shape is the API's own: `data[]` of `id`, `type`,
`display_name`, `created_at`, `max_input_tokens`, and `max_tokens` as the upstream reports them, with `first_id`,
`last_id`, and `has_more`; `limit` (1 to 1000, default 20), `after_id`, and `before_id` page it, and a `limit`
outside its range is refused `400 invalid_limit`. An account whose fetch failed contributes nothing until the
next fetch, and a model that is cooling down on an account still lists. `GET /v1/models/{id}` answers the one row,
or `404 not_found_error` with the message `model: {id}`; an id with a slash may be sent as is or percent-encoded.

Beside every model whose `max_input_tokens` is above 200,000 the listing carries a twin whose id ends in `[1m]`
and whose display name ends in " (1M Context)", with the same figures. Claude Code behind a gateway sizes its
context window by the id, 200K unless it ends in `[1m]`, so the twin is what lets it use the window the
upstream reports; a request that names the twin is routed as the model itself and the suffix never reaches the
upstream. The twins are on by default and switched off per key with `long_context_twins`.

## Headers and transport

Request headers travel as the client sent them, in the client's own order and spelling. The router drops the hop-by-hop
and forwarding headers, puts the account's credential where the client put the key, and adjusts `anthropic-beta` to what
the account may claim. It owns `Host` and `Content-Length`, and adds nothing for a header the client left out beyond the
restored `x-client-request-id` and an `anthropic-beta` line for a beta the router injects, so any Claude Code version
relays on its own measured shape rather than a guessed one.

Remote Control is unavailable behind the router. The client reports that `ANTHROPIC_BASE_URL` does not point at
`api.anthropic.com`, and states that the override does not apply to Remote Control.

The proxy listener speaks HTTP/1.1 and no HTTP/2: a header's order and spelling do not survive an h2 frame, and
the request would reach the upstream in a shape the client never sent.
