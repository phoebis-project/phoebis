# Pools, keys, routing, and overage

A pool holds one or more accounts, each with a priority; a key routes into exactly one pool; every request a key
presents is placed on one account of that pool by a selection that honours the session's pin, the pool's strategy,
and each account's quota. This page describes every field of an account, a pool, and a key, then the selection
itself, and the answers a client gets when nothing can serve it.

## Catalogs

An upstream's model list is read live from `GET /v1/models` on `api.anthropic.com` with the upstream's own credential,
at startup, on the refresh interval, and on demand, and held for `routing.catalog_cache_seconds` (sixty by default)
from the moment the answer arrives; the Upstreams view's catalog popup, the rule editors' suggestions, and the
proxy's [models listing](client.md#the-models-listing) all read that cache, and `GET /admin/accounts/{id}/catalog`
answers it with `?refresh=1` to read again. Nothing about routing depends on it: every enabled member of a pool is a
candidate for any model the rules allow, and an upstream that does not serve a model says so with a `404` the router
records as a rejection ([cooldowns](#cooldowns-and-failover)). An upstream whose fetch failed contributes nothing to a
listing until the next fetch, and the popup shows the error.

The catalog lists some models under a dated id (`claude-sonnet-4-5-20250929`) while Claude Code asks for the undated one
(`claude-sonnet-4-5`). An undated spelling is an alias for the newest dated snapshot the listing holds, so the router
derives the dated id from the listings it caches: the newest `<spelling>-YYYYMMDD` entry across every enabled upstream,
else the spelling itself (a dateless id such as `claude-sonnet-4-6` is its own snapshot). The listings are fetched at
startup for every enabled upstream before the proxy listener accepts requests (waiting `routing.startup_fetch_seconds`,
five by default, at most; an upstream still fetching then finishes in the background), and a request's model is resolved
by a lookup in that cache, never by a request of its own. The dated id is the canonical name the rules, the ledger, the
pricing, the pins, and the cooldowns use for that spelling from the session's first request; the body toward the
upstream keeps the client's spelling. A resolution is kept across restarts: the first request after a restart carries
the dated id from the stored mapping even before the listings arrive, and only a fresh installation whose upstreams are
unreachable at startup runs requests under the spelling, until the listing arrives. A listing that later lacks the dated
entry never takes a resolution back.

The listings are refreshed every `routing.catalog_refresh_seconds` (six hours by default), once more within a minute
of an upstream answering under a different snapshot than the router resolved (the alias moved), and on demand from
the Upstreams view's catalog popup and `GET /admin/accounts/{id}/catalog?refresh=1`. A refresh that finds a newer
snapshot for a spelling names the next requests by it; the pins and cooldowns already keyed on the previous dated id
stay where they are, since a snapshot is a model of its own, and the stored mapping is updated in the same transaction.

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
| `strategy` | `pace` | `pace`, `priority`, or `round_robin` |
| `prefer_subscription` | `true` | see below |
| `affinity_ttl_seconds` | `3600` | `0` or at least 60: `affinity_ttl_seconds is 0 or at least 60` |
| `affinity_mode` | `always` | `always` or `when_cached`; see below |
| `subagent_placement` | `by_cohort` | `by_cohort`, `by_strategy`, or `with_main`; see Selection |
| `session_placement` | `by_strategy` | `by_strategy` or `by_warm_prefix`; see Selection |
| `min_shared_prefix_tokens` | `20000` | at least 0; see Selection |
| `overage_policy` | `prefer_plan` | `prefer_plan`, `allow`, or `never` |
| `overage_move_grace_minutes` | `30` | 0 to 300; see Moves |
| `migrate_above_utilization` | unset | 0 to 1: `migrate_above_utilization is between 0 and 1`; starts shedding |
| `shed_share` | `0.5` | 0 to 1 |
| `burn_window_seconds` | `600` | at least 60 |
| `source_dwell_seconds` | `900` | at least 60 |
| `migrate_when_spent` | `true` | `true` or `false` |
| `warm_sessions` | `when_needed` | `never`, `when_needed`, or `always` |
| `retry_in_place_seconds` | `10` | 0 to 300; see Retry in place |
| `in_flight_cap` | `16` | at least 1; see In-flight caps |
| `pace_floor` | `0.05` | 0 to 1; see Selection |
| `select_below_utilization` | unset | 0 to 1: `select_below_utilization is between 0 and 1` |
| `model_rules` | empty | an ordered list of `{pattern, not}` entries, at most 256; see Model rules |
| `keepalive` | disabled | a policy object; its defaults are on the [Keepalive page](keepalive.md) |
| `cache_ttl` | off | a policy object, `inject_when_absent` off by default; see below |
| `cache_diagnostics` | unset (inherits) | `true`, `false`, or `null` |
| `quota_headers` | `pass` | `hide` or `pass`; see below |
| `body_capture_hours` | `0` (off) | `body_capture_hours exceeds stats.body_capture_max_hours` |
| `wind_down_minutes` | `30` | 0 to 10,080 (a week) |
| `members` | none | create only; replaced wholesale by `PUT /admin/pools/{id}/members` |

`cache_diagnostics` forces the cache-diagnosis beta and the `previous_message_id` member toward
`api.anthropic.com` for this pool's requests, whatever the [bootstrap option](installation.md#compatibility)
says; `null` inherits it.

On `hide` every answer but a 429 loses the `anthropic-ratelimit-unified-*` family, so an organization-wide quota
never reaches a key holder as its own; a 429 keeps the family so the client's continue-when-reset arms;
`request-id` passes either way, and the client shows no notice for the hidden family. The upstream's own
`anthropic-organization-id` and `set-cookie` are never forwarded to a key holder.

Under `when_cached` a request that carries no `cache_control` marker, and receives none from the cache TTL
policy, creates no pin and lands by the strategy instead of pinning; a non-portable request keeps its pin
either way.

A member's `priority` is a number; lower sorts first. Naming an unknown account in `members` answers `404
account {id} not found`, and the pool is not created.

An API-key member carries a spend `budget_usd`, unset by default (no cap), and a `budget_period` of `day` or `month`
(default `month`, the UTC calendar day or month); a budget on any other member answers `400 budget_usd applies to an
API-key member`, since a subscription member's usage is metered against its plan windows and its overage is not a
per-member budget. A member whose spend for the current period reaches its budget is skipped for requests until the
period ends, and a keepalive probe pinned to it waits rather than moving the pin. A pool whose every member is at its
budget answers `429` with the `overage` claim, the same one an account in overage carries. The spend sums the member's
hourly rollups, and the rows of an active budget period outlive `stats.rollup_retention_days` until that period ends.

`prefer_subscription` orders the set by kind (subscription accounts with plan quota, then those in overage, then
API keys) before priority; it is the operator's answer alone, on by default, and no strategy overrides it. A pool
with keys cannot be deleted: `409 pool still has keys`.

Each strategy's rule, as the console states it on the pool card (the pool dialog's "Prioritize subscription
upstreams" toggle sets `prefer_subscription`):

**`pace`**: Within the first tier that has an eligible upstream, the one with the most remaining weekly allowance
per hour until its window resets or its subscription ends, whichever comes first; an unsampled upstream counts as
untouched; ties are random.

**`priority`**: Within the first tier that has an eligible upstream, any of them at random.

**`round_robin`**: Within the first tier that has an eligible upstream, each in turn; a restart starts the round
again.

### Cache TTL

A pool can set the prompt cache TTL of every request it serves toward `api.anthropic.com`. The policy object carries
`enabled` (default `false`), `inject_when_absent` (default `false`), and two rule sets, `plan` and `metered`: `plan`
prices a subscription upstream on plan quota, `metered` an API-key upstream or a subscription upstream in overage. Each
set carries `main` and `subagent` in `5m` or `1h`, `keepalive` (the TTL a keepalive probe's replay writes with), and
`models`, a list of `{model, main, subagent}` overrides on canonical ids. `plan` defaults to one hour throughout and
`metered` to five minutes throughout: a one-hour write costs a metered upstream 2x base input against 1.25x for five
minutes, and a plan-funded upstream the same plan tokens either way. With the policy on, every `cache_control` marker in
the body is rewritten to the TTL the request's set and branch choose, the ones inside a tool result's own content
included, the `extended-cache-ttl-2025-04-11` beta travels exactly when the body is one hour, and the `cch` hash is
restamped, except a body the fail-closed guard refuses to edit, which goes as sent with its own markers and beta. The
guard refuses when a JSON decoder counts a different number of markers than the rewrite reached, as with a
`cache_control` key spelled with escapes or repeated in one object. On every other body the client's own
`CLAUDE_CODE_PROMPT_CACHE_TTL` has no effect through the pool. A body that carries no marker is left as it came unless
`inject_when_absent` is on, in which case the top-level `cache_control` member is added with the TTL, before `stream`; a
token count is never edited by the policy: its markers travel as the client sent them and no beta follows it. The
metered set applies while the write costs money: the serving upstream is in overage, or is an API-key upstream. A
keepalive probe's replay writes with the chosen set's `keepalive` TTL; any other request reads `main` or `subagent` by
its agent, and one without an agent id reads as the main conversation. The TTL is priced per attempt, so a request that
fails over reads its destination's state. Mixed TTLs across a session's requests are fine: the cache is keyed on the
prefix, and the TTL sets an entry's lifetime and price alone.
The cost of each choice is on the [keepalive page](keepalive.md#the-cost-reference).

![The Pools view: one card per pool with its strategy, affinity, overage policy, keepalive state, members, and keys;
the codex pool reads exhausted because its one member is at its daily budget][pl]
![The Pools view: one card per pool with its strategy, affinity, overage policy, keepalive state, members, and keys;
the codex pool reads exhausted because its one member is at its daily budget][pd]

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
| `affinity_source` | `auto`, `session`, `user_id`, `content`, or `whole_key`, default `auto`; see [Clients](#clients) |

Rotate issues a new secret under the same id; a revoked key cannot be rotated. Revoke is permanent; a revoked
key cannot be re-enabled (`409`). Delete removes the key outright. The console's warning when body capture is on
for a key: "Bodies contain the prompts, file contents, and tool results of every session using this key. Only
the admin secret can read them."

The key form sets `affinity_source` as
**Affinity source** (Automatic, Session id, User id, Conversation content, or Whole key) in its General
section after Requests in flight.

![The API keys view: stat tiles and the table of keys with prefix, pool, usage, and actions][kl]
![The API keys view: stat tiles and the table of keys with prefix, pool, usage, and actions][kd]

[kl]: assets/keys-light.png#only-light
[kd]: assets/keys-dark.png#only-dark

## Clients

Every request carries a class, read off its user agent: `claude-code/<entrypoint>` for a `claude-cli/` caller (the
entrypoint token it names, `cli` when it names none)
and `other` for anything else.

A conversation is identified by its session header: the session id the client sends names it.
A key's `affinity_source` fixes one of these rungs instead of walking the whole
ladder, and a request that does not fit a fixed rung has no identity and no pin. A new session id whose messages
continue another conversation of the same key keeps its own identity, and is recorded as a fork of that
conversation at the depth where the two part.

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
the dated id the catalog lists substituted. One entry therefore covers a model and its `[1m]` twin, and an entry
for a model the API lists under a dated id is written as a glob: `NOT claude-sonnet-4-5*` covers
`claude-sonnet-4-5`, `claude-sonnet-4-5-20250929`, and the twin of either. An entry that spells an undated id
exactly (`claude-sonnet-4-5`) never matches, because the router names that model by its dated snapshot, so an
exact entry for an undated model is written as a glob (`claude-sonnet-4-5*`) too, and the rule editors and the
Models view say so beside the list. A model the list denies answers `404 model_not_allowed` without reaching an
upstream.

The grammar is path-style: `*` matches any run of characters other than `/`, `**` any run including `/`, `?`
one character other than `/`, a backslash quotes the character after it, and a pattern with no wildcard matches
its own text alone. Anthropic's ids carry no slash, which is why `NOT *` reads as the catch-all above; `NOT **`
is the entry that also covers a name with a slash. The request log's client patterns
([statistics](statistics.md#analytics)) are another grammar, where `*` crosses everything. A pattern is refused
at save time when it is empty, longer than 256 characters, ends in an unquoted backslash, or holds three stars
in a row; the refusal names the entry and the reason (`model_rules[0]: pattern is empty`).

The Models view shows the processed list for a chosen key, each entry with the level it came from and the upstreams
cooling for a model it matches, and a note that an entry for an undated model is written with a trailing `*`; then
every model-scoped cooldown grouped by model with a clear button per row, and the last rejections and permits the
ledger recorded.

![The Models view: the processed rules of a key, the cooldowns by model, the recent rejections and permits][ml]
![The Models view: the processed rules of a key, the cooldowns by model, the recent rejections and permits][md]

[ml]: assets/models-light.png#only-light
[md]: assets/models-dark.png#only-dark

## Selection

Selection keys every request by `(key id, root session, agent, canonical model)`. The router holds all routing
state and answers every request over a channel, so two concurrent first requests of the same session cannot claim
different accounts. The identity a pin is held under is the session header [Clients](#clients) names.

A fresh request with no live pin lands where its prefix is already warm. The deepest message boundary any account still
holds wins first, for a main conversation and a subagent alike. Failing that, the deepest static boundary places a
subagent under `by_cohort` or a main conversation under `by_warm_prefix`, when the shared prefix, sized by the relay's
estimate from the request's bytes, is at least `min_shared_prefix_tokens`, the request carries or receives a cache
marker, and the account is not at its cap, at or above the ceiling (`select_below_utilization`), at or above the migrate
threshold less its 0.05 band (`migrate_above_utilization`), under the Pace floor, or winding down. A cooling account
never reaches either rung, since the eligible set excludes it; a spent account is in the eligible set, and both rungs
refuse it through the allowance check. A subagent under `with_main` lands its first request on its parent agent's pin in
place of the static walk, and one under `by_strategy` skips the static walk; every case that finds no live match lands
by the strategy. A held request of a subagent cohort waits where its cohort already is rather than on the strategy's own
pick; a session already on an account, under any of its agents, is never held while that account can still serve. A
token count is never placed and takes neither exemption from a hold: its pin or the strategy decides it.

The eligible set starts from every account that can serve the canonical model: enabled, paired or keyed, not
`reauth_required` or `funding_required`, off cooldown, and not already tried by this request.

A pin is held per agent of a conversation, where a conversation is the session id
or the conversation's own messages. It lasts `affinity_ttl_seconds` from its last
use: while it is live and its account is still eligible, the request returns to that account, and stickiness wins over
priority even across a later priority change. A pool with affinity off (`affinity_ttl_seconds` at `0`) keeps no pins
at all.

Narrowing then trims what is left. A model-specific window (a canonical id containing `fable`) at or above 1 drops the
account outright, and an account already at the pool's `in_flight_cap` takes no fresh placement, ahead of every
preference below; with every account at its cap the request is held for a slot instead (see **In-flight caps**, below).
The rest are preferences that fall back to the wider set rather than stranding the request: `select_below_utilization`
drops every account at or above that threshold in 5-hour utilization, and an account winding down toward its
subscription end (see **Subscription end**, below) is skipped for a fresh placement too.

What remains is ordered in tiers. With `prefer_subscription` on, the set splits into three kind groups walked in order,
plan quota, overage, then API key; within a group, or across the whole set with the flag off, tiers order by priority
ascending, then, under `prefer_plan`, an account not in overage before one in overage. The strategy picks inside the
first non-empty tier. `pace` reads each account's sample as a window map: a subscription account scores by remaining
allowance in its own window (the model's own window when it has one, else the shared 7-day window) over the hours to the
earlier of that window's reset and the account's derived end; an api_key account scores by one minus its worst live
rate-limit bucket's utilization alone, with no division by hours, since a bucket refills within a minute and its own
reset never enters the score. Either way a window nobody has sampled, or one whose reset has already passed, reads as
untouched: remaining 1 over the full week. Ties are random. An account whose remaining allowance is at or below the
pool's `pace_floor` is kept as the last resort, ranked by the same score among the others below it, once no account
above the floor remains; `priority` and `round_robin` read no sample and no floor at all. A `pace` pool fetches a
subscription account's usage at selection once its sample is older than the sample interval, whatever its other options,
since a quiet account brings no response head of its own to refresh it.

A request with no session id skips pins and stickiness: it runs the eligible set, the narrowing, and the pick
only.

A non-portable request, one whose body references a `file_id`, a `container`, or a server-tool result, is
answered by its live pin's account or not at all. With affinity off, an expired pin, or no session id, it runs
the normal selection instead, and the account-scoped reference it carries may then not resolve.

### Moves

A live pin on a portable request moves only when its session sends a request, and only by one of five rules,
checked in this order: spent, overage, return to plan, drain, and shed. The first rule that finds a destination
moves the pin and names the move after itself. A pin on an account past its subscription end reaches none of
them: the `subscription_ended` exclusion releases it, as a cooldown releases a cooled account's pin.

With `migrate_above_utilization` set, every rule prefers a cool destination, so a moved session does not land where the
next round would shed it again. A cool sibling reads below the threshold minus 0.05 (`MIGRATE_BAND`) on every reading it
holds (the 5-hour and 7-day windows of a subscription account, the worst live rate-limit bucket of an API-key account;
an unsampled account reads as under), has run no shedding round of its own within `source_dwell_seconds`, and has room
under the pool's `in_flight_cap`. When no destination of a rule is cool, the rule still moves the pin to one of them, so
a full cool sibling never keeps a free hot one from the session; only the shed rule insists on a cool one.

**Spent** The pinned account's window for the session's model is spent: the Fable window for a Fable session and the
shared plan window otherwise (either the 5-hour or the 7-day reading at or above 1.0), while that reading stands (until
its reset, or until the sample ages out). With `migrate_when_spent` on (the default), the pin moves on this request to a
sibling that holds allowance in that window, without waiting for the upstream's 429. The shedding budget does not apply,
and the band and the dwell only order the siblings: one reading over the threshold but under 1.0 still receives the
session when no cool one can. The router has to know the sibling's allowance: an API-key account has no plan window to
spend, and any other sibling needs a sample younger than the sample interval, so the toggle has the pool sample its
accounts. After a restart, until those samples land, a spent session stays and the upstream's answer decides. Under
`prefer_plan` a sibling in overage does not receive a spent pin, since the overage move would hand the session straight
back. With the toggle off, or no sibling holding allowance, the session stays.

**Overage** Under `prefer_plan`, a pin on a subscription account in overage returns to plan quota: warm or cold, it
moves to a subscription sibling that is not in overage, never to an API key. The sibling has to hold allowance too,
since a spent one would move the session again on its next request. It waits while the reset the overage evidence names
falls within `overage_move_grace_minutes` (default 30, `0` for no wait), so a session whose account gets its plan back
soon keeps its cache and moves at most once. Evidence that names no reset moves the pin at once.

**Return to plan** A pin on an API-key account moves to a subscription sibling that holds allowance and is not in
overage once its cache is cold, or once it has sat on the key for `source_dwell_seconds` since it landed there. An
API key names no reset, so no grace applies.

**Drain** A cold pin on an account winding down toward its subscription end moves to an eligible sibling that is
not winding down. A warm one stays (see **Subscription end**, below).

**Shed** With `migrate_above_utilization` set, a request pinned to an account that reads at or above the
threshold on its 5-hour or 7-day window starts a shedding round there, at most once per `source_dwell_seconds`.
The round orders every pin on the account, cold ones first by burn, then warm ones by burn over rewrite cost (the
pin's last prompt size in tokens), and marks them in that order until the marked burn reaches `shed_share`
(default 0.5) of the account's burn over `burn_window_seconds` (default 600). It never marks the account's last
unmarked pin, so an account with one session sheds nothing and no round takes all of an account's burn. A pin
whose burn carries the total past the share stays marked, and the round stops there. `warm_sessions` bounds the
order: `never` stops after the cold pins, `when_needed` (the default) goes on to the warm ones, and `always`
orders every pin by burn over rewrite cost, cold or warm.

The console's Pools form offers `shed_share`, `burn_window_seconds`, and `source_dwell_seconds` together as three
presets beside custom: balanced (0.5, 600 s, 900 s, the default), fast (0.5, 300 s, 600 s), and conservative
(0.3, 900 s, 1800 s).

A marked session moves at its next request, onto a cool sibling (see above) that did not shed this pin within
`source_dwell_seconds`. A marked pin whose account is cooling for the session's model is released instead, with
`cooldown` among its reasons, and its mark goes with it; the round on the account still runs, since a cooldown can hold
one model alone. An unmarked pin never moves for utilization. The marks live in memory for the dwell: a marked session
that stays idle never moves, no new round runs on the account until the dwell passes, and a restart forgets the marks,
so the next request after it may start a fresh round.

**Warmth** A pin stays warm while its last refresh is younger than the cache TTL in force for it. A `main`, `subagent`
or `compaction` request ([`x-claude-code-request-class`](client.md#claude_code_gateway_hint_headers)) refreshes it, and
any other class leaves it alone. Without the class header, a request whose messages extend the session's conversation
refreshes the pin; one that repeats the session's static prefix (tools and system prompt) without extending its messages
restarted the conversation and reads cold; one that matches neither changes nothing. Where one session holds several
conversations, a request that extends any
conversation the pin has served refreshes it.
A request that carries `x-claude-code-context-compacted` reads cold whatever else it
says, so a compacted session goes first in the round's order on the request that carries the fact. A body the router
cannot read refreshes the pin on every request. A keepalive probe replays another request's headers and body, so
the router judges none of them: a probe never colds a session, and its renewal counts as a refresh. A `count_tokens`
request creates no cache entry, so it neither refreshes nor colds the pin and leaves its cache TTL as it was.

**Burn** Each pin keeps a moving average of its priced usage over `burn_window_seconds` twice, in plan-meter
tokens and in dollars, and the round reads the one the account's funding selects: plan-meter tokens while a
subscription account runs on its plan, dollars on an API-key account or a subscription account in overage. Every
renewal folds both, so leaving overage switches the reading back without a reset. A keepalive probe's usage never
counts.

Any move records a `pin_migrated` event with reason `cooldown`, `drain`, `mark`, `overage`, `return_to_plan`, `shed`, or
`spent`: `cooldown` and `mark` when a failover or a credential mark moved the session, the others after the rule above
that moved it. A pin released without a move records `pin_expired` with `reasons`, every fact the selection found
against the account: `attempted`, `cooldown`, `deleted`, `expired`, `overage`, `paused`, `spent`, `subscription_ended`,
`unserved`, the account's state when it cannot serve (`disconnected`, `funding_required`, `reauth_required`,
`unpaired`), or `released` when the selection found nothing else against the account, as with a move whose destination
emptied. A pin whose creating request was answered 400, 401, 403, or 404, with no cooldown or mark on the account and no
other turn of the session renewing it meanwhile, records the single reason `client_error`: it never served, so the
session's next request is placed afresh rather than moved. Releasing a session by hand, from the Sessions view or
`POST /admin/sessions/{key}/{session}/release`, expires its pin the same way.

## Cooldowns and failover

The relay classifies every upstream response head before a byte reaches the client, partitioned by account kind
first, `oauth`, `api_key`, or `shared` for both; within a partition the first matching row wins:

**oauth**

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 429, 5h/7d/status rejected, unless a row below matches | Account-wide until reset, Retry-After, or fallback | Yes |
| 429, 7d_oi-status rejected, 5h/7d healthy | Model-scoped for the model, same deadline | Yes |
| 429, `status` rejected under a `seven_day_` claim, 5h/7d healthy | Model-scoped for the model, same deadline | Yes |
| 429 without unified headers | Account-wide, Retry-After or fallback | Yes |
| 200 SSE, first event `error`/`rate_limit_error` | As the 429 rows, using the response's headers | Yes |

A shared window is healthy when its status reads `allowed` or `allowed_warning`, or when the status is absent and the
window's own utilization reads below 1.0; an absent status with no utilization, or any other status value, is not
evidence that the window is fine, and the rejection cools the whole account.

**api_key**

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 429 naming `enforced_spend_limit_reached` | Account-wide until the first of next month, reason `spend_cap` | Yes |
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

An API-key account whose monthly spend cap is reached answers a 429 naming `enforced_spend_limit_reached`; the
account is cooled as a whole until the first of the next month, reason `spend_cap`, and an exhaustion it causes
claims `overage`.

A response head also lands a selection sample on the account it answered for, whichever header family it carries: an
OAuth head's unified `5h` and `7d` windows, and its `7d_oi` utilization when the response reports one; an api_key head's
`anthropic-ratelimit-{requests,input-tokens,output-tokens}-{limit,remaining,reset}` headers, one bucket per canonical
model. A later head for another model adds that model's buckets beside the ones already on file rather than replacing
them. An OAuth head that reads both shared windows, and every other window the account's sample already holds, stands in
for the router's own usage fetch: the account is fetched again only once its latest sample, from a head or a fetch, is
older than the sample interval. A head that leaves a known window unread, such as a Fable reading the response does not
report, leaves the fetch due, so that window is still re-read while the account is busy.

A 404 whose message names the model the body carried is the upstream's answer that it does not serve that model:
the account is skipped for that model for `routing.model_rejection_seconds` (an hour by default), the request goes
to the next member, and the ledger records `model_rejected` for the refusing account and `model_permitted` for the
one that served it. A 404 naming no model, and every 400 and 422, reach the client as they came.

Each client request runs at most `routing.max_attempts` wire submissions; a same-account retry after a transport
failure before the request was written counts, and the strip-and-retry does not, as the paragraph after the steps
says. Each attempt goes through these steps:

1. Ask the router. `timed` answers `429` with `Retry-After: ceil(deadline - now)` and the three unified rate-limit
   headers the client's automatic continue reads; `untimed` answers `503` with error code `accounts_unavailable` and no
   `Retry-After`, since only an operator can clear it, unless the request reached it through holds alone with nothing
   dialled: then every account is at its cap, and the answer is `429` with error code `accounts_at_cap`. Each carries
   an Anthropic-shaped error body. `held` waits for a slot and asks again ([in-flight caps](#in-flight-caps)).
2. Acquire the account's credential, apply its body edits, and send on its dispatch path.
3. Classify the head. A short throttle on a portable request is waited out on the same account
   ([retry in place](#retry-in-place)). Any other failover row on a portable request records the cooldown, invalidates
   the matching pin, and returns to step 1; on a non-portable request it records the cooldown and commits the failed
   response instead. Any other row commits and relays the response.
4. Once the committed response ends, renew the pin with the usage it carried: the served account is final for this
   request. A pin this request created and whose answer is `400`, `401`, `403`, or `404` is released instead, since
   it never served, unless another turn of the session renewed it meanwhile; a `429` keeps the cooldown path of step 3.

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

The console's Pools form offers `in_flight_cap`, `retry_in_place_seconds`, and `pace_floor` together as three
presets beside custom: balanced (16, 10 s, 5 %, the default), fast (8, 5 s, 10 %), and conservative (16, 20 s,
5 %); balanced and conservative differ in the retry-in-place bound alone.

### Retry in place

A throttle on a portable request whose `Retry-After` is at or under the pool's `retry_in_place_seconds` (default 10) is
waited out on the same account and retried there once, with the same credential. The cooldown row is still recorded, so
a sibling request selecting meanwhile skips the account, and the session's pin stays where it is. The retry is a wire
submission like any other and costs one attempt of `routing.max_attempts`, so it runs only while the budget has one
left; without one, the throttle is the client's answer. A second throttle on the retry, a `Retry-After` past the bound,
a throttle that names no `Retry-After`, and a request bound to its account all take the failover path above.

### In-flight caps

**Per account.** Every wire submission counts against its account from its charge until its response ends: a buffered
answer when it is returned, a stream for its whole length, whether it finishes, fails, or the client hangs up. The first
attempt on an account is charged by the selection that chose it, so two selections answered together never read the same
free slot; a retry on the same account is charged at its dispatch. The count is kept by the router itself; no report is
involved. An account at the pool's `in_flight_cap` (default 16) is passed over while a sibling is under it, even a
sibling above `select_below_utilization`: the cap is a bound, the ceiling a preference. Only when every account that
could serve is at its cap is the request held: it waits up to `retry_in_place_seconds` for a slot on the account it
would have chosen and lands there when one frees; past the bound it is selected again with that account passed over, and
with every account passed over it is answered `429` with the error type `rate_limit_error`, the error code
`accounts_at_cap`, the message `every account in this pool is at its in-flight cap`, and a `Retry-After` of the pool's
`retry_in_place_seconds` held between 1 and 19 seconds: the client waits out a `Retry-After` under 20 seconds and treats
a longer one as a block it does not retry. A hold costs one attempt of `routing.max_attempts`, and a hold that would
leave none for the dial is answered the same `429` at once. A request that dialled an account before its holds ran out,
and a pool with no account to hold for, keep the `503 accounts_unavailable` below. A request whose session already
holds a pin under any agent is never held, and a move whose every destination is at its cap is not made: the pin
renews in place and the next request judges the move again; a held subagent waits where its cohort is.

**Per key.** A key's own `in_flight_cap`, unset by default, counts each Messages request once, from its admission,
before anything is selected, until its response ends. A keepalive probe and a `count_tokens` request take no slot and
are never refused for want of one: the probe is the router's own traffic, and a token count is no conversation turn. The
count is held across every attempt, retry, and hold, so a request sleeping in a retry in place keeps its key's slot. A
request past the cap is refused before it reaches an account: `429` with the error type `rate_limit_error`, the error
code `key_in_flight_cap`, and a `Retry-After` of the pool's `retry_in_place_seconds`, one second when that is 0. The
refused request is observed with its body and gets its own request row under that code.

### When nothing can serve

With every account in the pool rate limited, `phoebis` answers `429` with `retry-after`,
`anthropic-ratelimit-unified-status: rejected`, `anthropic-ratelimit-unified-reset`, and
`anthropic-ratelimit-unified-representative-claim` naming the window; a cooldown that came from a bare `Retry-After` is
rendered as `five_hour`, because the client treats an unknown claim as no claim. The error code is `accounts_exhausted`
and the message is `every account in this pool is rate limited`. With every account at its in-flight cap past the hold,
`phoebis` answers `429` with error code `accounts_at_cap` and a `Retry-After` of at most 19 seconds
([in-flight caps](#in-flight-caps)), since the pool frees on its own. For anything only an operator can clear, such as a
pool whose accounts are all paused, `phoebis` answers `503` with error code `accounts_unavailable`, message
`no account in this pool can serve requests`, and no `retry-after`. A request admitted under a key that was deleted, or
deleted and created again under its id, before it was routed is refused with the `401` admission answers a key the
registry does not hold (see [What the proxy accepts](client.md#what-the-proxy-accepts)). A pool whose every member has
reached its spend budget answers `429` with the `overage` claim, the same one an account in overage carries (see
[Pools](#pools)), and a probe pinned to a member at its budget is answered with the budget period's end as its
reset, whatever its siblings hold.

A model every account in the pool has rejected within the rejection window answers `404` with error code
`model_rejected` and the message `every account in this pool rejected the model`; the
[cooldowns table](#cooldowns-and-failover) says how a rejection is recorded. A pool with no member answers
`503 accounts_unavailable`, since only an operator can add one.

Claude Code 2.1.269 resumes a subscriber session on its own, after a usage-limit reset, only when the `429`
carried all three of `anthropic-ratelimit-unified-status: rejected`, `anthropic-ratelimit-unified-reset` (Unix
seconds), and `anthropic-ratelimit-unified-representative-claim` naming the window (`five_hour`, `seven_day`,
`seven_day_overage_included`, `seven_day_opus`, `seven_day_sonnet`, or `overage`), with the reset at most 24
hours away. `Retry-After` alone arms nothing, and the client also skips arming when the claim names a model-scoped
window for a model the session is not using.

## Overage

A pool's `overage_policy` chooses what happens once an upstream starts billing extra usage:

- **Prefer plan quota** (`prefer_plan`): a pinned session on this upstream moves to a plan-funded subscription
  sibling with allowance, warm or cold, once the reset the overage evidence names is further away than
  `overage_move_grace_minutes` (default 30); an API-key sibling is never its destination, since a move onto an API
  key is kept for a hard reason. A hard reason on the account instead, such as a spent window or a cooldown, takes
  the move it already has, which may land on an API-key sibling. An API-key member returns to plan quota the same
  way, independent of this policy, once its pin is cold or has sat on the key for `source_dwell_seconds` (see
  [Moves](#moves)).
- **Allow** (`allow`): ignore extra usage.
- **Never** (`never`): treat an upstream in extra usage as unavailable until its window resets.

An account is in overage when its latest quota evidence says so, and only newer evidence, by the upstream's own
clock, replaces that state. A response head sets the flag when `anthropic-ratelimit-unified-overage-in-use` is
true, and clears it when the header is present and false, or absent while the shared window headers are
present. A usage sample sets the flag when `extra_usage.is_enabled` is true and the 5-hour or 7-day utilization
has reached 1.0, and clears it when both shared windows are below 1.0. An account with extra usage enabled keeps
serving with the flag set once its plan quota empties, so `prefer_plan` and `never` act through this flag rather than
through a cooldown. Once the organisation's spend cap is reached it answers `429` with the overage window rejected and
the claim `seven_day_overage_included`; both shared windows still read healthy, so the cooldown path scopes that
rejection to the model.

Whether the flag still counts is one rule every reader applies alike, the selector and the keepalive scheduler
included: an account counts as in overage while the reset its evidence names is ahead of now; evidence that names
none stands for the cooldown fallback (`routing.cooldown_fallback_seconds`) past its observation, and is then
re-read against the same rule.

## Subscription end

An operator can mark an OAuth account cancelled and save the end it stops serving at; the account form prefills
that end from the profile's start, or a trial end already passed, advanced to the next occurrence of the same day
of the month and time of day, editable before it is saved. A trial end still ahead of now counts as an end the
same way, even on an account nobody has cancelled.

Inside `wind_down_minutes` (default 30) of its end, an account takes a fresh session only as the last resort, and
a cold pin moves off it once another account can serve; a warm session stays (the **Drain** rule of
[Moves](#moves)). Past the end, a cancelled account is the obstacle `subscription_ended`, and the upstream refuses
the now-dead credential, so the session's next request fails over to a sibling.
