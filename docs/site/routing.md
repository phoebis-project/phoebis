# Pools, keys and routing

A pool holds one or more accounts, each with a priority, an account belongs to one pool, and a key routes into exactly
one pool. Every request a key presents is placed on one account of that pool by a selection that honors the session's
pin, the pool's strategy and each account's quota. This page is for the operator tuning pools and keys and reading why
a request landed where it did: every field of a pool and a key with its default and bounds, the client classes, the
model rules, the selection itself, the cooldowns and the failover, and the answers a client gets when nothing can serve
it. The views that edit these settings are on [The console](console.md), and the codes a refused client meets are on
[Refusal codes](client.md#refusal-codes).

## Catalogs

An account's model list is read live from `GET /v1/models` on `api.anthropic.com` with the account's own credential, at
startup, on the refresh interval and on demand. The listing is held until the scheduled refresh
(`routing.catalog_refresh_seconds`) or an eviction ends it, and a failed fetch for 60 seconds from the moment its
answer arrives. The Accounts view's catalog popup ([Accounts](console.md#accounts)), the rule editors' suggestions and
the proxy's [models listing](client.md#the-models-listing) all read that cache, and `GET /admin/accounts/{id}/catalog`
answers it, with `?refresh=1` to read again. Nothing about routing depends on it: every enabled member of a pool is a
candidate for any model the rules allow, and an upstream that does not serve a model to an account says so with a
`404` the router records as a rejection ([cooldowns](#cooldowns-and-failover)). An account whose fetch failed
contributes nothing to a listing until the next fetch, and the popup shows the error.

The catalog lists some models under a dated id (`claude-sonnet-4-5-20250929`) while Claude Code asks for the undated
one (`claude-sonnet-4-5`). An undated spelling is an alias for the newest dated snapshot the listing holds, so the
router derives the dated id from the listings it caches: the newest `<spelling>-YYYYMMDD` entry across every enabled
account, else the spelling itself (a dateless id such as `claude-sonnet-4-6` is its own snapshot). The listings are
fetched at startup for every enabled account before the proxy listener accepts requests, waiting
`routing.startup_fetch_seconds` (default `30`) at most, and an account still fetching then finishes in the background.
A request's model is resolved by a lookup in that cache, never by a request of its own. The dated id is the canonical
name the rules, the ledger, the pricing, the pins and the cooldowns use for that spelling from the session's first
request, and the body toward the upstream keeps the client's spelling. A resolution is kept across restarts: the first
request after a restart carries the dated id from the stored mapping even before the listings arrive, and only a fresh
installation that reaches no listing at startup runs requests under the spelling, until the listing arrives. A listing
that later lacks the dated entry never takes a resolution back.

The listings are refreshed every `routing.catalog_refresh_seconds` (default `21600`, six hours), once more within a
minute of an upstream answering under a different snapshot than the router resolved (the alias moved), and on demand
from the Accounts view's catalog popup and `GET /admin/accounts/{id}/catalog?refresh=1`. The popup lists the effort
levels each model reports supporting, when the listing names them. A refresh that finds a newer snapshot for a
spelling names the next requests by it. The pins and cooldowns already keyed on the previous dated id stay where they
are, since a snapshot is a model of its own, and the stored mapping is updated in the same transaction.

## Pools

A pool holds one or more accounts, each with a priority:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/pools \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"id":"main","label":"Main","members":[{"account_id":"vendor","priority":0}]}'
```

An account belongs to one pool. Every field, in the order the pool's JSON body lists them, with the answer a value
outside its bound gets:

| Field | Default | Bound |
| --- | --- | --- |
| `id` | required | a slug: `id must match [a-z0-9][a-z0-9-]{0,63}`, and with `label`: `id and label are required` |
| `label` | required | none |
| `enabled` | `true` | `true` or `false` |
| `strategy` | `pace` | `pace`, `priority` or `round_robin`: `strategy must be pace, priority, or round_robin`. See below |
| `prefer_subscription` | `true` | `true` or `false`. See below |
| `affinity_ttl_seconds` | `3600` | `0` or at least 60: `affinity_ttl_seconds is 0 or at least 60`. See Selection |
| `overage_policy` | `prefer_plan` | `prefer_plan`, `allow` or `never`: `overage_policy must be prefer_plan, allow, or never`. See Overage |
| `affinity_mode` | `always` | `always` or `when_cached`: `affinity_mode must be always or when_cached`. See below |
| `subagent_placement` | `by_strategy` | `by_cohort`, `by_strategy` or `with_main`: `subagent_placement must be by_cohort, by_strategy, or with_main`. See Selection |
| `session_placement` | `by_strategy` | `by_strategy` or `by_warm_prefix`: `session_placement must be by_strategy or by_warm_prefix`. See Selection |
| `min_shared_prefix_tokens` | `20000` | a whole number, 0 or more. See Selection |
| `shed_ahead_of_pace` | unset | 0 to 1: `shed_ahead_of_pace is between 0 and 1`. The console proposes 20 %. See Moves |
| `select_below_utilization` | unset | 0 to 1: `select_below_utilization is between 0 and 1`. The console proposes 80 %. See Selection |
| `model_rules` | empty | an ordered list of `{pattern, not}`, at most 256: `model_rules holds at most 256 entries`. See Model rules |
| `shed_share` | `0.5` | 0 to 1: `shed_share is between 0 and 1`. See Moves |
| `burn_window_seconds` | `600` | 60 to 604800: `burn_window_seconds is at least 60`, `burn_window_seconds is at most 604800 (one week)`. See Moves |
| `source_dwell_seconds` | `900` | 60 to 604800: `source_dwell_seconds is at least 60`, `source_dwell_seconds is at most 604800 (one week)`. See Moves |
| `migrate_when_spent` | `true` | `true` or `false`. See Moves |
| `warm_sessions` | `when_needed` | `always`, `never` or `when_needed`: `warm_sessions must be always, never, or when_needed`. See Moves |
| `retry_in_place_seconds` | `60` | 0 to 300: `retry_in_place_seconds is 0 to 300`. See Retry in place |
| `in_flight_cap` | `16` | at least 1: `in_flight_cap is at least 1`. See In-flight caps |
| `pace_floor` | `0.05` | 0 to 1: `pace_floor is between 0 and 1`. See Selection |
| `overage_move_grace_minutes` | `30` | 0 to 300: `overage_move_grace_minutes is 0 to 300`. See Moves |
| `wind_down_minutes` | `30` | 0 to 10080, a week: `wind_down_minutes is at most 10080 (one week)`. See Subscription end |
| `keepalive` | `enabled` off when the pool is created over the API without the block, on in the console's create form | a policy object of four fields, on [The four fields](keepalive.md#the-four-fields) |
| `cache_ttl` | off | a policy object, `inject_when_absent` off by default. See Cache TTL |
| `cache_diagnostics` | `false` | `true` or `false`: `cache_diagnostics must be true or false` |
| `auto_mode_server` | `true` | `true` or `false`: `auto_mode_server must be true or false`. See below |
| `body_capture_hours` | `0` (off) | at most `stats.body_capture_max_hours`: `body_capture_hours exceeds stats.body_capture_max_hours`. The console proposes 24 |
| `members` | none | a list of `{account_id, priority, budget_daily_usd, budget_monthly_usd}`, written as the whole membership when present, as `PUT /admin/pools/{id}/members` does |

An unknown field in the body, or a value past its field's type, is answered `400 body must be a JSON object`. A
member's `priority` is a number, `0` by default, and lower sorts first. Naming an unknown account in `members` answers
`404 account {id} not found`, naming one twice answers `409 members name account {id} twice`, and in either case the
pool is not created. A pool with keys cannot be deleted: `409 pool still has keys`.

Every member carries a daily and a monthly spend budget, `budget_daily_usd` and `budget_monthly_usd`, each unset by
default (no cap), above zero when set (`budget_daily_usd is above 0`, `budget_monthly_usd is above 0`) and measured
over the UTC calendar day or month. An API-key member's whole spend counts. A subscription member's metered spend alone
does, its overage traffic and its fast answers (a fast answer draws usage credits whatever the overage state), priced at
API list rates from the rollups filed under `overage` and `fast`, so a budget on one bounds its extra spend in dollars
from the router's side while its plan-funded traffic is never counted. A member whose spend reaches either budget is
skipped for requests until that period ends, the month's end whenever the monthly budget is reached, and a keepalive
probe pinned to it waits rather than moving the pin. A pool whose every member is at its budget answers `429` with the
`overage` claim, the same one an account in overage carries. The spend sums the member's hourly rollups, and the rows of
an active budget period outlive `stats.rollup_retention_days` until that period ends. While the ledger is degraded (a
batch of records failed to land and is being retried), a member with a budget reads `budget_unknown`: it is not placed
on, a session pinned to it moves to a sibling without a budget, and a pool whose members all carry budgets answers
`accounts_unavailable` until the writer catches up and reads the spend afresh. No operator action is needed, and the
state clears on the next landed batch.

`prefer_subscription` orders the set by kind (subscription accounts with plan quota, then those in overage, then API
keys) before priority. It is the operator's answer alone, on by default, and no strategy overrides it. The pool
dialog's "Prioritize subscription accounts" toggle sets it.

Each strategy's rule, as the pool dialog's Strategy hint states it:

**`pace`**: Within the first priority tier that has an eligible account, the one with the most allowance left in the
window it reads, the model's own Fable week where the model has one, else the 7-day window, per hour until that window
resets or the subscription ends. An unsampled account counts as untouched, and ties are random.

**`priority`**: Within the first priority tier that has an eligible account, any of them at random.

**`round_robin`**: Within the first priority tier that has an eligible account, each in turn. A restart starts the
round again.

The console's pool dialog offers, in its expert view, `shed_share`, `burn_window_seconds` and `source_dwell_seconds`
together as three presets beside custom: balanced (0.5, 600 s, 900 s, the default), fast (0.5, 300 s, 600 s) and
conservative (0.3, 900 s, 1800 s). It offers `in_flight_cap`, `retry_in_place_seconds` and `pace_floor` the same way:
balanced (16, 60 s, 5 %, the default), fast (8, 20 s, 10 %) and conservative (16, 120 s, 5 %), where balanced and
conservative differ in the retry-in-place bound alone. The Pools view, its cards and the dialog are described on
[Pools](console.md#pools).

Under `when_cached` a request that carries no `cache_control` marker, and receives none from the cache TTL policy,
creates no pin and lands by the strategy instead of pinning. A non-portable request keeps its pin either way.

The pool's `cache_diagnostics`, the pool dialog's Cache diagnostics switch in the expert view, turns the cache
diagnostics on for this pool's requests toward `api.anthropic.com`: the router adds the cache-diagnosis beta
([The betas](relay.md#the-betas)) and a `diagnostics.previous_message_id` member to a body that carries a cache marker
of the client's and a tools list, never to a token count, and never rewrites a member the client sent. The clients that
send no member (an older Claude Code, the reduced shape) are the case the insertion exists for. The member names the
answer the request continues: the newest answer at the deepest message boundary the request shares with an earlier
answer of its own conversation, or of the conversation it forked from, on the serving account, else the newest answer
of the session on that account and model. A fork names its parent's answer only until it has an answer of its own on
that account and model. Off by default.

`auto_mode_server` is whether the client's server-side auto mode checks travel to the upstream. On, the default, the
router passes the request for a check and its verdict through unchanged. Off, it drops the request so the client
reviews each tool use with a classifier request of its own. [Auto mode behind the
router](relay.md#auto-mode-behind-the-router) says what the router cuts and what the client does in each case, and a
key can override the pool ([Keys](#keys)).

### Cache TTL

A pool can set the prompt cache TTL of every request it serves toward `api.anthropic.com`. The policy object carries
`enabled` (default `false`), `inject_when_absent` (default `false`) and two rule sets, `plan` and `metered`: `plan`
prices a subscription account on plan quota, `metered` an API-key account or a subscription account in overage. Each
set carries `main` and `subagent` in `5m` or `1h` and `models`, a list of `{model, main, subagent}` overrides on
canonical ids. `plan` defaults to one hour throughout and `metered` to five minutes throughout: a one-hour write costs
a metered account 2x base input against 1.25x for five minutes, and a plan-funded account the same window share either
way. With the policy on, every `cache_control` marker in the body is rewritten to the TTL the request's set and branch
choose, the ones inside a tool result's own content included, the `extended-cache-ttl-2025-04-11` beta travels exactly
when the body is one hour ([The betas](relay.md#the-betas)), and the `cch` hash is restamped when the client sent one.
A restored placeholder `cch=00000` stays unsigned unless `proxy.sign_cch_placeholder` is on. A body whose marker the
relay's cache TTL guard cannot reach (a key on the marker's path spelled with an escape) is refused `400` with the error
code `body_shape`, so a TTL the policy could not enforce never travels unreported. On every other body the client's own
`CLAUDE_CODE_PROMPT_CACHE_TTL` has no effect through the pool. A body that carries no marker is left as it came unless
`inject_when_absent` is on and the request's class is not auxiliary (the class header absent, or naming `compaction`,
`main` or `subagent`), in which case the top-level `cache_control` member is added with the TTL, before `stream`. A
token count is never edited by the policy: its markers travel as the client sent them and no beta follows it. The
metered set applies while the write costs money: the serving account is in overage, or is an API-key account. Every
request, a keepalive probe's replay included, reads `main` or `subagent` by its agent, and one without an agent id
reads as the main conversation. The TTL is priced per attempt, so a request that fails over reads its destination's
state. Mixed TTLs across a session's requests are fine: the cache is keyed on the prefix, and the TTL sets an entry's
lifetime and price alone. The price of each choice is on [Pricing](statistics.md#pricing).

## Keys

A downstream client presents a key, and a key routes into exactly one pool:

```sh
curl -sS -b jar.txt -X POST https://phoebis.example/admin/keys \
  -H 'content-type: application/json' -H 'origin: https://phoebis.example' \
  -d '{"id":"desktop","label":"Desktop","pool_id":"main"}'
```

The secret is `phoebis-` followed by 43 characters of base64url encoding 32 random bytes. It rides in the answer to
creation and to rotation and nowhere else. The console shows it once, in a dialog with a copy button.

Every field, in the order the key's JSON body lists them, with the answer a value outside its bound gets:

| Field | Default | Bound |
| --- | --- | --- |
| `id` | required | a slug: `id must match [a-z0-9][a-z0-9-]{0,63}`, and with `label` and `pool_id`: `id, label, and pool_id are required` |
| `label` | required | at most 256 bytes: `label is at most 256 bytes` |
| `notes` | unset | at most 256 bytes: `notes is at most 256 bytes`. `null` clears it |
| `pool_id` | required | an existing pool: `404 pool not found`. A change moves the key to that pool |
| `enabled` | `true` | `true` or `false`. A revoked key cannot be re-enabled: `409 a revoked key cannot be re-enabled` |
| `expires_at` | unset | Unix milliseconds. `null` clears it |
| `model_rules` | empty | an ordered list of `{pattern, not}`, at most 256: `model_rules holds at most 256 entries`. See Model rules |
| `model_rules_mode` | `extend` | `extend` or `replace`: `model_rules_mode is extend or replace`. See Model rules |
| `long_context_twins` | `true` | `true` or `false`, whether the [models listing](client.md#the-models-listing) carries the `[1m]` twins |
| `keepalive_opt_out` | `false` | `true` or `false` |
| `body_capture_hours` | unset, inheriting the pool's | `0` turns capture off, else at most `stats.body_capture_max_hours`: `body_capture_hours exceeds stats.body_capture_max_hours`. `null` inherits again |
| `auto_mode_server` | unset, inheriting the pool's | `true`, `false` or `null`: `auto_mode_server must be true, false, or null`. `null` inherits again |

Rotate issues a new secret under the same id, and a revoked key cannot be rotated (`409 a revoked key cannot be
rotated`). Revoke is permanent, so a revoked key cannot be re-enabled. Delete removes the key outright and detaches its
request history from the key's name.

A key's `body_capture_hours` keeps the bodies of every request under the key in the ledger for that many hours, the
pool's value when the key inherits it, at most `stats.body_capture_max_hours` ([stats](installation.md#stats)). The
console warns what the bodies hold where the operator switches capture on for a key, in the key dialog's Capture group
([Keys](console.md#keys)): "Bodies contain the prompts, file contents, and tool results of every session using this
key, and a session in auto mode sends the machine's permission rules with their literal text, its working and home
directories and its git remotes with every request. Only the admin secret can read them." The capture itself and its
reader are on [Body capture](statistics.md#body-capture).

The Keys view, the key dialog and the secret dialog are described on [Keys](console.md#keys).

## Clients

Every request carries a class, read off its user agent: `claude-code/<entrypoint>` for a `claude-cli/` caller (the
entrypoint token it names, `cli` when it names none)
and `other` for anything else.

A Claude Code session in auto mode asks the upstream to review each tool use on the model's own turn: a `safeguards`
member near the end of its body and the `dangerous-tool-use-2026-09-03` beta in its header, answered by a verdict per
tool use. With the pool's `auto_mode_server` on (the default), or a key's override on, both travel as sent and the
verdict comes back unchanged. Off, the router cuts the member and removes the beta from every request of the key, a
token count and a keepalive probe included, and restamps a first-party body's billing hash. The answer is copied as it
comes, never edited. The client then reviews each tool use with a classifier request of its own for the rest of its
process, billed to the account's windows, as [Auto mode behind the router](relay.md#auto-mode-behind-the-router)
describes. A body whose `safeguards` member the router cannot cut under an off setting (spelled with an escape, or
carried twice) is refused `400` with the error code `body_shape` before anything reaches the upstream.

A conversation is identified by its session header: the session id the client sends names it, keys its pin and its
session memory, and is what a turn that follows its own conversation's previous answer within milliseconds waits on, at
most five milliseconds, for that answer's report to reach the map. A new session id whose messages continue another
conversation of the same key keeps its own identity, and is recorded as a fork of that conversation at the depth where
the two part, when that depth is 2 or deeper: a shared first message is a template, not a conversation. An auxiliary
request (the client's quota probe among them) never names a fork and leaves no content entry, so no later request
matches it.

## Model rules

A pool and a key each hold one ordered list of rules, and a request's model is judged against the key's processed
list: the key's own entries first, then the pool's when the key's `model_rules_mode` is `extend` (the default), or the
key's own entries alone under `replace`. Evaluation runs top to bottom and the first matching entry decides: a plain
entry allows the model, a `not` entry denies it, and a model no entry matches is allowed. Both lists start empty, so
nothing is excluded until an operator writes an entry. The list is first-match-wins and nothing else, so an allowlist
is a list that names what it allows and ends in a catch-all denial:

- `[claude-opus-5, NOT *]` allows the one model and blocks the rest.
- `[NOT *]` alone blocks everything.

A rule's subject is the canonical id: the request's model with the `[1m]` suffix removed, ASCII case folded, and the
dated id the catalog lists substituted. One entry therefore covers a model and its `[1m]` twin, and an entry for a
model the API lists under a dated id is written as a glob: `NOT claude-sonnet-4-5*` covers `claude-sonnet-4-5`,
`claude-sonnet-4-5-20250929` and the twin of either. An entry that spells an undated id exactly (`claude-sonnet-4-5`)
never matches, because the router names that model by its dated snapshot, so an exact entry for an undated model is
written as a glob (`claude-sonnet-4-5*`) too, and the rule editors and the Models view say so beside the list. A model
the list denies answers `404 model_not_allowed` without reaching an upstream.

The grammar is path-style: `*` matches any run of characters other than `/`, `**` any run including `/`, `?` one
character other than `/`, a backslash quotes the character after it, and a pattern with no wildcard matches its own
text alone. Anthropic's ids carry no slash, which is why `NOT *` reads as the catch-all above, and `NOT **` is the
entry that also covers a name with a slash. The request log's client patterns on the Requests and Analytics views
([Requests](console.md#requests)) are another grammar, where `*` crosses everything. A pattern is refused at save time
when it is empty, longer than 256 characters, ends in an unquoted backslash or holds three stars in a row, and the
refusal names the entry and the reason: `model_rules[0]: pattern is empty`, `pattern is longer than 256 characters`,
`pattern ends in an unquoted backslash`, `pattern holds three stars in a row`.

The processed list of a chosen key, the accounts cooling for each model and the link to the routing log's model
rejected and model permitted rows are on the Models view ([Models](console.md#models)).

## Selection

Selection keys every request by `(key id, root session, agent, canonical model)`. The router holds all routing state
and answers every request over a channel, so two concurrent first requests of the same session cannot claim different
accounts. The identity a pin is held under is the session header [Clients](#clients) names.

A fresh request with no live pin lands where its prefix is already warm. The deepest message boundary any account still
holds wins first, for a main conversation and a subagent alike. Both rungs read entries of the request's own cache lane
alone: the model, the speed, the thinking configuration and, on a model without the per-turn effort form, the effort.
The upstream shares no cached prefix across those, so an entry of another lane is no warm prefix. Failing that, the
deepest static boundary places a subagent under `by_cohort` or a main conversation under `by_warm_prefix`, when the
request carries or receives a cache marker, the entry's estimate (the relay's, from the request's bytes) is above
zero, the shared prefix is at least `min_shared_prefix_tokens` for a main conversation, and the account is not at its
cap, at or above the ceiling (`select_below_utilization`), at or above the shedding margin less its 0.05 band on its
windows ahead of their pace (`shed_ahead_of_pace`, a subscription account), without room in its worst live rate-limit
bucket (an API-key account while a margin is set: a reading at or above 1.0, whatever its value), under the Pace floor,
or winding down. The threshold binds a main conversation alone: a subagent cohort lands together on any shared prefix
above zero, and an entry with a zero estimate places nothing for either. A cooling account never reaches either rung,
since the eligible set excludes it. A spent account is in the eligible set, and both rungs refuse it through the
allowance check. A subagent under `with_main` lands its first request on its parent agent's pin in place of the static
walk, and one under `by_strategy` skips the static walk. Every case that finds no live match lands by the strategy. A
held request of a subagent cohort waits where its cohort already is rather than on the strategy's own pick, and a
session already on an account, under any of its agents, is never held while that account can still serve. A token
count is never placed and never held: it is bound to no cap and occupies no slot, so its pin or the strategy decides
it. It creates no pin, since it touches no cache: a count with a pin follows it past the cap and the move rules, and
one without lands by the strategy and leaves the agent's next message to be placed under the cap. An entry's warmth
lasts the marker's TTL on its tier. When every marker of the request carried one TTL and the response reports its cache
writes at the other alone, the reported TTL is the lifetime of the message boundaries that response wrote, and of the
static boundaries only when the response read nothing from the cache, since the usage names no tier. A static tier the
response read from and a tier it only read keep their marker's lifetime, and a request with markers at both TTLs keeps
each marker's.

The eligible set starts from every account that can serve the canonical model: enabled, paired or keyed, `ready` or
`funding_required` past its retry instant, off cooldown, and not already tried by this request. A request sent fast is
judged for the fast lane instead: it passes an account cooldown whose window is a plan window, and an account whose
fast lane is cooling or whose usage credits the poll reads as unavailable cannot serve it. An API-key account serves it
whenever it can serve at all.

A pin is held per agent of a conversation, where a conversation is the session id or the conversation's own messages.
It lasts `affinity_ttl_seconds` from its last use: while it is live and its account is still eligible, the request
returns to that account, and stickiness wins over priority even across a later priority change. A pool with affinity
off (`affinity_ttl_seconds` at `0`) keeps no pins at all. A keepalive probe holds while `affinity_ttl_seconds` is
shorter than its recipe's due time, the cache TTL less the lead ([The five states](keepalive.md#the-five-states)).

Narrowing then trims what is left. For a model with a week of its own (a canonical id containing `fable`), an account
whose week or either shared window reads at or above 1 drops outright, unless the pool's overage policy is `allow` and
the poll has not read the account's extra usage as blocked, in which case it stays and orders behind every plan-funded
account. An account already at the pool's `in_flight_cap` takes no fresh placement, ahead of every preference below.
With every account at its cap the request is held for a slot instead (see **In-flight caps**, below). The rest are
preferences that fall back to the wider set rather than stranding the request: `select_below_utilization` drops every
account at or above that threshold in 5-hour utilization, and an account winding down toward its subscription end (see
**Subscription end**, below) is skipped for a fresh placement too.

What remains is ordered in tiers. With `prefer_subscription` on, the set splits into three kind groups walked in order,
plan quota, overage, then API key. Within a group, or across the whole set with the flag off, tiers order by priority
ascending, then, under `prefer_plan`, an account not in overage before one in overage. An account kept under `allow`
with a spent window joins the overage group with the flag on, and with it off sorts behind every account that is not,
whatever the priorities. The strategy picks inside the first non-empty tier. `pace` reads each account's sample as a
window map: a subscription account scores by remaining allowance in its own window (the model's own Fable week where
the model has one, else the 7-day window) over the hours to the earlier of that window's reset and the account's
derived end. An API-key account scores by one minus its worst live rate-limit bucket's utilization alone, with no
division by hours, since a bucket refills within a minute and its own reset never enters the score. Either way a window
nobody has read reads as untouched: remaining 1 over the full week. A subscription window read past its reset reads as
the router's own use since it, while an API-key bucket past its reset is refunded and reads as untouched, with no share
of the router's own. Ties are random. An account whose remaining allowance in that window (the model's own Fable week
where the model has one, else the 7-day window) is at or below the pool's `pace_floor` is kept as the last resort,
ranked by the same score among the others below it, once no account above the floor remains. `priority` and
`round_robin` read no sample and no floor at all.


A non-portable request, one whose body references a `file_id`, a `container` or a server-tool result, is answered by
its live pin's account or not at all. With affinity off or no live pin, it runs the normal selection instead, and the
account-scoped reference it carries may then not resolve.

Every decision is recorded in full, with what the engine read in the order it read it, and read in the console's
[routing log](console.md#routing-log). The log's words for the rules, the rungs, the partitions and the strategies are
the ones this page uses.

### The windows the router works from

The router judges a subscription account by the 5-hour and the 7-day window every model shares, and a model family's
own week where the family table names one (its one row is Fable's `seven_day_fable`). A weekly reading the endpoint
reports under a key the table does not name (such as `seven_day_opus`) is kept, shown and persisted, cools the account
when a 429 claims it, and feeds no allowance until a row names it. The windows are read from two sources: every
response head the account answers, and a poll of the usage endpoint every five minutes per account, each account on
its own phase, backed off alone on a 429 ([Quota samples](statistics.md#quota-samples)). Between readings the router
predicts: a reading stands until its window's reset, plus the share of the window the router's own requests consumed
since the reading (priced through the subscription cost model on the account's tier). A window the cost model cannot
price for the request adds nothing and holds its reading. At the reset it reads zero and counts the requests since, a
weekly window's reset advanced by whole weeks, the 5-hour window's next reset named by the next reading. A reading that
names no reset stands until the next reading, and an API-key account's rate-limit buckets stand until their reset or,
without one, for a minute. The share is priced through the account's stored tier: a tier change is read at the next
profile refresh, within the hour, and applied when the router reloads, and a tier changed while the router was down is
read at start. The newest reading per account and window is kept in the registry and loaded at startup, so a restart,
and an account whose first poll the endpoint refuses, route from the last known windows. The router never fetches usage
at a selection: use of an account outside the router shows in the next head or poll, so a decision can lag it by up to
five minutes plus any backoff, and by up to one more interval after a failed poll or a restart. The
[Quota view](console.md#quota) shows when the poll last asked the endpoint for each OAuth account and how it answered,
and while an account backs off, since when and when its next attempt comes. The `poll_backoff` condition of the
[`phoebis_attention` gauge](statistics.md#conditions-and-gauges) holds once an account has backed off for thirty
minutes.

### Moves

A live pin on a portable request moves only when its session sends a request, and only by one of five rules, checked
in this order: spent, overage, return to plan, drain and shed. The first rule that finds a destination moves the pin
and names the move after itself. A pin on an account past its subscription end reaches none of them: the
`subscription_ended` exclusion releases it, as a cooldown releases a cooled account's pin. A request sent fast takes
neither the spent nor the overage rule, since it draws usage credits and no plan window.

With `shed_ahead_of_pace` set, every rule prefers a cool destination, so a moved session does not land where the next
round would shed it again. A cool sibling reads under the margin minus 0.05 (`MIGRATE_BAND`) on how far the 5-hour and
7-day windows of a subscription account run ahead of their pace, or holds room (under 1.0) in the worst live rate-limit
bucket of an API-key account, whatever the margin, since a bucket refills within its minute. An unsampled account reads
as under. The account has run no shedding round within `source_dwell_seconds`, and it has room under the pool's
`in_flight_cap`. When no destination of a rule is cool, the rule still moves the pin to one of them, so a full cool
sibling never keeps a free hot one from the session. Only the shed rule insists on a cool one.

**Spent** The pinned account's allowance for the session's model is spent: the model's own week and both shared windows
for a model with a week of its own, the shared plan windows otherwise (any of those readings at or above 1.0), while
that reading stands (until its reset, or until the next reading for one that names none). With `migrate_when_spent` on
(the default), the pin moves on this request to a sibling that holds allowance in those windows, without waiting for
the upstream's 429. The shedding budget does not apply, and the band and the dwell only order the siblings: one ahead
of its pace by the band's line that still holds allowance still receives the session when no cool one can. The router
has to know the sibling's allowance: an API-key account has no plan window to spend, and any other sibling needs a
reading of each of those windows, from a head, the poll or the windows kept across restarts, carried forward as the
windows passage says. A sibling with none stays out of the choice. Under `prefer_plan` a sibling in overage does not
receive a spent pin, since the overage move would hand the session straight back. With the toggle off, or no sibling
holding allowance, the session stays.

**Overage** Under `prefer_plan`, a pin on a subscription account in overage returns to plan quota: warm or cold, it
moves to a subscription sibling that is not in overage, never to an API-key account. The sibling has to hold allowance
too, since a spent one would move the session again on its next request. It waits while the reset the overage evidence
names falls within `overage_move_grace_minutes` (default 30, `0` for no wait), so a session whose account gets its plan
back soon keeps its cache and moves at most once. Evidence that names no reset moves the pin at once.

**Return to plan** A pin on an API-key account moves to a subscription sibling that holds allowance and is not in
overage once its cache is cold, or once it has sat on the account for `source_dwell_seconds` since it landed there. An
API key names no reset, so no grace applies.

**Drain** A cold pin on an account winding down toward its subscription end moves to an eligible sibling that is not
winding down. A warm one stays (see **Subscription end**, below).

**Shed** With `shed_ahead_of_pace` set, a request pinned to an account whose 5-hour or 7-day window runs ahead of its
pace by at least the margin (the prediction less the share of the window already elapsed toward the window's reset, or
toward the account's subscription end when that comes first, where a 5-hour window past its reset reads the router's
own use since) starts a shedding round there, at most once per `source_dwell_seconds`. The round orders the pool's pins
on the account, cold ones first by burn, then warm ones by burn over rewrite cost (the pin's last prompt size in
tokens), and marks them in that order until the marked burn reaches `shed_share` (default 0.5) of the burn on the
account over `burn_window_seconds` (default 600). It never marks the pool's last unmarked pin there, so a pool with one
session on the account sheds nothing, and no round takes all of an account's burn. A pin a former pool left on the
account is outside the round until it ends. A pin whose burn carries the total past the share stays marked, and the
round stops there. A pin whose burn the cost model cannot price ranks after every priced pin of its group, counts
nothing toward the share and is never marked, so a session is never shed on a guess. `warm_sessions` bounds the order:
`never` stops after the cold pins, `when_needed` (the default) goes on to the warm ones, and `always` orders every pin
by burn over rewrite cost, cold or warm. While the account runs on its plan, a pin whose last refreshing request was
sent in fast mode is out of the round: fast mode draws usage credits and no plan window, so the pin has no share to
shed. In dollars, on an API-key account or one in overage, it stays in the round. A session that returns to standard
requests brings its pin back with the plan burn it still carries.

A marked session moves at its next request, the request that ran the round included, since the mark check follows the
round. It moves onto a cool sibling (see above) that did not shed this pin within `source_dwell_seconds`. A mark from a
round on plan quota moves a standard request alone: a fast-mode request of the marked session stays where it is. A pin
on an account cooling for the session's model, marked or not, is released rather than moved, with `cooldown` among its
reasons: it is not renewed at step 5, the fresh placement takes its place, and a mark goes with it. The round on the
account still runs, since a cooldown can hold one model alone. An unmarked pin never moves for utilization. The marks
last for the dwell: a marked session that stays idle never moves, and no new round runs on the account until its dwell
passes. The round's marks and its dwell are written to the registry and survive a restart, so a restart neither
forgets a mark nor starts a fresh round inside the dwell. A held request does not survive one (the relay's open
connection closes and the client retries), and the routing log's router started row says so.

**Warmth** A pin stays warm while its last refresh is younger than the cache TTL in force for it. A `main`, `subagent`
or `compaction` request ([`x-claude-code-request-class`](client.md#claude_code_gateway_hint_headers)) refreshes it, and
any other class leaves it alone. A request that would refresh the pin reads it cold instead when it is on another cache
lane than the pin's last (a speed change, a thinking change, or an effort change on a model the client sends no
per-turn control entry for), whatever its messages continue, and the pin takes the new lane. A request that leaves the
pin alone leaves its lane as it was, and a per-turn control entry is one more message and continues the conversation.
Without the class header, a request whose messages extend the session's conversation refreshes the pin, one that
repeats the session's static prefix (tools and system prompt) without extending its messages restarted the
conversation and reads cold, and one that matches neither changes nothing. Where one session holds several
conversations, a request that extends any conversation the pin has served refreshes it. A request that carries
`x-claude-code-context-compacted` reads cold whatever else it says, so a compacted session goes first in the round's
order on the request that carries the fact. A body the router cannot read refreshes the pin on every request on the
pin's lane, and the first one after a request on another lane reads it cold, as any lane change does. A keepalive probe
replays another request's headers and body, so the router judges none of them: a probe never colds a session, and its
renewal counts as a refresh while the pin still holds the lane the probe was sent on. A renewal that lands after the
session moved to another lane refreshed another cache and leaves the pin as it was. A `count_tokens` request creates no
cache entry, so it neither refreshes nor colds the pin and leaves its cache TTL as it was.

**Burn** Each pin keeps its usage over `burn_window_seconds` as typed tokens per hour (input, cache read, 5-minute and
1-hour cache writes, output) for the pin's one model, in three copies: one plain and one weighed by the peak band at
each renewal for its standard answers, and a third for its fast answers, priced at the fast cells where the model has
them. The registry keeps all three on the pin's row, so a restart reads each as saved. Nothing is priced until it is
read: the round prices the banded copy as a share of the account's 5-hour window while a subscription account runs on
its plan, and the plain copy in dollars, at the set the pin's last prompt selects on a model priced by prompt length
([Pricing](statistics.md#pricing)), with the fast copy added in, on an API-key account or a subscription account in
overage, so leaving overage switches the reading without a reset and a session that moves between tiers reads the right
share on both. A keepalive probe's usage never counts. A burn the model cannot price, for a model it has no weight for
or a tier it has no capacity for, reads as none.

Any move records a `pin_migrated` event with reason `cooldown`, `drain`, `fast_lane`, `mark`, `overage`,
`return_to_plan`, `shed` or `spent`: `cooldown` and `mark` when a failover or a credential mark moved the session,
`fast_lane` when a fast-mode rejection moved the session, the others after the rule above that moved it. A pin released
without a move records `pin_expired` with `reasons`, every fact the selection found against the account: `attempted`,
`budget` (the pinned member has reached one of its spend budgets), `budget_unknown` (the ledger is degraded and the
pinned member carries a budget), `cooldown`, `deleted`, `expired`, `fast_credits` (a fast request's pinned account whose
`fast_credits` row stands or whose usage credits the poll reads as unavailable) and `fast_rate_limit` (one whose `fast`
row is the blocking row that stands latest), `overage`, `paused`, `spent`, `subscription_ended`, `unserved`, the
account's state when it cannot serve (`disconnected`, `funding_required` until its retry is admitted, `reauth_required`,
`unpaired`), or `released` when the selection found nothing else against the account, as with a move whose destination
emptied. A pin whose creating request was answered 400, 403 or 404, or was bound to its account and ended at an
authentication failure that marked nothing, with no cooldown or mark on the account and no other turn of the session
renewing it meanwhile, records the single reason `client_error`: it never served, so the session's next request is
placed afresh rather than moved. Releasing a session by hand, from the Sessions view ([Sessions](console.md#sessions))
or `POST /admin/sessions/{key}/{session}/release`, expires its pin the same way.

## Cooldowns and failover

The relay classifies every upstream response head before a byte reaches the client, partitioned by account kind first,
`oauth`, `api_key`, or `shared` for both. Within a partition the first matching row wins. Where a row names the
fallback, it is `routing.cooldown_fallback_seconds`, default `60`, 1 to 3600, a key of the bootstrap file's
[routing](installation.md#routing) table. Every deadline a row sets carries a second of margin, so a request sent at
the reset is not refused by a clock a hair behind.

**oauth**

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 429, a shared window (5h or 7d) or `status` rejected, the 5-hour status absent beside a rejected 7-day window included | Account-wide until the later of the rejected window's reset and the top-level reset, else `Retry-After`, else the fallback | Yes |
| 429, the claim naming the model's own week, the `7d_oi` or the top-level `status` rejected, 5h and 7d healthy | Model-scoped until the later of the `7d_oi` reset and the top-level reset, else `Retry-After`, else the fallback | Yes |
| 429, the `7d_oi` status rejected, no other family's week claimed, 5h and 7d healthy | Model-scoped, the same deadline | Yes |
| 429 whose unified headers reject nothing | Account-wide until `Retry-After`, else the fallback | Yes |
| 429 without unified headers | Account-wide until `Retry-After`, else the fallback | Yes |
| 200 SSE, first event `error`/`rate_limit_error` | As the 429 rows, using the response's headers | Yes |

A shared window is healthy when its status reads `allowed` or `allowed_warning`, or when the status is absent and the
window's own utilization reads below 1.0. An absent status with no utilization, or any other status value, is not
evidence that the window is fine, and the rejection cools the whole account.

The two model-scoped rows apply to a model whose family has a week of its own in the family table (Fable, whose week is
`seven_day_fable`). The row cools that model on the account and recovers once a reading of that week is under 1.0. The
claim is the `representative-claim` header: one naming the model's own week, or `seven_day_overage_included`, or none
at all leaves the decision to these rows. A claim naming another family's week, or a week no row names (such as
`seven_day_opus`), cools the whole account whatever the `7d_oi` status reads, and so does any of these rejections for a
model without a week of its own: under the shared row when the top-level `status` reads rejected, and under the row for
headers that reject nothing otherwise.

**API key**

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 429 naming `enforced_spend_limit_reached` | Account-wide until the first of next month, reason `spend_cap` | Yes |
| 429 with a valid `Retry-After` | Model-scoped until `Retry-After` | Yes |
| 429 without a valid `Retry-After` | Account-wide for the fallback duration | Yes |
| 400 `invalid_request_error`, credit balance | Marked `funding_required` until a retry succeeds or the operator clears it | Yes |
| 200 SSE, first event `error`/`rate_limit_error` | As the 429 rows, using the response's `Retry-After` | Yes |

A `funding_required` account is tried again on its own clock, since an API-key balance has no signal the router can
read. The retry is due five minutes after the mark lands, or after `routing.model_rejection_seconds` when that is
shorter, and the account then takes one request at a time: the attempt the selection placed on it holds the slot until
that attempt ends, however long its head takes. A 2xx answer to a message request clears the mark, and its
`account_state_changed` row reads the cause `funding_retry`. A held stream that committed on an error event clears no
mark, whatever its head's status. Another credit-balance refusal marks the account again with a new retry instant, and a
success from an attempt sent before that mark leaves it standing. A token count may reach the account under the same
rule, but it holds no slot and clears nothing. The router keeps the retry instant in memory, so after a restart the
first request tries a marked account at once. Clear funding, on the Accounts view or through the admin API, makes the
account eligible at once ([Account states](pairing.md#account-states)).

**fast mode** (either kind, read ahead of the two tables above)

| 429 or SSE `rate_limit_error` to a fast request | Cooldown | Failover |
| --- | --- | --- |
| Overage-disabled reason on the head | Scope `fast_credits`, reason `fast_credits` | Yes |
| No such reason, 5h and 7d not rejected, no API-key spend cap | Scope `fast`, reason `fast_rate_limit` | Yes |
| No such reason, the 5h or 7d window rejected, or an API-key spend cap | The rows above | Yes |

The overage-disabled reason decides first, whatever the shared windows read, since a fast request draws usage credits
and no plan window. A rejected top-level `status` that names neither shared window leaves the rate limit on the fast
lane. The two refusals keep a row each, so each lifts on its own evidence. A `fast_credits` row holds until the month
starts for `org_spend_cap_reached`, else until the overage reset the head names, else for one hour, and the poll's next
enabled reading lifts it earlier, leaving a `fast` row beside it standing. A `fast` row holds until the later fast
window reset the head names, else until `Retry-After`, else for ten minutes. Both rows hold fast requests alone, and
standard requests on the account are unaffected. A fast refusal moves the session's pin unless the pin has changed
lanes after the refused request was selected: a session that has moved on to another lane is not asking for the fast
lane any more. The lane compared is the one the selection left on the pin, so a fast request that left a standard
pin's lane alone (an auxiliary class, or a request foreign to the conversation) moves that pin too.
`DELETE /admin/accounts/{id}/cooldowns/fast` and `DELETE /admin/accounts/{id}/cooldowns/fast_credits` clear them.

**shared** (either kind)

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 401, or 403 `authentication_error`, OAuth | Refresh, then `reauth_required` if conclusive | Yes, never forwarded |
| 401, or 403 `authentication_error`, API key | `reauth_required` at once | Yes, never forwarded |
| 403 `permission_error` | Model-scoped, one hour, reason `permission`. The credential stays valid | Yes |
| 404 `not_found_error` naming a model | Model-scoped, `routing.model_rejection_seconds`, reason `model_rejected` | Yes |
| 529 | None | No, forwarded, the client retries |
| Other 5xx | None | No, forwarded |
| Transport failure before fully written | None | Yes, once, same account |
| Transport failure after fully written, no head at all | None | No, 502 `upstream_unreachable`, the client retries |
| Transport failure after the head, before any byte reached the client | None | No, 502 `upstream_cut` |
| A coding or a payload the router cannot decode, before any byte reached the client | None | No, 502 `upstream_undecodable` |
| Idle bound after the head, before any byte reached the client | None | No, 502 `upstream_stalled` |
| 200 head, body ends before its end | None | No, the request reads `incomplete` |

The router classifies a rejection whose body runs past 64 KiB from its first 64 KiB, a coded body from the text those
64 KiB decode to: a 429 cools and fails over, a 401 or a 403 `authentication_error` takes the credential path, and a
body the verdict forwards reaches the client whole, the prefix first.

The router answers a failure after the head with its own `502` while nothing of the upstream's answer has reached the
client: `upstream_stalled` when a rejection body went silent past the idle bound, `upstream_undecodable` when the answer
names a content coding the router cannot read or carries a payload that does not decode, and `upstream_cut` for a
connection cut while a rejection body or a held stream was being read, or a body past its bound. The request reads
`failed` with `stream_stalled`, `stream_undecodable` or `stream_cut`. `upstream_unreachable` names a request that got no
head at all.

An `incomplete` request's error code says how its body ended: `stream_cut` for a connection cut mid-body or a body past
its bound, `stream_incomplete` when an event stream closed without its `message_stop`, `stream_stalled` when the
upstream went silent past the idle bound, and `stream_undecodable` when the body did not decode. A forwarded answer with
any other status stays `failed`, and its error code reads `stream_cut`, `stream_stalled` or `stream_undecodable` when
its body was cut, went silent or did not decode.

Every new upstream connection, on the inference and the control plane alike, logs one line at debug level naming the
host and two durations: `dial_ms` for the TCP connect and `handshake_ms` for the TLS handshake.

`Retry-After` parsing is case-insensitive and accepts integer seconds or an HTTP date, rounding fractional seconds up.
Anything non-finite, negative, malformed or absent is invalid, and an API-key row then takes the account-wide fallback
duration instead. An HTTP date is read against the upstream's own `Date` header when one is present, to reduce
clock-skew error. A valid long delay is never shortened, and every deadline is an absolute time with the one-second
margin above that survives a restart.

An API-key account whose monthly spend cap is reached answers a 429 naming `enforced_spend_limit_reached`. The account
is cooled as a whole until the first of the next month, reason `spend_cap`, and an exhaustion it causes claims
`overage`.

A response head also lands a selection sample on the account it answered for, whichever header family it carries: an
OAuth head's unified `5h` and `7d` windows, and its `7d_oi` utilization when the response reports one, or an API-key
head's `anthropic-ratelimit-{requests,input-tokens,output-tokens}-{limit,remaining,reset}` headers, one bucket per
canonical model. A later head for another model adds that model's buckets beside the ones already on file rather than
replacing them. A bucket reading, an API-key head's or the fast lane's, leaves the account's map ten minutes after it
was read, and an account holds at most 64 of them, the oldest leaving first, so a model nobody asks for any more leaves
no buckets behind. The plan windows are never cut. A head's readings are one of the two sources of the windows the
router works from (see above), and the poll is the other.

A 404 whose message names the model the body carried is the upstream's answer that it does not serve that model: the
account is skipped for that model for `routing.model_rejection_seconds` (default `3600`, an hour), the request goes to
the next member, and the ledger records `model_rejected` for the refusing account and `model_permitted` for the one
that served it. A 404 naming no model, and every 400 and 422, reach the client as they came.

Each client request visits every eligible account of the pool at most once. A same-account retry after a transport
failure before the request was written, the one refresh replay, the one strip retry and the one retry in place keep
their own bounds, as the paragraph after the steps says. Each attempt goes through these steps:

1. Ask the router. `timed` answers `429` with `Retry-After: ceil(deadline - now)` and the three unified rate-limit
   headers the client's automatic continue reads. `untimed` answers `503` with error code `accounts_unavailable` and
   no `Retry-After`, since only an operator can clear it, unless the request reached it through holds alone with
   nothing dialed: then every account is at its cap, and the answer is `429` with error code `accounts_at_cap`. Each
   carries an Anthropic-shaped error body. `held` waits for a slot and asks again ([in-flight caps](#in-flight-caps)).
2. Acquire the account's credential, apply its body edits, and send on its dispatch path.
3. Classify the head. A short throttle on a portable request is waited out on the same account
   ([retry in place](#retry-in-place)). Any other failover row on a portable request records the cooldown, invalidates
   the matching pin, and returns to step 1. On a non-portable request it records the cooldown and commits the failed
   response instead. An authentication failure is the exception: it is never committed (see the paragraph after these
   steps), and the failover leaves the pin of an account it marks for the next selection to end. Any other row commits
   and relays the response.
4. Once the committed response ends, renew the pin with the usage it carried: the served account is final for this
   request. A pin this request created and whose answer is `400`, `403` or `404` is released instead, since it never
   served, unless another turn of the session renewed it meanwhile, and so is one a non-portable request created
   before an authentication failure that marked nothing. A `429` keeps the cooldown path of step 3.

An upstream `401`, or a `403` whose body is an `authentication_error`, never reaches the client. On an OAuth account the
router refreshes the token and replays the request on a new token. A refresh the token endpoint refused without rotating
anything is tried a second time within the transient backoff, about five seconds unless the endpoint asked for longer.
After a token endpoint's `5xx` or a lost answer the request is held on the same account for up to a minute, then
refreshed a second time, so a session keeps its warm account through the endpoint's bad minute, which is waited out once
per request. A refusal whose `Retry-After` is over five seconds and within the minute counts as that minute, so a
request that already held its minute on another account fails over at once. A `Retry-After` past that minute, a second
failure, or an API-key account's failure fails the request over to the next eligible account, and a request bound to
its account, or one with no account left, is answered the router's own `503 accounts_unavailable` with no account
named. An account the failover marks, or finds disconnected, keeps its pin until the reselect expires it and places a
fresh one. Any other failover moves the pin. The failover, its waits included, starts no new leg past the request's own
deadline, ten minutes less a margin: past it the request is answered `503 accounts_unavailable` rather than tried on
another account. The attempt rows read `auth_failed` with the upstream status, and the account reads `reauth_required`
after a conclusive failure of the refresh (an `invalid_grant`, a token endpoint's `401`) or after a rejected replay,
which the Accounts view and the `reauth_required` attention condition show. Whoever lands the mark reports it at once:
the relay for a failover or a rejected replay, and the usage poll, its profile read and the catalog refresh when their
own token refresh lands it. The routing writer's fenced write and the reload after it then take the account out of
selection. Until that reload the credential cache refuses the marked credential, so no request dials the account on the
token it still held. A rejected replay's mark refuses the replayed token's lineage in the credential cache the same way,
so the token the upstream just refused is not handed to another request.

A stream commits at its `message_start` event or its first `error` event, whichever comes first. Earlier events are
buffered, up to 64 KiB, and relayed after commit in order, and a stream that exceeds the buffer or sends no committing
event within 30 seconds commits as received. The failover visits every eligible account once per request, since every
failover row (a credit-balance 400, a 401, a 403, a model 404, a 429) refuses before any inference and a visit costs no
tokens. The same-account retries keep their bounds: one transport retry after a failure that happened before the
request was fully written, one refresh replay, one strip retry when an upstream answers 400 naming a beta or field the
router itself added (the router strips it and retries once on the same account, correcting its own edit), and one retry
in place for a short throttle. The connect and the TLS handshake toward the upstream have their own bound of 10
seconds. A handshake that stalls is retried once on a fresh connection and then answered `502 upstream_unreachable`, so
a stalled upstream costs at most 20 seconds before the client hears.

The Accounts view shows what an account is cooling under in a cooling-down block, one line per scope, the whole account
or one model, with the reason, the deadline and a clear button, and the Models view lists the same rows grouped by
model ([Accounts](console.md#accounts), [Models](console.md#models)).

### Retry in place

A throttle on a portable request whose `Retry-After` is at or under the pool's `retry_in_place_seconds` (0 to 300,
default 60) is waited out on the same account and retried there once, with the same credential. The relay sleeps the
accepted delay before any byte of the answer's head reaches the client, so a bound above the client's first-byte window
makes the client end the request and send it again. [When the client waits](client.md#when-the-client-waits) states
the relation and the operator's choice. The cooldown row is still recorded, so a sibling request selecting meanwhile
skips the account, and the session's pin stays where it is. The retry is a wire submission like any other and runs once
per leg. A second throttle on the retry, a `Retry-After` past the bound or past the request's deadline, a throttle that
names no `Retry-After`, and a request bound to its account all take the failover path above.

### In-flight caps

Every wire submission counts against its account from its charge until its response ends: a buffered answer when it is
returned, a stream for its whole length, whether it finishes, fails or the client hangs up. The first attempt on an
account is charged by the selection that chose it, so two selections answered together never read the same free slot,
and a retry on the same account is charged at its dispatch. The count is kept by the router itself, and no report is
involved. A token count is not counted: it occupies no slot, is never passed over at the cap and is never held for one.
An account at the pool's `in_flight_cap` (default 16) is passed over while a sibling is under it, even a sibling above
`select_below_utilization`: the cap is a bound, the ceiling a preference. Only when every account that could serve is
at its cap is the request held: it waits for a slot on the account it would have chosen, and every hold of one request
shares one bound of `retry_in_place_seconds` from the first hold, whatever the pool's size. A slot that frees inside it
lands the request there, and past it the request is selected again with that account passed over. With every account
passed over it is answered `429` with the error code `accounts_at_cap` and a `Retry-After` of the pool's
`retry_in_place_seconds` held between 1 and 19 seconds: the client waits out a `Retry-After` under 20 seconds and treats
a longer one as a block it does not retry. A request is held at most once per account and never past the request's one
bound: a hold that runs out, or whose freed slot another request takes first, adds the account to the request's
attempted set and the selection runs again. A request that dialed an account before its holds ran out, and a pool with
no account to hold for, keep the `503 accounts_unavailable` below. A request whose session already holds a pin under
any agent is never held, and a move whose every destination is at its cap is not made: the pin renews in place and the
next request judges the move again. A held subagent waits where its cohort is.

### When nothing can serve

Each answer below has one cause, and this section states the cause. The answer's status, its error type and its
message are the refusal table's ([Refusal codes](client.md#refusal-codes)).

With every account in the pool rate limited, the answer is `429` `accounts_exhausted` with `retry-after`,
`anthropic-ratelimit-unified-status: rejected`, `anthropic-ratelimit-unified-reset` and
`anthropic-ratelimit-unified-representative-claim` naming the window. A cooldown that came from a bare `Retry-After` is
rendered as `five_hour`, because the client treats an unknown claim as no claim. With every account at its in-flight
cap past the hold, the answer is `429` `accounts_at_cap` with a `Retry-After` of at most 19 seconds ([in-flight
caps](#in-flight-caps)), since the pool frees on its own. For anything only an operator can clear, such as a pool whose
accounts are all paused, the answer is `503` `accounts_unavailable` with no `retry-after`. A request whose key moved to
another pool between its admission and its selection gets the same `503` with `Retry-After: 1` rather than being
dispatched under the old pool's policy: the router logs a warning naming the key, both pools and the obstacle
`pool_moved`, the request row reads rejected rather than exhausted, and the next request admits under the new pool. A
request admitted under a key that was deleted, or deleted and created again under its id, before it was routed is
refused with the `401` admission answers a key the registry does not hold ([What the proxy
accepts](client.md#what-the-proxy-accepts)). A pool whose every member has reached its spend budget answers `429` with
the `overage` claim, the same one an account in overage carries ([Pools](#pools)), and a probe pinned to a member at
its budget is answered with the end of the latest period whose budget is reached as its reset, whatever its siblings
hold. A probe whose pinned account is disabled, past its subscription's end, or a member with a budget while the ledger
is degraded is answered `503` `accounts_unavailable` whatever its siblings hold, and so is one that already tried its
pinned account while a sibling could take it, where moving would otherwise land the pin on that sibling. Such a probe
never moves the pin, and its request row reads rejected rather than exhausted, so the `pool_exhausted` condition of the
`phoebis_attention` gauge does not count it. A probe that already tried its pinned account with no sibling to take it
gets the pool's own answer above.

Every answer the router commits loses the upstream's rate-limit headers, the `anthropic-ratelimit-*` families and the
`anthropic-fast-*` token family, so an organization's standing never reads as the key holder's own and a client sees
one quota, the router's. A rate limit keeps them: the upstream's own when the relay forwards a 429 or a stream whose
first event is a `rate_limit_error`, and the three unified headers above when the router composes the exhaustion
answer, so the client's continue arms on the reset. The upstream's `anthropic-organization-id` and `set-cookie` are
never forwarded, and `request-id` passes either way.

A request sent fast that no account's fast lane can take is answered locally, before any dial, with a `429`. While any
account holds only a fast rate limit, the code is `fast_lane_exhausted` and `retry-after` the earliest fast rate-limit
reset, in whole seconds rounded up and at least one. When every account the pool could use is refused usage credits,
the code is `fast_credits_unavailable` and `anthropic-ratelimit-unified-overage-disabled-reason` carries the session's
pinned account's reason, else the first account's. The router never waits for a fast reset in place: a fast `429`
timed by `Retry-After` alone retries in place under the pool's bound as any throttle does ([retry in
place](#retry-in-place)), and any other moves the request to a sibling or ends in one of these answers.

A model every account in the pool has rejected within the rejection window answers `404` `model_rejected`, and the
[cooldowns table](#cooldowns-and-failover) says how a rejection is recorded. A pool with no member answers `503`
`accounts_unavailable`, since only an operator can add one.

While the ledger's writer channel is at least half full, a request, a keepalive probe included, is refused before any
account is chosen with `503` `ledger_behind` and `Retry-After: 5`, so the router never accepts a request the ledger
cannot record. The writer's standing is on [The ledger writer](statistics.md#the-ledger-writer).

Claude Code resumes a subscriber session on its own, after a usage-limit reset, only when the `429` carried all three
of `anthropic-ratelimit-unified-status: rejected`, `anthropic-ratelimit-unified-reset` (Unix seconds) and
`anthropic-ratelimit-unified-representative-claim` naming the window (`five_hour`, `seven_day`,
`seven_day_overage_included`, `seven_day_opus`, `seven_day_sonnet` or `overage`), with the reset at most 24 hours away.
`Retry-After` alone arms nothing, and the client also skips arming when the claim names a model-scoped window for a
model the session is not using.

## Overage

A pool's `overage_policy` chooses what happens once an account starts billing extra usage:

- **Prefer plan quota** (`prefer_plan`): a pinned session on this account moves to a plan-funded subscription sibling
  with allowance, warm or cold, once the reset the overage evidence names is further away than
  `overage_move_grace_minutes` (default 30). An API-key sibling is never its destination, since a move onto an API-key
  account is kept for a hard reason. A hard reason on the account instead, such as a spent window or a cooldown, takes
  the move it already has, which may land on an API-key sibling. An API-key member returns to plan quota the same way,
  independent of this policy, once its pin is cold or has sat on the account for `source_dwell_seconds`
  ([Moves](#moves)).
- **Allow** (`allow`): ignore extra usage.
- **Never** (`never`): treat an account in extra usage as unavailable until its window resets.

Under `allow`, a subscription account whose five-hour window, seven-day window or model week is spent stays a candidate
on the overage path for every model, since extra usage funds every model: the router sends the request, and a `429`
cools the account. For a model with a week of its own, whose narrowing reads those windows, the account orders behind
plan-funded siblings and drops only while the poll reads its extra usage as blocked (disabled, a reason named, or the
spend cap reached). A credits state the poll has not read yet drops nothing. A credits reading with room, taken after
the `429`, lifts the account's spent-window cooldowns (the account-wide rate limits that name a plan window and the
model-scoped week rows) as it lifts the fast lane's credits refusal. Otherwise the rows end on their own at the
window's reset, at `Retry-After` or after the fallback. Under `prefer_plan` and `never`, a model with a week of its own
passes the account over until its spent window resets.

An account is in overage when its latest quota evidence says so, and only newer evidence, by the upstream's own clock,
takes its place. A response head sets the flag when `anthropic-ratelimit-unified-overage-in-use` is true, and clears
it when the header is present and false, or absent while the shared window headers are present. The poll's snapshot sets
the flag when `extra_usage.is_enabled` is true and the 5-hour or 7-day utilization has reached 1.0, and clears it when
both shared windows are below 1.0. An account with extra usage enabled keeps serving with the flag set once its plan
quota empties, so `prefer_plan` and `never` act through this flag rather than through a cooldown. Once the
organization's spend cap is reached it answers `429` with the overage window rejected and the claim
`seven_day_overage_included`. Both shared windows still read healthy, so the cooldown path scopes that rejection to the
requested model on its own week when it has one (the Fable week) and to the account otherwise.

Whether the flag still counts is one rule every reader applies alike, the selector and the keepalive scheduler
included: an account counts as in overage while the reset its evidence names is ahead of the present instant. Evidence
that names none stands for the cooldown fallback (`routing.cooldown_fallback_seconds`) past its observation, and is
then re-read against the same rule.

The poll's `extra_usage` state also decides fast eligibility: an account whose credits read disabled, name a reason or
stand at their limit serves no fast request until a later reading shows room. The newest reading per account is kept
in the registry beside its windows and loaded at startup, so a restart judges fast requests on the credits the last poll
read. It goes with the windows when the account is deleted, and an account created again under the same id starts with
none. A request sent fast draws usage credits and no plan window, so neither `prefer_plan` nor `never` holds it off an
account in overage.

## Subscription end

An operator can mark an OAuth account cancelled and save the end it stops serving at. The account form prefills that
end from the profile's start, or a trial end already passed, advanced to the next occurrence of the same day of the
month and time of day, editable before it is saved. A trial end still ahead counts as an end the same way, even on an
account nobody has cancelled.

Inside `wind_down_minutes` (default 30) of its end, an account takes a fresh session only as the last resort, and a
cold pin moves off it once another account can serve. A warm session stays (the **Drain** rule of [Moves](#moves)).
Past the end, a cancelled account is the obstacle `subscription_ended`, and the upstream refuses the dead credential, so
the session's next request fails over to a sibling.
