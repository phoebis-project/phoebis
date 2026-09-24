# Pools, keys, routing, and overage

A pool holds one or more accounts, each with a priority; a key routes into exactly one pool; every request a key
presents is placed on one account of that pool by a selection that honours the session's pin, the pool's strategy,
and each account's quota. This page describes every field of an account, a pool, and a key, then the selection
itself, and the answers a client gets when nothing can serve it.

## Catalogs

An upstream's model list is read live from `GET /v1/models` on `api.anthropic.com` with the upstream's own
credential, on demand, and held for `routing.catalog_cache_seconds` (sixty by default); the Upstreams view's
catalog popup, the rule editors' suggestions, and the proxy's [models listing](client.md#the-models-listing) all
read that cache, and `GET /admin/accounts/{id}/catalog` answers it with `?refresh=1` to read again. Nothing about
routing depends on it: every enabled member of a pool is a candidate for any model the rules allow, and an
upstream that does not serve a model says so with a `404` the router records as a rejection
([cooldowns](#cooldowns-and-failover)). An upstream whose fetch failed contributes nothing to a listing until the
next fetch, and the popup shows the error.

The catalog lists some models under a dated id (`claude-sonnet-4-5-20250929`) while Claude Code asks for the
undated one (`claude-sonnet-4-5`). The router resolves a spelling it has not seen through
`GET /v1/models/{spelling}` in the background, never on the request's own path, and from then on the dated id
is the canonical name the rules, the ledger, the pricing, the pins, and the cooldowns use for that spelling; the
body toward the upstream keeps the client's spelling. Until a spelling's first resolution lands, the client's
folded spelling is the canonical name, which is why a rule for such a model is written as a glob; a resolution
is kept across restarts, and a later probe that fails or answers 404 never takes it back.

## Pools

A pool holds one or more accounts, each with a priority:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/pools \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"id":"main","label":"Main","members":[{"account_id":"vendor","priority":0}]}'
```

Every field, in the order the request body takes them:

| Field | Default | Bound |
| --- | --- | --- |
| `id` | required | a slug: `id must match [a-z0-9][a-z0-9-]{0,63}` |
| `label` | required | none |
| `enabled` | `true` | none |
| `strategy` | `least_utilized` | `earliest_refresh`, `least_utilized`, `priority_random`, or `round_robin` |
| `api_key_sub_strategy` | `priority_random` | `priority_random` or `round_robin` |
| `prefer_subscription` | `false` | see below |
| `affinity_ttl_seconds` | `3600` | `0` or at least 60: `affinity_ttl_seconds is 0 or at least 60` |
| `overage_policy` | `prefer_plan` | `prefer_plan`, `allow`, or `never` |
| `migrate_above_utilization` | unset | 0 to 1: `migrate_above_utilization is between 0 and 1` |
| `select_below_utilization` | unset | 0 to 1: `select_below_utilization is between 0 and 1` |
| `model_rules` | empty | an ordered list of `{pattern, not}` entries, at most 256; see Model rules |
| `keepalive` | disabled | a policy object; its defaults are on the [Keepalive page](keepalive.md) |
| `cache_ttl` | off | a policy object; see below |
| `cache_diagnostics` | unset (inherits) | `true`, `false`, or `null` |
| `body_capture_hours` | `0` (off) | `body_capture_hours exceeds stats.body_capture_max_hours` |
| `members` | none | create only; replaced wholesale by `PUT /admin/pools/{id}/members` |

`cache_diagnostics` forces the cache-diagnosis beta and the `previous_message_id` member toward
`api.anthropic.com` for this pool's requests, whatever the [bootstrap option](installation.md#compatibility)
says; `null` inherits it.

A member's `priority` is a number; lower sorts first. Naming an unknown account in `members` answers `404
account {id} not found`, and the pool is not created.

`prefer_subscription` sets `prefer_subscription_effective` in the answer, which is also true whenever the
strategy is `earliest_refresh` or `least_utilized`, whatever the flag holds, because those two strategies order
by kind regardless of it. A pool with keys cannot be deleted: `409 pool still has keys`.

The kind order (subscription accounts with plan quota, then those in overage, then API keys) is in force under
`earliest_refresh` and `least_utilized` whatever `prefer_subscription` says, and under `priority_random` and
`round_robin` only when `prefer_subscription` is on; where it is in force, it outranks the priorities.
`api_key_sub_strategy` orders the API-key group only under `earliest_refresh` and `least_utilized`, once that
group is the first with an eligible upstream, because an API key has no usage sample for those two to order by;
under `priority_random` and `round_robin` the API-key group takes the pool's own strategy. Each strategy's rule, as
the console states it on the pool card (the pool dialog's "Prioritize subscription upstreams" toggle sets
`prefer_subscription`):

**`earliest_refresh`**: Subscription upstreams with plan quota first, then those in overage, then API keys;
within a tier, the one whose 7-day window resets soonest; ties are random.

**`least_utilized`**: Subscription upstreams with plan quota first, then those in overage, then API keys;
within a tier, the one with the lowest 5-hour utilization; ties are random.

**`priority_random`**: Within the first tier that has an eligible upstream (tiers split by kind when
"Prioritize subscription upstreams" is on), any of them at random.

**`round_robin`**: Within the first tier that has an eligible upstream (tiers split by kind when
"Prioritize subscription upstreams" is on), each in turn; a restart starts the round again.

### Cache TTL

A pool can set the prompt cache TTL of every request it serves toward `api.anthropic.com`. The policy object
carries `enabled` (default `false`), `main` (default
`1h`) and `subagent` (default `5m`) in `5m` or `1h`, `in_overage` (default unset), and `models`, a list of
`{model, main, subagent}` overrides on canonical ids. With it on, every `cache_control` marker in the body is
rewritten to the TTL for the request's agent and model, the ones inside a tool result's own content included, the
`extended-cache-ttl-2025-04-11` beta travels exactly when the body is one hour, and the `cch` hash is restamped,
except a body the fail-closed guard refuses to edit, which goes as sent with its own markers and beta. The guard
refuses when a JSON decoder counts a different number of markers than the rewrite reached, as with a
`cache_control` key spelled with escapes or repeated in one object. On every other body the client's own
`CLAUDE_CODE_PROMPT_CACHE_TTL` has no effect through the pool. A request that carries no marker at all gets the
API's top-level `cache_control` member with the TTL instead, and a request with any marker
keeps its count. `in_overage` takes over while the write costs money: the serving upstream is in overage, or is
an API-key upstream. The TTL is priced per attempt, so a request that fails over reads its destination's state,
and a request without an agent id reads as the main conversation. Mixed TTLs across a session's requests are fine:
the cache is keyed on the prefix, and the TTL sets an entry's lifetime and price alone. The cost of each choice
is on the [keepalive page](keepalive.md#the-cost-reference).

![The Pools view: one card per pool with its strategy, affinity, overage policy, keepalive state, members, and keys][pl]
![The Pools view: one card per pool with its strategy, affinity, overage policy, keepalive state, members, and keys][pd]

[pl]: assets/pools-light.png#only-light
[pd]: assets/pools-dark.png#only-dark

## Keys

A downstream client presents a key, and a key routes into exactly one pool:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/keys \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"id":"desktop","label":"Desktop","pool_id":"main"}'
```

The secret is `phoebis-` followed by 43 characters of base64url encoding 32 random bytes. It rides in the answer to
creation and to rotation and nowhere else; the console shows it once, in a modal with a copy button.

| Field | Notes |
| --- | --- |
| `id` | required together with `label` and `pool_id`: `id, label, and pool_id are required` |
| `label` | required |
| `pool_id` | required; must name an existing pool; changing it moves the key to that pool |
| `notes` | optional |
| `enabled` | `true` by default |
| `expires_at` | unset by default; Unix milliseconds |
| `model_rules`, `model_rules_mode`, `long_context_twins` | see Model rules and the client page's models listing |
| `keepalive_opt_out` | `false` by default |
| `body_capture_hours` | unset inherits the pool's; `0` turns it off; capped by `stats.body_capture_max_hours` |

Rotate issues a new secret under the same id; a revoked key cannot be rotated. Revoke is permanent; a revoked
key cannot be re-enabled (`409`). Delete removes the key outright. The console's warning when body capture is on
for a key: "Bodies contain the prompts, file contents, and tool results of every session using this key. Only
the admin secret can read them."

![The API keys view: stat tiles and the table of keys with prefix, pool, usage, and actions][kl]
![The API keys view: stat tiles and the table of keys with prefix, pool, usage, and actions][kd]

[kl]: assets/keys-light.png#only-light
[kd]: assets/keys-dark.png#only-dark

## Model rules

A pool and a key each hold one ordered list of rules, and a request's model is judged against the key's
processed list: the key's own entries first, then the pool's when the key's `model_rules_mode` is `extend` (the
default), or the key's own entries alone under `replace`. Evaluation runs top to bottom and the first matching
entry decides: a plain entry allows the model, a `not` entry denies it, and a model no entry matches is allowed.
Both lists start empty, so nothing is excluded until an operator writes an entry. The list is first-match-wins
and nothing else, so an allowlist is a list that names what it allows and ends in a catch-all denial:

- `[claude-opus-5, NOT *]` allows the one model and blocks the rest;
- `[NOT *]` alone blocks everything.

A rule's subject is the canonical id: the request's model with the `[1m]` suffix removed, ASCII case folded, and
the dated id the catalog lists substituted once the router has resolved the spelling. One entry therefore
covers a model and its `[1m]` twin, and an entry for a model the API lists under a dated id is written as a
glob: `NOT claude-sonnet-4-5*` covers `claude-sonnet-4-5`, `claude-sonnet-4-5-20250929`, and the twin of
either. A model the list denies answers `404 model_not_allowed` without reaching an upstream.

The grammar is path-style: `*` matches any run of characters other than `/`, `**` any run including `/`, `?`
one character other than `/`, a backslash quotes the character after it, and a pattern with no wildcard matches
its own text alone. Anthropic's ids carry no slash, which is why `NOT *` reads as the catch-all above; `NOT **`
is the entry that also covers a name with a slash. The request log's client patterns
([statistics](statistics.md#analytics)) are another grammar, where `*` crosses everything. A pattern is refused
at save time when it is empty, longer than 256 characters, ends in an unquoted backslash, or holds three stars
in a row; the refusal names the entry and the reason (`model_rules[0]: pattern is empty`).

The Models view shows the processed list for a chosen key, each entry with the level it came from and the upstreams
cooling for a model it matches, then every model-scoped cooldown grouped by model with a clear button per row, and
the last rejections and permits the ledger recorded.

![The Models view: the processed rules of a key, the cooldowns by model, the recent rejections and permits][ml]
![The Models view: the processed rules of a key, the cooldowns by model, the recent rejections and permits][md]

[ml]: assets/models-light.png#only-light
[md]: assets/models-dark.png#only-dark

## Selection

Selection keys every request by `(key id, root session, canonical model)`. The router holds all routing state
and answers every request over a channel, so two concurrent first requests of the same session cannot claim
different accounts.

The eligible set starts from every account that can serve the canonical model: enabled, paired or keyed, not
`reauth_required` or `funding_required`, off cooldown, and not already tried by this request.

A pin lasts `affinity_ttl_seconds` from its last use: while it is live and its account is still eligible, the
request returns to that account, and stickiness wins over priority even across a later priority change. A pool
with affinity off (`affinity_ttl_seconds` at `0`) keeps no pins at all.

Narrowing then trims what is left: a model-specific window (a canonical id containing `fable`) at or above 1
drops the account outright, and `select_below_utilization` drops every account at or above that threshold in
5-hour utilization, unless doing so would empty the set.

What remains is ordered in three kind groups, plan quota, overage, then API key, whenever
`prefer_subscription_effective` holds; each group orders by priority ascending, and the strategy picks inside
the first non-empty tier: `earliest_refresh` ranks by the applicable weekly reset, the model's own window when
it has one, else the shared 7-day window; `least_utilized` ranks by 5-hour utilization within the tier;
`priority_random` and `round_robin` read no sample at all.

A request with no session id skips pins and stickiness: it runs the eligible set, the narrowing, and the pick
only.

A non-portable request, one whose body references a `file_id`, a `container`, or a server-tool result, is
answered by its live pin's account or not at all. With affinity off, an expired pin, or no session id, it runs
the normal selection instead, and the account-scoped reference it carries may then not resolve.

### Migration

A pool with `migrate_above_utilization` moves a pinned session off an account whose latest 5-hour or 7-day
sample has reached the threshold, onto a sibling whose 5-hour and 7-day readings are both below the threshold
minus 0.05 (`MIGRATE_BAND`), then leaves that session alone for five minutes (`MIGRATE_DWELL_MS`) so two accounts
hovering at the line do not trade it back and forth. Under `prefer_plan`, a pinned account in overage yields to
any eligible sibling that is not.

A pinned account whose window for the session's model is spent, the Fable window for a Fable session and the
shared plan window otherwise (either the 5-hour or the 7-day reading at or above 1.0), while that reading stands
(until its reset, or until the sample ages out), moves on the next request to a sibling with allowance left in
that window, without waiting for the upstream's 429. The migration band and the dwell do not apply to a spent
move: a sibling reading over the threshold but under 1.0 still receives the session. Under `prefer_plan` a
sibling in overage does not receive a spent pin, whether or not the pinned account is itself in overage, since
the overage rule would hand the session back on the next request. With no sibling holding allowance the session
stays and the upstream's answer decides.

Any move records a `pin_migrated` event with reason `cooldown` or `mark` (a failover or a credential mark moved
it), `overage`, `spent`, or `utilization`. A pin released without a move records `pin_expired` with `reasons`,
every fact the selection found against the account: `attempted`, `cooldown`, `deleted`, `expired`, `overage`,
`paused`, `spent`, `unserved`, the account's state when it cannot serve (`disconnected`, `funding_required`,
`reauth_required`, `unpaired`), or `released` when the selection found nothing else against the account, as with a
utilization move whose destination emptied. Releasing a session by hand, from the Sessions view or
`POST /admin/sessions/{key}/{session}/release`, expires its pin the same way.

## Cooldowns and failover

The relay classifies every upstream response head before a byte reaches the client, partitioned by account kind
first, `oauth`, `api_key`, or `shared` for both; within a partition the first matching row wins:

**oauth**

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 429, 5h/7d/status rejected (not Fable-only) | Account-wide until the reset, or Retry-After, or fallback | Yes |
| 429, only 7d_oi-status rejected, both shared windows OK | Model-scoped for the model, same deadline | Yes |
| 429 without unified headers | Account-wide, Retry-After or fallback | Yes |
| 200 SSE, first event `error`/`rate_limit_error` | As the 429 rows, using the response's headers | Yes |

**api_key**

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 429 with a valid Retry-After | Model-scoped until Retry-After | Yes |
| 429 without a valid Retry-After | Account-wide for the fallback duration | Yes |
| 400 `invalid_request_error`, credit balance | Marked `funding_required` until cleared | Yes |
| 200 SSE, first event `error`/`rate_limit_error` | As the 429 rows, using the response's Retry-After | Yes |

**shared** (either kind)

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 401, or 403 with `authentication_error`, Oauth | Refresh once, else `reauth_required` | Yes, post-refresh |
| 401, or 403 with `authentication_error`, api_key | `reauth_required` now | Yes |
| 403 `permission_error` | Model-scoped, one hour, reason permission; credential stays valid | Yes |
| 404 `not_found_error` naming a model | Model-scoped, `routing.model_rejection_seconds`, reason model_rejected | Yes |
| 529 | None | No; forwarded, retries |
| Other 5xx | None | No; forwarded |
| Transport failure before fully written | None | Yes, once, same account |
| Transport failure after fully written, no head | None | No; 502 `upstream_unreachable`, client retries |

`Retry-After` parsing is case-insensitive and accepts integer seconds or an HTTP date, rounding fractional
seconds up; anything non-finite, negative, malformed, or absent is invalid, and an api_key row then takes the
account-wide fallback duration instead. An HTTP date is read against the upstream's own `Date` header when one
is present, to reduce clock-skew error. A valid long delay is never shortened, and every deadline is an absolute
time with a one-second margin that survives a restart.

A 404 whose message names the model the body carried is the upstream's answer that it does not serve that model:
the account is skipped for that model for `routing.model_rejection_seconds` (an hour by default), the request goes
to the next member, and the ledger records `model_rejected` for the refusing account and `model_permitted` for the
one that served it. A 404 naming no model, and every 400 and 422, reach the client as they came.

Each client request runs at most `routing.max_attempts` wire submissions; a same-account retry after a transport
failure before the request was written counts, and the strip-and-retry does not, as the paragraph after the steps
says. Each attempt goes through these steps:

1. Ask the router. `timed` answers `429` with `Retry-After: ceil(deadline - now)` and the three unified
   rate-limit headers the client's automatic continue reads; `untimed` answers `503` with error code
   `accounts_unavailable` and no `Retry-After`, since only an operator can clear it. Both carry an
   Anthropic-shaped error body.
2. Acquire the account's credential, apply its body edits, and send on its dispatch path.
3. Classify the head. A failover row on a portable request records the cooldown, invalidates the matching pin,
   and returns to step 1; on a non-portable request it records the cooldown and commits the failed response
   instead. Any other row commits and relays the response.
4. On commit, renew the pin: the served account is final for this request.

A stream commits at its `message_start` event or its first `error` event, whichever comes first; earlier events
are buffered, up to 64 KiB, and relayed after commit in order; a stream that exceeds the buffer or sends no
committing event within 30 seconds commits as received. Every wire submission counts against
`routing.max_attempts`, including the one same-account retry after a transport failure that happened before the
request was fully written, with one exception: when an upstream answers 400 naming a beta or field the router
itself added, the router strips it and retries once on the same account, and that retry costs no attempt, since
it corrects the router's own edit rather than trying another upstream.

The Upstreams view shows what an upstream is cooling under in a cooling-down block: one line per scope, the whole
upstream or one model, with the reason, the deadline, and a clear button; the Models view lists the same rows grouped
by model.

### When nothing can serve

With every account in the pool rate limited, `phoebis` answers `429` with `retry-after`,
`anthropic-ratelimit-unified-status: rejected`, `anthropic-ratelimit-unified-reset`, and
`anthropic-ratelimit-unified-representative-claim` naming the window; a cooldown that came from a bare
`Retry-After` is rendered as `five_hour`, because the client treats an unknown claim as no claim. The error code
is `accounts_exhausted` and the message is `every account in this pool is rate limited`. For anything only an
operator can clear, such as a pool whose accounts are all paused, `phoebis` answers `503` with error code
`accounts_unavailable`, message `no account in this pool can serve requests`, and no `retry-after`. A request
admitted under a key that was deleted and created again before it was routed is answered the same way, since
the registry it was admitted against no longer holds its key.

A model every account in the pool has rejected within the rejection window answers `404` with error code
`model_rejected` and the message `every account in this pool rejected the model`; the
[cooldowns table](#cooldowns-and-failover) says how a rejection is recorded. A pool with no member answers
`503 accounts_unavailable`, since only an operator can add one.

Claude Code 2.1.269 resumes a subscriber session on its own, after a usage-limit reset, only when the `429`
carried all three of `anthropic-ratelimit-unified-status: rejected`, `anthropic-ratelimit-unified-reset` (Unix
seconds), and `anthropic-ratelimit-unified-representative-claim` naming the window (`five_hour`, `seven_day`,
`seven_day_opus`, `seven_day_sonnet`, or `overage`), with the reset at most 24 hours away. `Retry-After` alone
arms nothing, and the client also skips arming when the claim names a model-scoped window for a model the
session is not using.

## Overage

A pool's `overage_policy` chooses what happens once an upstream starts billing extra usage:

- **Prefer plan quota** (`prefer_plan`): move sessions to a sibling with plan quota when this upstream starts
  billing extra usage.
- **Allow** (`allow`): ignore extra usage.
- **Never** (`never`): treat an upstream in extra usage as unavailable until its window resets.

An account is in overage when its latest quota evidence says so, and only newer evidence, by the upstream's own
clock, replaces that state. A response head sets the flag when `anthropic-ratelimit-unified-overage-in-use` is
true, and clears it when the header is present and false, or absent while the shared window headers are
present. A usage sample sets the flag when `extra_usage.is_enabled` is true and the 5-hour or 7-day utilization
has reached 1.0, and clears it when both shared windows are below 1.0. An account with extra usage enabled never
answers `429` once its plan quota empties: it keeps serving with the flag set, so `prefer_plan` and `never` act
through this flag rather than through a cooldown.
