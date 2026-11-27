# The client

You are pointing Claude Code at the router, or you hold a key and want to read what it is doing. This page gives the
block the Settings view renders and what each of its lines changes in the client, what the router requires of a Claude
Code request, the first request, what the proxy listener accepts and the codes it refuses with, your key's usage page,
the models listing, and what the client does while it waits on a slow answer.

## The environment block

```sh
ANTHROPIC_BASE_URL=http://router.example.com:8317
CLAUDE_CODE_OAUTH_TOKEN=<key>
CLAUDE_CODE_SUBSCRIPTION_TYPE=max
_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1
CLAUDE_CODE_GATEWAY_HINT_HEADERS=1
DISABLE_TELEMETRY=1
CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK=1
NODE_EXTRA_CA_CERTS=<path to the CA's PEM>   # a private CA only
```

![The Settings view: Appearance, About and the client block](assets/settings-light.png#only-light)
![The Settings view: Appearance, About and the client block](assets/settings-dark.png#only-dark)

Copy the seven lines from the Downstream client configuration card of the Settings view
([Settings](console.md#settings)), which renders them for the router it serves. The card builds the first line from the
proxy listener: `https` when the listener terminates TLS, else `http`, and the port the listener binds, on the
placeholder host `router.example.com`, since the router knows the address it binds and never the name a client reaches
it by. The pictured router binds its proxy on `127.0.0.1:8080`, so its card reads that port, and the block above is the
same card for a listener bound to `127.0.0.1:8317` without TLS. The hint under the card's block says what to replace:
"Inference is served on `{proxy}`, the address the proxy listener binds, so the base URL carries its scheme and port.
Replace the placeholder host with the name that reaches the listener, and the scheme and the port too where a TLS proxy
or a port mapping stands between Claude Code and it." The second line reads `<key>` because the router stores a hash of
the secret and shows it once, in the Key created or Key rotated dialog, which names this line as the secret's place
([Keys](console.md#keys)). The eighth line is yours to add when the listener's certificate comes from a private CA: the
path of a copy of the CA's PEM. A public CA needs nothing here.

The block has two homes, the `env` block of the client's settings file or the shell that launches the client. When the
same variable is set in both, the settings file's value applies in most sessions of Claude Code 2.1.296, as its
documentation states it: the client writes each `env` entry into its process environment and replaces the value
inherited from the shell, and some sessions keep the inherited value instead, the ones its documentation names under
how `env` values interact with the shell. A settings file can set a variable but cannot remove one, and an empty
string in the `env` block reads as unset. Between settings files, `env` values follow the settings precedence, a
managed entry over a user or project entry.

The card's Setup switch shows two more blocks, "Base URL and proxy" and "Base URL and DNS redirect", for a client that
uses Artifacts and the claude.ai connectors through the router ([claude.ai features](claude-ai-features.md)). Each holds
the eight inputs of [the launch script](claude-ai-features.md#the-launch-script) of that page with the values the router
knows: the base URL with the listener's scheme and port, the feature CA's fingerprint, and the setup's word. Paste the
block over the inputs at the script's head and replace the key. The script then sets each line of the client's
environment by itself, these seven among them. Beside those two blocks the card shows the fingerprint, which the script
takes as an input, with a copy control, and offers the CA's certificate as the file `feature-ca.pem`.

### ANTHROPIC_BASE_URL

Points Claude Code at the router's proxy listener, at the URL that reaches it from the client's machine. Every model
request and the companion mod's reports go here. The scheme has to match the listener: the router accepts plain HTTP on
any address, and a listener with a certificate pair speaks TLS alone ([TLS on the
listeners](installation.md#tls-on-the-listeners)). Without this line the client sends everything to `api.anthropic.com`
and the router sees nothing.

### CLAUDE_CODE_OAUTH_TOKEN

Holds the key's secret, the one the Key created or Key rotated dialog shows once. Put it here rather than in
`ANTHROPIC_API_KEY`: Claude Code treats the token as a subscription credential, which keeps three behaviors that a
credential in `ANTHROPIC_API_KEY` switches off, the one-hour cache TTL for the main conversation, the parsing of the
unified rate-limit headers on a `429`, and the automatic continue after a usage window resets. The three hold with or
without `CLAUDE_CODE_SUBSCRIPTION_TYPE`. Without a key the proxy listener answers every request to a route that takes
one with `401` `auth_missing`.

### CLAUDE_CODE_SUBSCRIPTION_TYPE

Optional, and needed for the companion mod's reports under `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`. Set it to `max`
beside the token. The client builds the credential it uses from `CLAUDE_CODE_OAUTH_TOKEN` with the subscription type
this variable names, null when it is unset, and reads the type for its policy eligibility check, which fetches the
organization's policy limits from `api.anthropic.com` for a null, `team` or `enterprise` type and skips the fetch for
`pro` or `max`, a consumer subscription no organization policy governs. Under the first-party flag the fetch goes to
`api.anthropic.com` with the router key, answers `401`, and the client then refuses every fetch a plugin makes, so the
companion mod reports nothing and the Sessions view reads `none` under Companion. Without the flag the client skips the
fetch for a base URL that is not its own. With or without the flag, a null type's settings lookup at start goes to
`api.anthropic.com` with the key's secret, under either telemetry switch, fails there, and the client tolerates that.
Under the flag that lookup and the policy fetch run before the first request, each retried five times, so a host it
cannot reach holds that request back 17 to 18 seconds and the client warns `Remote managed settings failed to load`.
Under `max` it fetches neither. The type also sets the welcome header's plan label, `Claude API` against `Claude Max`,
and the tools and skills the client announces: `DesignSync`, `RemoteTrigger` and `WebFetch` among the deferred tools,
the `schedule` skill and, in the terminal only, the `artifact-capabilities` skill appear under `max` and not under a
null type. The client documents the variable nowhere, sets it itself from its own auth state when the token is its own,
and a future build may drop it.

In a setup for the claude.ai features the line is required and reads `max` or `pro`: under another type or none the
client asks for the organization's policy limits, the router refuses that read, and the client then denies Artifacts.
Under `max` and `pro` the client still sends the hourly policy read in the background
(`GET /api/claude_code/policy_limits`) and the two settings reads of an interactive session
(`GET /api/claude_code_grove`, `GET /api/oauth/account/settings`). In a feature setup these reach the router and get its
`403`, where the plain setup's get the upstream's `401`.

### `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`

Optional, and worth setting. Under it Claude Code sends its first-party request shape to the router: the attribution
line with every field, the billing hash among them, `x-client-request-id`, the gateway hint headers and the
first-party betas, the cache diagnostics beta included ([The betas](relay.md#the-betas)), so the router restores
nothing and the request log reads the turn origin and the hints as the client sent them. It also stops the client
reading the router's models listing, which it otherwise uses to size its context window by the id
([The models listing](#the-models-listing)). Without it the client sends the reduced shape, and the router fills in,
toward `api.anthropic.com`, the fields the restoration names that the line lacks, each only when absent and never a
field the client sent ([The restoration](relay.md#the-restoration)). Two checks guard that line against a client
build the router is not pinned to and refuse the request as `client_drift` before any account is chosen
([Client drift](relay.md#client-drift)). The variable is an internal one that a future build may drop. A setup for the
claude.ai features requires it at `1`, which keeps the Artifact tool on under `DISABLE_TELEMETRY=1`.

### CLAUDE_CODE_GATEWAY_HINT_HEADERS

Required: the router refuses a Claude Code request without the `x-claude-code-request-class` header the gate sends ([The
required set](#the-required-set)). Under `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1` the headers arrive anyway. Set to
`1`, it makes the client send its gateway hint headers on a custom base URL: `x-claude-code-agent-type` (the subagent's
type on its own turns), `x-claude-code-compaction` (on the summarization request), `x-claude-code-context-compacted`
(once, on the first request after a compaction), `x-claude-code-prev-tool-durations` (the tool calls of the turn before
and their milliseconds, a value over 4,096 bytes left unread), `x-claude-code-prompt-id` (the prompt id the restoration
writes as `cc_prompt_id`) and `x-claude-code-request-class` (required, the class the router reads for the pin, the
restoration and the record). The router reads them for the request log and the analytics: the row carries each value as
sent, the tool durations as their sum, and the drawer shows them ([Requests](console.md#requests)).
`x-claude-code-context-compacted` also reaches routing: the session's pin reads cold on the request that carries it, so
a move that waits for a cold cache may take the session and a shedding round orders it first
([Moves](routing.md#moves)). The router reads `x-claude-code-request-class` on every Claude Code request: a `main` or
`subagent` class refreshes the session's pin and re-arms its keepalive recipe, a `compaction` class refreshes the pin
and stops the recipe at its completion instead of re-arming it, and any other class does neither ([Which requests
re-arm a recipe](keepalive.md#which-requests-re-arm-a-recipe)). The compaction header is
required on a request of the class `compaction`, and the other headers are optional, a request without one relayed the
same way. `0` stops them on every connection, and the router then refuses every inference request for the missing class
header.

The client also sends a summary fork under the class `auxiliary` for each running subagent every 30 seconds: the
subagent's whole context with one appended user block asking for a status line, which the router can block with the
Subagent status summary preset. The prompt suggestion and the away recap are forks of the main thread with the same
shape, each with its own preset, and the settings `promptSuggestionEnabled` and `awaySummaryEnabled` turn them off on
one machine. The forks, the presets and what a blocked fork shows are on [Guardrails](guardrails.md).

### DISABLE_TELEMETRY

Optional. Set to `1`, it stops the client's telemetry batches, which it posts to `api.anthropic.com` with the
credential it holds, behind the router the key's secret. `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` does the same and
also stops update checks and the bootstrap lookup. Neither switch stops the settings lookup a null subscription type
makes at start, which [`CLAUDE_CODE_SUBSCRIPTION_TYPE`](#claude_code_subscription_type) describes.

### CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK

Optional, and needed for fast mode behind the router. Without it the client asks `api.anthropic.com` directly, with
the session's credential and past `ANTHROPIC_BASE_URL`, whether the organization has fast mode, the check fails with a
router key, and fast mode stays off. With it the client skips the check and sends `speed: "fast"` when you switch fast
mode on. The router reads the speed from the request and the answer: a fast request is served on an account whose fast
lane is open, priced in dollars from usage credits, and moved to a sibling when an account's fast lane cools
([Cooldowns and failover](routing.md#cooldowns-and-failover)). It draws no plan window.

### For the companion mod

The companion mod needs Claude Code 2.1.299 or later. With its settings unset it reports to `ANTHROPIC_BASE_URL` under
the key it finds in the client's variables, in the order [The companion mod](../companion/install.md#configure) states,
so the block above needs no line of its own for it beyond
[`CLAUDE_CODE_SUBSCRIPTION_TYPE`](#claude_code_subscription_type), which lets its requests leave the process. At session
start the mod writes its instance header, `X-Phoebis-Instance-Id`, into `ANTHROPIC_CUSTOM_HEADERS` beside the variable's
other lines, and the client sends it on every model request of the process. The router reads it to tell that process's
requests from those of another process on the same session id, so binding a session to its process needs no wrapper
script. Installing and configuring the mod is that page's.

`CLAUDE_CODE_PROMPT_CACHE_TTL=5m` is an optional line the block does not carry, worth setting for Fable 5.2 and Fable
5.1 sessions in a pool of API-key accounts with cache warming on: at a 0.025x read price, a five-minute entry kept warm
by probes stays cheaper on a metered account than a one-hour entry for idle warming of up to about 2.2 hours, past which
the one-hour entry costs less ([Cache TTL](routing.md#cache-ttl)). Leave it unset in a pool of plan-funded accounts,
where a write costs the same window share at either TTL and a five-minute entry takes 15 probes an hour against 1.11 for
a one-hour entry ([The four fields](keepalive.md#the-four-fields)), and unset without warming. Through a pool with a
cache TTL policy the variable has no effect, since the pool decides ([Cache TTL](routing.md#cache-ttl),
[Keepalive](keepalive.md)).

## The required set

The router reads a Claude Code inference request for the session's pin, the restoration, the identity splice and the
record: the user agent's version is the one the restoration signs into the line, the session header keys the pin and the
session memory, the request class decides whether the restoration fills `cc_prev_req` and lands on the record, the
attribution line carries the fields the router restores, `metadata.user_id` carries the account identity the router
splices, and the agent and compaction headers tell a subagent's turn and a summarization apart. The body never decides a
class, so a request that lacks one of these items is refused rather than guessed at.

| Item | The client sends it | Word | Kinds |
| --- | --- | --- | --- |
| The `claude-cli/` version spelled as the client spells one, digits and dots with at most one `-` and a prerelease suffix | Always | `client_version` | inference, count, probe |
| `x-claude-code-session-id` | Always | `session_header` | inference, count, probe |
| `x-claude-code-request-class` | Under `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` or the first-party assumption | `class_header` | inference |
| The attribution line at `system[0]`, where `system` is an array | Always, unless `CLAUDE_CODE_ATTRIBUTION_HEADER=0` | `attribution_line` | inference |
| `metadata.user_id` with an `account_uuid` member | Always | `metadata_user_id` | inference |
| `x-claude-code-agent-id`, under the class `subagent` or `workflow` | On a subagent's turns and its forks | `agent_id_header` | inference |
| `x-claude-code-compaction`, under the class `compaction` | Under `CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` or the first-party assumption, on the summarization request | `compaction_header` | inference |

A request that lacks an item is refused `403` `permission_error` with the code `client_contract` before any account is
chosen. The message names the missing item and the fix, the answer carries `x-should-retry: false`, and the Requests
view names the item in the drawer's Outcome section, and the routing log reads `client contract` with the item's word.
The first missing item in the table's order names the refusal. A token count and a keepalive probe are inside the set
for the version and the session header, the two items their headers carry, and outside the five others.

The other headers stay optional: the agent type, the context-compacted mark, the parent agent id, the prompt-id header,
the tool durations and `x-client-request-id`. A request without one is relayed the same way. An agent id and a parent
agent id may hold an `@` beside the ASCII letters, digits and `-./:_` of a session id, since a teammate of an agent team
names itself by its name, an `@` and its team.

`CLAUDE_CODE_ATTRIBUTION_HEADER=0` meets the refusal, since the line is required where `system` is an array. The quota
probe (no `system`) is outside the line's item alone, and a client outside the Claude Code class is refused `403`
`client_not_allowed` before the set is read ([Clients](routing.md#clients)).

## The first request

Start Claude Code from the shell that holds the block, or with the block in its settings file, and send a prompt. The
answer comes through the router. Open the console's Requests view: the newest row is that request, its Status reads
`200` and its Account names the account that served it ([Requests](console.md#requests)). Then read the key's own
figures from the proxy listener with the `curl` of [Your key's usage](#your-keys-usage): the request is one of the
`24h` window's `requests`, and its model is among that window's `models`.

## What the proxy accepts

| Method | Path | Credential | Answer |
| --- | --- | --- | --- |
| HEAD | `/api/hello` | none | `200 OK` |
| HEAD | `/api/ready` | none | `200` while the router loop, the ledger consumer and the store are alive, else `503` |
| GET | `/api/ready` | none | the same status, with `{"ready": true}` or `{"ready": false, "dead": [...]}` |
| POST | `/v1/messages` | router key | relayed to the selected account |
| POST | `/v1/messages/count_tokens` | router key | relayed, same failover loop, travels as sent |
| GET | `/v1/usage` | router key | that key's own usage for the last 24 hours and 7 days, JSON or HTML by `Accept` |
| GET | `/v1/models` | router key | the models the key may route to, with a `[1m]` twin per long-context model |
| GET | `/v1/models/{*id}` | router key | one row of that listing, or `404` |
| POST | `/v0/companion/events` | router key | the companion mod's event report |
| GET | `/v0/companion/allowance` | router key | the key's pool under pseudonyms, for the companion mod's `allowance` tool |
| GET | `/v0/features/ca.pem` | none | the feature CA's certificate as PEM text, on a router with the `features` block |
| any other method on a known path | | none | `405` |
| any unmatched path | | none | `404` |

The calls of the [claude.ai features](claude-ai-features.md) arrive on a listener of their own, the feature listener of
a router with the `features` block ([features](installation.md#features)), where no path of this table answers.

The query string of a request to either Messages route travels as the client sent it, `?beta=true` included: the
router routes on the path alone.

Claude Code probes `/api/hello` before it sends anything and ignores the answer. `phoebis health` and the image's
health check read `/api/ready` instead: the two differ when the router loop, the ledger consumer or the store died
while the listener stays up, and `dead` names each one that died as `ledger`, `router` or `store`. Every refusal on
the listener, the wrong-method and unknown-path answers included, carries an Anthropic-shaped error body
([Refusal codes](#refusal-codes)), with two exceptions. The not-ready `503` of `/api/ready` carries the readiness body
instead, and the events route answers a report it refuses with a JSON body whose `code` the mod reads, under the codes
[The companion protocol](relay.md#the-companion-protocol) lists, while a key it refuses gets the answer `/v1/messages`
gives. Every answer of `/v0/companion/events`, `/v1/models`, `/v1/models/{*id}` and `/v1/usage` carries
`Cache-Control: no-store`, a refusal of a malformed path among them.

The key is presented as exactly one of `Authorization: Bearer <secret>` or `x-api-key: <secret>`. No credential
header, two of them, one twice, or a malformed one is refused `401` under its own code, and so is a well-formed key
the registry refuses: one it does not hold, a revoked, retired, disabled or expired one, one whose pool is disabled or
gone, and one deleted, or deleted and created again under its id, between admission and routing. Every registry
refusal is the one `auth_unknown`, whose message names no cause, so a caller learns nothing about why. Every
credential refusal, the registry's included, carries `x-should-retry: false`, so the client does not resend it.

A request on either Messages route whose class is not Claude Code is refused `403` `client_not_allowed` before its body
is read, under every key ([Clients](routing.md#clients)). When no account can serve for the moment the answer is `503`
`accounts_unavailable`, which also stands where an account's authentication failure is the request's last word: the
router never forwards an upstream `401`, and the answer names no account and no reason ([When nothing can
serve](routing.md#when-nothing-can-serve)).

A token count is relayed through the same selection and failover loop as a message and travels as the client sent it
under every policy: the markers it carries keep their TTLs, no marker is added to a marker-less one, no beta follows
it ([The betas](relay.md#the-betas)), and nothing else in the body is added. It takes no slot of the pool's per-account
in-flight cap and is never held or refused for want of one ([In-flight caps](routing.md#in-flight-caps)), though it
takes one of its key's body-read slots.

### GET /v1/usage

`GET /v1/usage` with the key as `Authorization: Bearer <secret>` or `x-api-key: <secret>` answers that key's own usage
as JSON, or as one HTML page when `Accept` prefers `text/html`. The JSON carries `key` (`id`, `label`, `pool_id`),
`now`, and `windows` with `24h` and `7d`, each `from`, `to`, `requests`, `succeeded`, `failed`, `incomplete`,
`canceled`, `tokens` (`input`, `output`, `cache_read`, `cache_write`), `cost_usd` and `models` (per canonical model:
`model`, `requests`, `tokens`, `cost_usd`). What the figures cover, the key refusal and the HTML page are under
[Your key's usage](#your-keys-usage), and a ledger the route cannot read is the `usage_unavailable` row of
[Refusal codes](#refusal-codes).

## Refusal codes

Every refusal the relay answers on the proxy listener and on the feature listener is in Anthropic's error shape,
`{"type": "error", "error": {"type", "message"}, "request_id"}`, with the header `x-phoebis-error-code` naming the code
and `Retry-After` where the row says so. A client meets `domain_checks_at_cap`, `feature_account_unavailable`,
`feature_account_unset`, `feature_not_allowed`, `key_sockets` and `sockets_at_cap` on the feature listener alone
([claude.ai features](claude-ai-features.md#what-the-router-refuses-and-what-the-client-shows)), and the `401` codes of
a key on both. The companion mod's routes answer a refused key the same way and their other refusals in their own shape,
described on [The relay](relay.md#the-companion-protocol). One row per code, with every message the code is answered
with.

<!-- refusal-codes: begin -->
| Code | Status | Type | Messages | What to do |
| --- | --- | --- | --- | --- |
| `account_canceled` | 503 | `api_error` | `the bound account was disconnected while the request was in flight` | Send the request again. It lands on another account. |
| `accounts_at_cap` | 429 | `rate_limit_error` | `every account in this pool is at its in-flight cap` | Wait out the `Retry-After` (1 to 19 seconds) and send it again, or raise the cap ([in-flight caps](routing.md#in-flight-caps)). |
| `accounts_exhausted` | 429 | `rate_limit_error` | `every account in this pool is rate limited` | Wait out the `Retry-After`, or add an account to the pool ([when nothing can serve](routing.md#when-nothing-can-serve)). |
| `accounts_unavailable` | 503 | `api_error` | `no account in this pool can serve requests`, `the bound account cannot serve requests` | Read the pool's members on the Pools view ([when nothing can serve](routing.md#when-nothing-can-serve)). A key moved to another pool while the request was in flight gets the same answer with `Retry-After: 1`. Send the request again. |
| `auth_ambiguous` | 401 | `authentication_error` | `present exactly one key` | Send the key in one header, `Authorization: Bearer` or `x-api-key`, never both. The answer carries `x-should-retry: false`, so the client does not resend it. |
| `auth_malformed` | 401 | `authentication_error` | `the key header is malformed` | Send the key as `Authorization: Bearer <secret>` or `x-api-key: <secret>`. The answer carries `x-should-retry: false`, so the client does not resend it. |
| `auth_missing` | 401 | `authentication_error` | `a key is required` | Set `CLAUDE_CODE_OAUTH_TOKEN` to the key's secret. The answer carries `x-should-retry: false`, so the client does not resend it. The feature listener gives this answer to a socket request that no token admits too, and its refusal family counts that one under `socket_token_unknown` ([What the router refuses and what the client shows](claude-ai-features.md#what-the-router-refuses-and-what-the-client-shows)). |
| `auth_unknown` | 401 | `authentication_error` | `the presented key is not valid` | The key is unknown, revoked, retired, disabled or expired, or its pool is disabled or gone. Ask the operator. The answer carries `x-should-retry: false`, so the client does not resend it. |
| `body_budget_exhausted` | 503 | `api_error` | `the router holds as many request bytes as it allows` | Send the request again. The bodies in flight and their parsed trees held `proxy.body_budget_bytes`. |
| `body_over_budget` | 413 | `request_too_large` | `the request body and its parsed tree are larger than the router's body budget` | The body and the tree the relay parses from it are more than the whole of `proxy.body_budget_bytes`, so no wait makes room for them. The operator raises `proxy.body_budget_bytes`, or the client sends a smaller body. The answer carries `x-should-retry: false`, so the client does not resend it. |
| `body_shape` | 400 | `invalid_request_error` | `the request body carries a billing line the relay cannot decode`, `the request body carries a billing stamp the relay can neither check nor restamp`, `the request body carries a cache_control marker the relay cannot place, so the pool's cache TTL policy cannot be applied to it`, `the request body holds more elements than the relay reads`, `the request body has no metadata.user_id the account can be bound to`, `the request body has no top-level model to resolve`, `the request body carries a safeguards member the relay cannot place under this key's auto-mode setting`, `the request body carries the top-level member "{name}" more than once` for `metadata`, `model`, `safeguards`, `stream` and `system`, and `the request body carries the member "{name}" more than once` for `metadata.user_id` and `system[0].text` | Send the body as Claude Code sends it. The relay edits that shape and no other. |
| `client_contract` | 403 | `permission_error` | `the client version in the User-Agent header is not spelled as Claude Code spells one. Report the client version to the operator, who updates the router`, `the request carries no x-claude-code-session-id header the router requires of a Claude Code client. Send the requests as Claude Code sends them`, `the request carries no x-claude-code-request-class header the router requires of a Claude Code client. Set CLAUDE_CODE_GATEWAY_HINT_HEADERS=1`, `the request carries no attribution line at system[0] the router requires of a Claude Code client. Leave CLAUDE_CODE_ATTRIBUTION_HEADER unset`, `the request body has no metadata.user_id with an account_uuid member the router requires of a Claude Code client. Send the requests as Claude Code sends them`, `the request names a subagent class and carries no x-claude-code-agent-id header the router requires of a Claude Code client. Send the requests as Claude Code sends them`, `the request names the compaction class and carries no x-claude-code-compaction header the router requires of a Claude Code client. Set CLAUDE_CODE_GATEWAY_HINT_HEADERS=1` | Set the lines of the environment block above, and send the requests as Claude Code sends them ([The required set](#the-required-set)). |
| `client_drift` | 403 | `permission_error` | `the client's version suffix does not reproduce from any text block of the first user message, the empty prompt or the version the user agent names, the anchors this build tries for every request. Where the session's main turns pass, re-pinning the router does not help: the refused body shows where this client anchored the suffix, and its billing line's cc_is_subagent and the x-claude-code-agent-id, x-claude-code-request-class and x-claude-code-agent-type headers, where the client sends them, tell a subagent's request apart`, `the conversation was resumed, and the client's version suffix does not reproduce from the resume placeholder, the empty prompt or the user agent. Re-pin the router to this client version`, `the client's billing hash does not reproduce under the Claude Code release this build is measured against. Re-pin the router to this client version` | Where the session's main turns fail too, the client build moved past the one the router is pinned to, in a resumed conversation as in any other, and the operator re-pins ([re-pinning the client build](operations.md#re-pinning-the-client-build)). Where they pass, a subagent took its version suffix from a text outside the anchors the router tries, and the operator captures the refused body ([Client drift](relay.md#client-drift)). The second message names the resume. The answer carries `x-should-retry: false`. |
| `client_not_allowed` | 403 | `permission_error` | `the router admits Claude Code clients only` | Use Claude Code. The router serves no other client, under any key ([clients](routing.md#clients)). The answer carries `x-should-retry: false`. |
| `domain_checks_at_cap` | 429 | `rate_limit_error` | `as many domain checks are in flight as the router forwards at once` | A domain check, which the client sends ahead of a web fetch, waited out its 30 seconds for a place. The router forwards 16 at once, 4 from one address. Wait a second (`Retry-After: 1`) and send the check again. |
| `fast_credits_unavailable` | 429 | `rate_limit_error` | `no account in this pool can draw usage credits for fast mode` | Turn fast mode off, or wait for the credits. |
| `fast_lane_exhausted` | 429 | `rate_limit_error` | `every account in this pool is at its fast limit` | Wait out the `Retry-After`, or turn fast mode off. |
| `feature_account_unavailable` | 403 | `permission_error` | `the selected account cannot serve this feature (operator)`, `the selected account cannot serve this feature (subscription_ended)` | The account selected for the feature is not ready, for example signed out, or its subscription has ended. Ask the operator, who signs the account in again or selects another. The answer carries `x-should-retry: false`. |
| `feature_account_unset` | 403 | `permission_error` | `no account is selected for artifacts on this key, ask the router's operator`, `no account is selected for connectors on this key, ask the router's operator` | The key selects no account for the feature, or it takes its pool's choice and the pool selects none. Ask the operator, who selects one. The answer carries `x-should-retry: false`. |
| `feature_not_allowed` | 403 | `permission_error` | `the router does not forward this claude.ai call` | The router forwards this call for no client, so nothing is to be done unless a claude.ai feature stopped working, and then ask the operator. The answer carries `x-should-retry: false`. |
| `guardrail_blocked` | 403 | `permission_error` | `blocked by guardrail "{name}"`, the guardrail's id standing in for a stored name that carries "model:", "OAuth token has been revoked" or "prompt is too long" | A guardrail the operator attached to your key matched the request, and the answer carries `x-should-retry: false`, so do not send it again unchanged. Claude Code reads a 403 as final, so it neither heals, retries nor tries a fallback model on it, and a blocked request costs one request. The Requests view names the guardrail ([Guardrails](guardrails.md)). |
| `invalid_body` | 400 | `invalid_request_error` | `the request body is not valid gzip`, `the request body nests deeper than the relay reads`, `the request body is not a valid Messages request` | Send a Messages request as Claude Code sends it. |
| `invalid_limit` | 400 | `invalid_request_error` | `limit must be between 1 and 1000` | Ask `GET /v1/models` for 1 to 1000 rows. |
| `invalid_model` | 400 | `invalid_request_error` | `the request body's model is not a usable model name` | Set a model the listing names ([the models listing](#the-models-listing)). |
| `key_body_reads` | 429 | `rate_limit_error` | `this key has as many request bodies arriving as it allows` | Wait one second (`Retry-After: 1`) and send it again. |
| `key_sockets` | 429 | `rate_limit_error` | `this key has as many sockets open as it allows` | The key has as many sockets open on the feature listener as one key may, a count [claude.ai features](claude-ai-features.md#what-the-router-refuses-and-what-the-client-shows) names. The client waits and dials again by itself, and a place frees as soon as one of the key's sockets ends. The answer carries no `Retry-After`, since no time bounds the wait for a place. |
| `ledger_behind` | 503 | `api_error` | `the router is not accepting requests right now` | Wait out the `Retry-After` of 5 seconds. The operator reads the writer's standing on [Statistics](statistics.md#the-ledger-writer). |
| `method_not_allowed` | 405 | `invalid_request_error` | `the method is not supported on this route` | Use the method the table in [What the proxy accepts](#what-the-proxy-accepts) names. |
| `missing_model` | 400 | `invalid_request_error` | `the request body has no model` | Set `model` in the body. |
| `model_not_allowed` | 404 | `not_found_error` | `model not allowed for this key` | The key's or the pool's model rules refuse the model ([model rules](routing.md#model-rules)). |
| `model_not_found` | 404 | `not_found_error` | `model: {id}` | `GET /v1/models/{id}` named a model the listing lacks. |
| `model_rejected` | 404 | `not_found_error` | `every account in this pool rejected the model` | Every member answered 404 for the model. Pick one the accounts serve. |
| `not_found` | 404 | `not_found_error` | `the requested resource was not found` | Use a path the table in [What the proxy accepts](#what-the-proxy-accepts) names. |
| `request_incomplete` | 400 | `invalid_request_error` | `the request body ended before it was complete` | The connection closed before the body did. Send the request again. |
| `request_timeout` | 408 | `invalid_request_error` | `the request body did not arrive in time` | The body took longer than `proxy.body_read_seconds`. Send the request again. |
| `request_too_large` | 413 | `request_too_large` | `the request body is too large` | The body is over `proxy.max_request_bytes`, before or after inflation. |
| `router_unavailable` | 503 | `api_error` | `the router is not available` | Send the request again. The operator reads the router's log. |
| `sockets_at_cap` | 429 | `rate_limit_error` | `as many sockets are open as the router carries at once` | The feature listener carries as many open sockets as it may, whatever their keys, a count [claude.ai features](claude-ai-features.md#what-the-router-refuses-and-what-the-client-shows) names. The client waits and dials again by itself, and a place frees as soon as any socket ends. The answer carries no `Retry-After`, since no time bounds the wait for a place. |
| `unsupported_content_encoding` | 415 | `invalid_request_error` | `the request body's content-encoding is not supported. Send gzip or identity` | Send the body as gzip or identity. |
| `upstream_cut` | 502 | `api_error` | `the upstream response could not be read after its head` | Send the request again. |
| `upstream_stalled` | 502 | `api_error` | `the upstream stalled after its head` | The upstream sent nothing for 600 seconds. Send the request again. |
| `upstream_undecodable` | 502 | `api_error` | `the upstream response could not be decoded after its head` | Send the request again. |
| `upstream_unreachable` | 502 | `api_error` | `the upstream did not answer` | Send the request again. The operator reads the attempt on the Requests view. |
| `usage_unavailable` | 500 | `api_error` | `the usage ledger could not be read` | `GET /v1/usage` could not read the ledger. Send it again. |
<!-- refusal-codes: end -->

`body_shape` names the member the relay has to find or edit and could not, or a body holding more elements than the
relay reads. A top-level `metadata`, `model`, `stream` or `system` carried more than once, a first `system` block whose
`text` or a `metadata` whose `user_id` appears twice, raw or escaped, is refused since the relay edits the first
spelling and a last-wins parser upstream would read the other. So is a `safeguards` member the relay cannot cut while
the key's server-side auto mode is off (carried twice, or spelled with an escape), a `cache_control` marker the pool's
cache TTL policy cannot reach (a key on the marker's path spelled with an escape), a billing stamp the relay can neither
check nor restamp (a key on its path, the billing line's header name or its `cch=` spelled with an escape, or a second
stamped `text` in the first `system` block). Each is refused before anything reaches the upstream, whether or not the
request needs an edit. The splice of the account's uuid refuses a body without `metadata.user_id` the same way, a guard
no client meets: an inference request without the member is refused as `client_contract` first, and a count request
keeps its body as sent, since the public `count_tokens` schema has no `metadata` member. A billing line the strict
decoder refuses for a lone surrogate escape is refused before anything reaches the upstream too, on a request whose line
the relay restores. A body holding more than 1,000,000 elements, each `[`, `{` and `,` outside a string, is refused
before the relay parses it ([Compiled-in limits](installation.md#compiled-in-limits)). A body that nests deeper than the
relay reads, or is not a JSON object, is `invalid_body` instead.

Five rows rest on the proxy listener's bounds ([proxy](installation.md#proxy)). A body that does not finish arriving
within `proxy.body_read_seconds` of its authentication (60 by default) is `request_timeout`. A body past
`proxy.max_request_bytes` (32 MiB by default), declared, arriving or inflated, is `request_too_large`. A body whose
bytes and parsed tree together are more than the whole of `proxy.body_budget_bytes` (512 MiB by default) is
`body_over_budget`, refused for good since no wait makes room for it. A request whose body, or whose body with its tree,
fits that budget but would take the router past it beside the bodies already in flight is `body_budget_exhausted`, with
no `Retry-After` since the router refuses rather than queues. A key with `proxy.key_body_reads` bodies already arriving
(128 by default) is `key_body_reads`. `key_body_reads`, a declared size past the bound and a budget that cannot hold the
body's declared size, or the bound where the request declares none, are refused from the head, before a body byte is
read. The inflated body and the parsed tree are counted after the read, so `body_budget_exhausted` can also answer then,
and `body_over_budget` answers then alone.

A body sent with `content-encoding: gzip` is inflated within the body bound before it is read, and one that is not
valid gzip is `invalid_body`. Any other token (`br`, `deflate`, `zstd` or an unknown one), a value that is not visible
ASCII, and more than one `content-encoding` line are `unsupported_content_encoding`, refused before the body is read,
since the relay never carries bytes it can neither read nor edit under an account's credential.

`ledger_behind` is answered while the ledger's writer channel holds at least half its depth, 2,048 records of 4,096: on
the proxy listener after the body is read and before any account is chosen, and on the feature listener ahead of the
body's read ([claude.ai features](claude-ai-features.md#what-the-router-refuses-and-what-the-client-shows)).
`router_unavailable` is answered when the router loop did not answer the request's selection.

## Your key's usage

`GET /v1/usage` on the proxy listener answers the usage of the key that presents it, and of no other: requests, tokens
and the estimated cost over the last 24 hours and the last 7 days, per model, with the key's label and pool. The figures
are the router's hourly rollups under the key, the same rollups the operator's analytics read. A keepalive probe counts
as no request, and a probe that the plan did not fund adds its dollars to the cost alone, so a model can read a cost
beside no request. Each window counts a request that its client canceled under `canceled`, so `succeeded`, `failed`,
`incomplete` and `canceled` sum to `requests`. Each window is summed from whole hourly buckets, so it starts at the top
of an hour: the `from` the answer reports is that hour, up to an hour earlier than the window's name, and the figures
cover it all. The figures are this key's own from its creation, filed under the generation it was created with, so a
deleted key's rows stay with the key they belonged to and a later key under the same id never reads them. Nothing about
the accounts is answered, and the secret never is.

```sh
curl -sS -H "Authorization: Bearer $CLAUDE_CODE_OAUTH_TOKEN" http://127.0.0.1:8317/v1/usage | jq .windows.\"24h\"
```

With `Accept` preferring `text/html` (listed alone, or weighted above `application/json` as a browser sends it), the
same route answers one self-contained page, `Usage of {label}` with a table per window, so a browser or a
`curl -H 'Accept: text/html'` saved to a file shows the figures without the console and without an admin login. A
wrong or missing key is refused exactly as `/v1/messages` refuses it, in the Anthropic error shape.

## The models listing

`GET /v1/models` on the proxy listener answers the key that presents it with the models it may route to: every enabled
account of the key's pool in state `ready` is asked for its catalog through the router's cache
([Catalogs](routing.md#catalogs)), the same cache the router derives a model's dated id from, so a request naming an
undated id is routed, priced and recorded under the newest dated snapshot the listing holds for it. The pool's and the
key's [model rules](routing.md#model-rules) are applied to each id, one row is kept per id (the first account in
priority order that lists it), and the rows are sorted by id. The shape is the API's own: `data[]` of `id`, `type`,
`display_name`, `created_at`, `max_input_tokens` and `max_tokens` as the upstream reports them, with `first_id`,
`last_id` and `has_more`. `limit` (1 to 1000, default 20), `after_id` and `before_id` page it, and a `limit` outside its
range is refused `400` `invalid_limit`. The listing answers any user agent that holds a key. An account whose fetch
failed contributes nothing until the next fetch, and a model that is cooling down on an account still lists. `GET
/v1/models/{id}` answers the one row, or `404` `model_not_found` with the message `model: {id}`. An id with a slash may
be sent as is or percent-encoded.

Beside every model whose `max_input_tokens` is above 200,000 the listing carries a twin whose id ends in `[1m]` and
whose display name ends in ` (1M Context)`, with the same figures. Claude Code behind a gateway sizes its context
window by the id, 200K unless it ends in `[1m]`, so the twin is what lets it use the window the upstream reports. A
request that names the twin is routed as the model itself, and the suffix never reaches the upstream. The twins are on
by default and switched off per key with `long_context_twins`.

## When the client waits

Claude Code 2.1.296 watches an answer on two clocks. Once the response head is in, 20 seconds without a byte show the
banner `Waiting for API response · will retry in X · check your network`, and 180 seconds of silence make the client
abort the request and send it again. Before the head, the first byte has a window of 180 seconds plus 1 second per
32 KiB of request body, with one retry near 600 seconds. `CLAUDE_ENABLE_BYTE_WATCHDOG` and
`CLAUDE_ENABLE_STREAM_WATCHDOG` switch the watchdog off.

Five waits of the router meet those clocks. The router's own guards on the upstream, 600 seconds for the response head
and 600 seconds for the gap between two stream events, sit above the client's window, so a dead upstream stream is cut
by the client at 180 seconds with the watchdog on, and the router's guard stands behind it
([The relay's waits](relay.md#the-relays-waits)). A retry in place is the second wait: on a throttle the router sleeps
the throttle's delay on the same account before any byte of the head reaches the client, up to the pool's
`retry_in_place_seconds` (0 to 300, default 60), so a bound above the client's first-byte window makes the client end
the request and send it again while the router is still waiting ([Retry in place](routing.md#retry-in-place)). The hold
for a slot is the third, under the same bound: when every account that could serve is at its in-flight cap, the router
holds the request for a slot before any byte of the head reaches the client
([In-flight caps](routing.md#in-flight-caps)). The slow-down's hold is the fourth, with a bound of its own: on a pool
whose `slow_down` is on, a request of a slowed account's session waits before any byte of the head for the pin's cycle
times the slow-down, up to the pool's `slow_down_hold_seconds` (1 to 300, default 120), served before a slot or reset
wait and under the same 180 s window, so the default leaves 60 s for the upstream's head and the pre-commit hold, and a
bound over 150 s draws the pool's `hold_over_window` warning ([Slow-down](routing.md#slow-down)). A hold that crosses
the window costs one visible retry, since the retry rejoins the pin's deadline. The wait for a moved pin's first head is
the fifth, under the slow-down's bound with the pool's `slow_down` on or off: the next request of a pin that landed on
another account waits before any byte of the head for the head of the request that carried the move, up to the pool's
`slow_down_hold_seconds` from that request's dispatch and never past its own deadline, since the upstream makes a cache
entry readable only once the response that writes it has begun, so a move pays its rewrite once
([The windows the router works from](routing.md#the-windows-the-router-works-from)). The choice is the operator's: keep
each bound at or under the window, or switch the watchdog off for that client with `CLAUDE_ENABLE_BYTE_WATCHDOG=0`,
`API_FORCE_IDLE_TIMEOUT=0` and `API_TIMEOUT_MS=18000000`, which lets the client wait out a hold longer than its window,
the slow-down's among them where the operator raises its bound.

## How a request is identified

A conversation is identified by its session header, the session id every Claude Code request carries
([Clients](routing.md#clients)), and the two boundary-hash chains the router reads off every body are [The boundary
chains](relay.md#the-boundary-chains): a new session whose messages continue another conversation of the same key at
depth 2 or deeper is recorded as a fork of it. A body the strict JSON reader refuses because a string was cut between
the two halves of a character, which the client sends as a lone surrogate escape, is still read for its facts, and the
cut character travels as the client sent it.

## Auto mode behind the router

Claude Code in auto mode asks the upstream to review each tool use on the model's own turn, and what the router does
with that request is the pool's `auto_mode_server` (on by default), which a key can override. The mechanism, the latch,
the classifier's two stages and the client's notice are on [Auto mode behind the
router](relay.md#auto-mode-behind-the-router). The client's classifier calls that carry the classifier marker, the
`auxiliary` class with the beta `auto-mode-classifier-2026-07-16` sent under the first-party assumption with
experimental betas allowed, take no share of a plan window on a subscription account where they run on the account
their judged turn ended on, and the Requests view lists them under Classifier `client`. One that runs off that account,
or in a session with no turn to judge, is charged as any call and listed under `client_off`
([Statistics](statistics.md#plan-meter)).

## Headers and transport

Request headers travel as the client sent them, in the client's own order and spelling, the proxy listener speaks
HTTP/1.1 and no HTTP/2, and what the router drops, owns and adds is on
[Headers and transport](relay.md#headers-and-transport). The per-turn effort form, the control entry Claude Code
appends to `messages` beside `output_config.effort` on a model it marks for it, is relayed as sent and described under
[The per-turn effort form](relay.md#the-per-turn-effort-form). Remote Control is unavailable behind the router: the
client reports that `ANTHROPIC_BASE_URL` does not point at `api.anthropic.com`, and that the override does not apply to
Remote Control.

An interactive start of the client also reaches past the router. It opens connections to `downloads.claude.ai`,
`github.com` and, without `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `raw.githubusercontent.com`, all three by name
and none through `ANTHROPIC_BASE_URL`, runs `npm --global config get prefix` and `gh` where the host has them, and
registers a URL handler (`claude-code-url-handler.desktop`) in the user's applications directory. On a host that reaches
the router alone those connections fail and the client starts regardless.

In a setup for the claude.ai features the client's connections to `api.anthropic.com` and `mcp-proxy.anthropic.com`
reach the router's feature listener and no upstream. The first-start check of a config directory that no client has used
gets its `200` there, and each other read of a start gets the router's refusal, as
[`CLAUDE_CODE_SUBSCRIPTION_TYPE`](#claude_code_subscription_type) has it for the reads under `max` and `pro`. In the
"Base URL and proxy" setup the connections to each other host, the three above among them, pass through `phoebis-proxy`,
which tunnels each host to itself ([Base URL and proxy](claude-ai-features.md#base-url-and-proxy)).
