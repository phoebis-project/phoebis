# Pools, keys and routing

A pool holds one or more accounts, each with a priority, an account belongs to one pool, and a key routes into exactly
one pool. Every request a key presents is placed on one account of that pool by a selection that honors the session's
pin, the pool's strategy and each account's quota. This page is for the operator tuning pools and keys and reading why a
request landed where it did: every field of a pool and a key with its default and bounds, the client classes, the model
rules, the selection itself, the cooldowns and the failover, and the answers a client gets when nothing can serve it.
The views that edit these settings are on [The console](console.md), and the codes a refused client meets are on
[Refusal codes](client.md#refusal-codes).

## Catalogs

An account's model list is read live from `GET /v1/models` on `api.anthropic.com` with the account's own credential, at
startup, on the refresh interval and on demand. The listing is held until the scheduled refresh
(`routing.catalog_refresh_seconds`) or an eviction ends it, and a failed fetch for 60 seconds from the moment its answer
arrives. The Accounts view's catalog popup ([Accounts](console.md#accounts)), the rule editors' suggestions and the
proxy's [models listing](client.md#the-models-listing) all read that cache, and `GET /admin/accounts/{id}/catalog`
answers it, with `?refresh=1` to read again. Nothing about routing depends on it: every enabled member of a pool is a
candidate for any model the rules allow, and an upstream that does not serve a model to an account says so with a `404`
the router records as a rejection ([cooldowns](#cooldowns-and-failover)). An account whose fetch failed contributes
nothing to a listing until the next fetch, and the popup shows the error.

The catalog lists some models under a dated id (`claude-sonnet-4-5-20250929`) while Claude Code asks for the undated one
(`claude-sonnet-4-5`). An undated spelling is an alias for the newest dated snapshot the listing holds, so the router
derives the dated id from the listings it caches: the newest `<spelling>-YYYYMMDD` entry across every enabled account,
else the spelling itself (a dateless id such as `claude-sonnet-4-6` is its own snapshot). The listings are fetched at
startup for every enabled account before the proxy listener accepts requests, waiting `routing.startup_fetch_seconds`
(default `30`) at most, and an account still fetching then finishes in the background. A request's model is resolved by
a lookup in that cache, never by a request of its own. The dated id is the canonical name the rules, the ledger, the
pricing, the pins and the cooldowns use for that spelling from the session's first request, and the body toward the
upstream keeps the client's spelling. A resolution is kept across restarts: the first request after a restart carries
the dated id from the stored mapping even before the listings arrive, and only a fresh installation that reaches no
listing at startup runs requests under the spelling, until the listing arrives. A listing that later lacks the dated
entry never takes a resolution back.

The listings are refreshed every `routing.catalog_refresh_seconds` (default `21600`, six hours), once more within a
minute of an upstream answering under a different snapshot than the router resolved (the alias moved), and on demand
from the Accounts view's catalog popup and `GET /admin/accounts/{id}/catalog?refresh=1`. The popup lists the effort
levels each model reports supporting, when the listing names them. A refresh that finds a newer snapshot for a spelling
names the next requests by it. The pins and cooldowns already keyed on the previous dated id stay where they are, since
a snapshot is a model of its own, and the stored mapping is updated in the same transaction.

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
| `window_ceiling` | `0.95` | 0.5 to 1: `window_ceiling is between 0.5 and 1.0`. See Selection and Moves |
| `model_rules` | empty | an ordered list of `{pattern, not}`, at most 256: `model_rules holds at most 256 entries`. See Model rules |
| `load_shedding` | `true` | a switch: off, no round runs. See Moves |
| `shed_cap` | `0.5` | 0.1 to 1: `shed_cap is between 0.1 and 1.0`. See Moves |
| `burn_average_seconds` | `1800` | 600 to 7200: `burn_average_seconds is between 600 and 7200`. See Moves |
| `shed_dwell_seconds` | `900` | 300 to 7200: `shed_dwell_seconds is between 300 and 7200`. See Moves |
| `migrate_when_spent` | `true` | `true` or `false`. See Moves |
| `warm_sessions` | `when_needed` | `always`, `never` or `when_needed`: `warm_sessions must be always, never, or when_needed`. See Moves |
| `slow_down` | `false` | a switch: off, no request is held. See Slow-down |
| `slow_down_stretch_seconds` | `9000` | 60 to 18000: `slow_down_stretch_seconds is between 60 and 18000`. See Slow-down |
| `slow_down_hold_seconds` | `120` | 1 to 300: `slow_down_hold_seconds is between 1 and 300`. See Slow-down |
| `retry_in_place_seconds` | `60` | 0 to 300: `retry_in_place_seconds is 0 to 300`. See Retry in place |
| `in_flight_cap` | `16` | at least 1: `in_flight_cap is at least 1`. See In-flight caps |
| `overage_move_grace_minutes` | `30` | 0 to 300: `overage_move_grace_minutes is 0 to 300`. See Moves |
| `wind_down_minutes` | `30` | 0 to 10080, a week: `wind_down_minutes is at most 10080 (one week)`. See Subscription end |
| `return_to_plan` | `always` | `always`, `when_cold` or `never`: `return_to_plan must be always, when_cold, or never`. See Moves |
| `return_to_plan_dwell_seconds` | `900` | 0 to 86400: `return_to_plan_dwell_seconds is between 0 and 86400`. See Moves |
| `keepalive` | `enabled` off, over the API without the block and in the console's create form | a policy object of four fields, on [The four fields](keepalive.md#the-four-fields) |
| `cache_ttl` | off | a policy object, `inject_when_absent` off by default. See Cache TTL |
| `cache_diagnostics` | `false` | `true` or `false`: `cache_diagnostics must be true or false` |
| `auto_mode_server` | `true` | `true` or `false`: `auto_mode_server must be true or false`. See below |
| `body_capture_hours` | `0` (off) | at most `stats.body_capture_max_hours`: `body_capture_hours exceeds stats.body_capture_max_hours`. The console proposes 24 |
| `guardrails` | unchanged when absent | an ordered list of at most 64 ids, refused on a create, the refusals in the paragraph below |
| `expected_revision` | unset | the `revision` the pool was read at, a non-negative integer: `expected_revision is the revision the row was read at`. An edit lands only while the pool still carries it, else `409 the pool changed while it was being edited, so reload and try again` with the code `stale_edit` and nothing written. A create accepts it and ignores it |
| `members` | none | a list of `{account_id, priority, budget_daily_usd, budget_monthly_usd}`, written as the whole membership when present, as `PUT /admin/pools/{id}/members` does |

`guardrails` is the pool's guardrails as an ordered list of ids, replaced whole on an edit. A pool's guardrails cover
every key of the pool and run before the key's own, in this order, which the pool dialog's Guardrails group arranges
([Guardrails](guardrails.md)). Naming a guardrail twice answers `400 guardrails lists {id} twice`, a list past 64 `400
pool {id} would hold more than 64 guardrails`, a list that with a key's own list takes a key of the pool past 64 `400
key {id} would hold more than 64 guardrails`, the member on a create `400 guardrails applies on edit`, and an unknown
guardrail `404 guardrail {id} not found`, in each case with nothing written. The write moves the stamp of every
guardrail it attaches or detaches, so a save of that guardrail opened before it is refused as stale.

An unknown field in the body, or a value past its field's type, is answered `400 body must be a JSON object`, and a
create under an id a pool already holds `409 a pool with this id exists`. A member's `priority` is a number, `0` by
default, and lower sorts first. Naming an unknown account in `members` answers `404 account {id} not found`, naming one
twice `409 members name account {id} twice`, and naming an account that is a member of another pool
`409 account {id} is a member of pool {pool}`, the refusal that keeps an account in one pool. In each case nothing is
written. A pool with keys cannot be deleted: `409 pool still has keys`.

Every member carries a daily and a monthly spend budget, `budget_daily_usd` and `budget_monthly_usd`, each unset by
default (no cap), above zero when set (`budget_daily_usd is above 0`, `budget_monthly_usd is above 0`) and measured over
the UTC calendar day or month. An API-key member's whole spend counts. A subscription member's metered spend alone does,
its overage traffic and its fast answers (a fast answer draws usage credits whatever the overage state), priced at API
list rates from the rollups filed under `overage` and `fast`, so a budget on one bounds its extra spend in dollars from
the router's side while its plan-funded traffic is never counted. A member whose spend reaches either budget is skipped
for requests until that period ends, the month's end whenever the monthly budget is reached, and a keepalive probe
pinned to it waits rather than moving the pin. A pool whose every member is at its budget answers `429` with the
`overage` claim, the same one an account in overage carries. The spend sums the member's hourly rollups, and the rows of
an active budget period outlive `stats.rollup_retention_days` until that period ends. While the ledger is degraded (a
batch of records failed to land and is being retried), a member with a budget reads `budget_unknown`: it is not placed
on, a session pinned to it moves to a sibling without a budget, and a pool whose members all carry budgets answers
`accounts_unavailable` until the writer catches up and reads the spend afresh. No operator action is needed, and the
state clears on the next landed batch.

`prefer_subscription` orders the set by kind (subscription accounts with plan quota, then those in overage, then the
paid sources: API keys and usage-based seats) before priority. It is the operator's answer alone, on by default, and no
strategy overrides it. The pool dialog's "Prioritize subscription accounts" toggle sets it.

Each strategy's rule, as the pool dialog's Strategy hint states it:

**`pace`**: Pace scores each member by its spare rate: the allowance left in the window it reads, the model's own 7-day
Fable window where the model has one, else the 7-day window, per hour until that window resets or the subscription ends,
less what its sessions burn of that window an hour, within the first priority tier that has an eligible account. An
API-key member scores the room left in its worst bucket. An unsampled account counts as untouched, and ties are random.

**`priority`**: Within the first priority tier that has an eligible account, any of them at random.

**`round_robin`**: Within the first priority tier that has an eligible account, each in turn. A restart starts the round
again.

On the 7-day window, Pace's allowance first loses the member's Fable reservation, the share its expected Fable use is to
take of it ([Selection](#selection)).

The console's pool dialog offers, in its expert view, `shed_cap`, `burn_average_seconds` and `shed_dwell_seconds`
together as three presets beside custom: balanced (0.5, 1800 s, 900 s, the default), fast (0.5, 900 s, 600 s) and
conservative (0.3, 3600 s, 1800 s). A preset leaves `load_shedding` as it is. It offers `in_flight_cap` and
`retry_in_place_seconds` the same way: balanced (16, 60 s, the default), fast (8, 20 s) and conservative (16, 120 s),
where balanced and conservative differ in the retry-in-place bound alone. The Pools view, its cards and the dialog are
described on [Pools](console.md#pools).

Under `when_cached` a request that carries no `cache_control` marker, and receives none from the cache TTL policy,
creates no pin and lands by the strategy instead of pinning. A non-portable request keeps its pin either way.

The pool's `cache_diagnostics`, the pool dialog's Cache diagnostics switch in the expert view, turns the cache
diagnostics on for this pool's requests toward `api.anthropic.com`: the router adds the cache-diagnosis beta ([The
betas](relay.md#the-betas)) and a `diagnostics.previous_message_id` member to a body that carries a cache marker of the
client's and a tools list, never to a token count, and never rewrites a member the client sent. The clients that send no
member (an older Claude Code, the reduced shape) are the case the insertion exists for. The member names the answer the
request continues: the newest answer at the deepest message boundary the request shares with an earlier answer of its
own conversation, or of the conversation it forked from, on the serving account, else the newest answer of the session
on that account and model. A fork names its parent's answer only until it has an answer of its own on that account and
model. Off by default.

`auto_mode_server` is whether a Claude Code session's server-side auto mode checks travel to the upstream, on by
default, and a key's own `auto_mode_server` overrides the pool's ([Keys](#keys)). What the router cuts under the off
setting and what the client does then are on [Auto mode behind the router](relay.md#auto-mode-behind-the-router).

### Cache TTL

A pool can set the prompt cache TTL of every request it serves toward `api.anthropic.com`. The policy object carries
`enabled` (default `false`), `inject_when_absent` (default `false`) and two rule sets, `plan` and `metered`: `plan`
prices a subscription account on plan quota, `metered` an API-key account, a usage-based seat or a subscription account
in overage. Each set carries `main` and `subagent` in `5m` or `1h` and `models`, a list of `{model, main, subagent}`
overrides on canonical ids. `plan` defaults to one hour throughout and `metered` to five minutes throughout: a one-hour
write costs a metered account 2x base input against 1.25x for five minutes, and a plan-funded account the same window
share either way. With the policy on, every `cache_control` marker in the body is rewritten to the TTL the request's set
and branch choose, the ones inside a tool result's own content included, the `extended-cache-ttl-2025-04-11` beta
travels exactly when the body is one hour ([The betas](relay.md#the-betas)), and the `cch` hash is restamped when the
client sent one. A restored placeholder `cch=00000` stays unsigned unless `proxy.sign_cch_placeholder` is on. A body
whose marker the relay's cache TTL guard cannot reach (a key on the marker's path spelled with an escape) is refused
`400` with the error code `body_shape`, so a TTL the policy could not enforce never travels unreported. On every other
body the client's own `CLAUDE_CODE_PROMPT_CACHE_TTL` has no effect through the pool. A body that carries no marker is
left as it came unless `inject_when_absent` is on and the request's class is not auxiliary (the class header absent, or
naming `compaction`, `main` or `subagent`), in which case the top-level `cache_control` member is added with the TTL,
before `stream`. A token count is never edited by the policy: its markers travel as the client sent them and no beta
follows it. The metered set applies while the write costs money: the serving account is in overage, is an API-key
account or is a usage-based seat. Every request, a keepalive probe's replay included, reads `main` or `subagent` by its
agent, and one without an agent id reads as the main conversation. The TTL is priced per attempt, so a request that
fails over reads its destination's state. Mixed TTLs across a session's requests are fine: the cache is keyed on the
prefix, and the TTL sets an entry's lifetime and price alone. The price of each choice is on
[Pricing](statistics.md#pricing).

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
| `companion_permissions` | `["allowance", "keepalive"]` | the words that are on, from `allowance` (the companion may read the pool's allowance) and `keepalive` (the router warms the key's sessions' caches): `companion_permissions holds allowance and keepalive, not {word}` for any other word. A repeated word is read once, and the answer lists the words that are on in that order ([The key's permissions](companion-mod.md#the-keys-permissions)) |
| `body_capture_hours` | unset, inheriting the pool's | `0` turns capture off, else at most `stats.body_capture_max_hours`: `body_capture_hours exceeds stats.body_capture_max_hours`. `null` inherits again |
| `auto_mode_server` | unset, inheriting the pool's | `true`, `false` or `null`: `auto_mode_server must be true, false, or null`. `null` inherits again |
| `guardrails` | unchanged when absent | an ordered list of at most 64 ids, refused on a create, the refusals in the paragraph below |
| `expected_revision` | unset | the `revision` the key was read at, a non-negative integer: `expected_revision is the revision the row was read at`. An edit lands only while the key still carries it, else `409 the key changed while it was being edited, so reload and try again` with the code `stale_edit` and nothing written, but an edit naming a pool that does not exist answers `404 pool not found` first. A create accepts it and ignores it |

`guardrails` is the key's own guardrails as an ordered list of ids, replaced whole on an edit. They run after the
pool's, which the key cannot drop and the key dialog draws first as inherited, and a guardrail on both the key and its
pool runs once, at its pool position ([Guardrails](guardrails.md)). Naming a guardrail twice answers `400 guardrails
lists {id} twice`, a list that with the pool's takes the key past 64 `400 key {id} would hold more than 64 guardrails`,
as does a `pool_id` change that moves the key under a pool whose guardrails, with the key's own, pass 64, the member on
a create `400 guardrails applies on edit`, and an unknown guardrail `404 guardrail {id} not found`, in each case with
nothing written. A write that attaches or detaches a guardrail moves that guardrail's stamp, so a save of the guardrail
opened before it is refused as stale.

A create under an id a key already holds answers `409 a key with this id exists`. Rotate issues a new secret under the
same id, and a revoked key cannot be rotated (`409 a revoked key cannot be rotated`). Revoke is permanent, so a revoked
key cannot be re-enabled. Delete removes the key outright and detaches its request history from the key's name.

A key's `body_capture_hours` keeps the bodies of every request under the key in the ledger for that many hours, the
pool's value when the key inherits it, at most `stats.body_capture_max_hours` ([stats](installation.md#stats)). The
console warns what the bodies hold where the operator switches capture on for a key, in the key dialog's Capture group
([Keys](console.md#keys)): "Bodies contain the prompts, file contents, and tool results of every session using this key,
and a session in auto mode sends the machine's permission rules with their literal text, its working and home
directories and its git remotes with every request. Only the admin secret can read them." The capture itself and its
reader are on [Body capture](statistics.md#body-capture).

The Keys view, the key dialog and the secret dialog are described on [Keys](console.md#keys).

## Clients

Every request carries a class, read off its user agent: `claude-code/<entrypoint>` for a `claude-cli/` caller (the
entrypoint token it names, `cli` when it names none) and `other` for anything else. The router admits Claude Code alone:
a request on `/v1/messages` or `/v1/messages/count_tokens` whose class is `other` is refused `403` `permission_error`
`client_not_allowed` with the message `the router admits Claude Code clients only` before its body is read, under every
key ([Refusal codes](client.md#refusal-codes)), and the answer carries `x-should-retry: false`. The class is the
client's own claim, read off a user agent the client sends, so the gate is a routing rule and not authentication: the
contract behind it holds every admitted request to what Claude Code sends. A Claude Code request whose `claude-cli/`
version is not spelled as the client spells one, or that lacks the headers and the body members the router reads, is
refused `403` `client_contract` before any account is chosen, the client page naming them ([The required
set](client.md#the-required-set)).

With the pool's `auto_mode_server` on, or a key's override on, the upstream reviews a Claude Code session's tool uses on
the model's own turn. Off, the client reviews them with classifier requests of its own, which the router routes as
requests of the session: they carry its id, on the client's Sonnet default. A request that carries the classifier marker
([Auto mode behind the router](relay.md#auto-mode-behind-the-router)) runs without a pin: the router looks up, creates,
renews and moves no pin for it and writes nothing to a conversation's pin but the classifier copy. It judges the
session's turn that ended last, the main agent's or a subagent's, and goes to the account that turn's stream ended on,
where that account is eligible under the same incarnation and holds allowance, since the call is free there. Else it
goes to the account the judged turn's pin stands on now, under the same conditions. Else it is placed as a request
without a pin is, by the rungs and the strategy, and the upstream charges it. A turn here is a stream of the class
`main` or `subagent`: the end of an auxiliary call, the subagent summary fork among them, names no turn. The read is
best effort: the call names no agent, so the router infers the judged turn from the order of the session's stream ends.
Two turns that end within the gap before a call, a selection repeated by a failover or a hold, and the calls after a
restart before the session's next turn ends can follow the other turn, and such a call is charged by the upstream while
the router's meter reads it free. A marked call whose committed attempt runs on the account and incarnation the judged
turn's stream ended on takes no share of a plan window on a subscription account and the normal price of its model in
the account's overage and on an API-key account ([Plan meter](statistics.md#plan-meter)). A call whose committed attempt
runs off that account and incarnation, or in a session with no turn to judge, is charged as any call and recorded
`client_off`. A charged call counts toward no pin, and the calls' usage folds into the classifier copy of the judged
turn's pin where a call ran on that pin's account, priced in dollars at the call's model. A body whose `safeguards`
member the router cannot cut under an off setting (spelled with an escape, or carried twice) is refused `400` with the
error code `body_shape` before anything reaches the upstream. The mechanism is on [Auto mode behind the
router](relay.md#auto-mode-behind-the-router).

A conversation is identified by its session header: the session id the client sends names it, keys its pin and its
session memory, and is what a turn that follows its own conversation's previous answer within milliseconds waits on, at
most five milliseconds, for that answer's report to reach the map. A new session id whose messages continue another
conversation of the same key keeps its own identity, and is recorded as a fork of that conversation at the depth where
the two part, when that depth is 2 or deeper: a shared first message is a template, not a conversation. An auxiliary
request (the client's quota probe among them) never names a fork and leaves no content entry, so no later request
matches it.

## Model rules

A pool and a key each hold one ordered list of rules, and a request's model is judged against the key's processed list:
the key's own entries first, then the pool's when the key's `model_rules_mode` is `extend` (the default), or the key's
own entries alone under `replace`. Evaluation runs top to bottom and the first matching entry decides: a plain entry
allows the model, a `not` entry denies it, and a model no entry matches is allowed. Both lists start empty, so nothing
is excluded until an operator writes an entry. The list is first-match-wins and nothing else, so an allowlist is a list
that names what it allows and ends in a catch-all denial:

- `[claude-opus-5, NOT *]` allows the one model and blocks the rest.
- `[NOT *]` alone blocks everything.

A rule's subject is the canonical id: the request's model with the `[1m]` suffix removed, ASCII case folded, and the
dated id the catalog lists substituted. One entry therefore covers a model and its `[1m]` twin, and an entry for a model
the API lists under a dated id is written as a glob: `NOT claude-sonnet-4-5*` covers `claude-sonnet-4-5`,
`claude-sonnet-4-5-20250929` and the twin of either. An entry that spells an undated id exactly (`claude-sonnet-4-5`)
never matches, because the router names that model by its dated snapshot, so an exact entry for an undated model is
written as a glob (`claude-sonnet-4-5*`) too, and the rule editors say so beside the list. A model the list denies
answers `404 model_not_allowed` without reaching an upstream.

The grammar is path-style: `*` matches any run of characters other than `/`, `**` any run including `/`, `?` one
character other than `/`, a backslash quotes the character after it, and a pattern with no wildcard matches its own text
alone. Anthropic's ids carry no slash, which is why `NOT *` reads as the catch-all above, and `NOT **` is the entry that
also covers a name with a slash. The request log's client patterns on the Requests and Analytics views
([Requests](console.md#requests)) are another grammar, where `*` crosses everything. A pattern is refused at save time
when it is empty, longer than 256 characters, ends in an unquoted backslash or holds three stars in a row, and the
refusal names the entry and the reason: `model_rules[0]: pattern is empty`, `pattern is longer than 256 characters`,
`pattern ends in an unquoted backslash`, `pattern holds three stars in a row`.

The processed list of a chosen key is the key dialog's Effective list ([Keys](console.md#keys)), the accounts cooling
for a model are on the Accounts view's cooling-down block ([Accounts](console.md#accounts)), and the model rejected and
model permitted rows are in the routing log under its Kinds filter ([Routing log](console.md#routing-log)).

## Selection

Selection keys every request by `(key id, root session, agent, canonical model)`. The router holds all routing state and
answers every request over a channel, so two concurrent first requests of the same session cannot claim different
accounts. The identity a pin is held under is the session header [Clients](#clients) names.

One read comes ahead of a live pin's renewal, the moves and the strategy: a body carrying a fallback credit token the
relay remembers is placed on the account that issued it, where that account is eligible and under its cap, or already
holds the session's live pin, and the record's `pin` step reads `match` with the fact `credit` naming it ([The
relay](relay.md#admission)). A Claude Code client's marked classifier call is the second such read: it goes to the
account its judged turn's stream ended on, else to the account that turn's pin stands on now, where that account is
eligible and holds allowance, the cap not read since the call belongs to a session already there, and the rungs and
the pick name the reason `classifier` ([Clients](#clients)). The call holds no pin, so its `pin` step reads `none` with
the reason `classifier` and the facts `judged_agent` and `judged_model` (the turn it judges), `judged` (the account that
turn's stream ended on, or the one its pin stands on where no end is recorded, `none` with no turn to judge) and
`direction` (`turn_account`, `pin_account` or `strategy`, where the call went). A call a fallback credit directs keeps
`none`, reads the fact `credit` and no `direction`.

A fresh request with no live pin lands where its prefix is already warm. The deepest message boundary any account still
holds wins first, for a main conversation and a subagent alike. Both rungs read entries of the request's own cache lane
alone: the model, the speed, the thinking configuration and, on a model without the per-turn effort form, the effort.
The upstream shares no cached prefix across those, so an entry of another lane is no warm prefix. Failing that, the
deepest static boundary places a subagent under `by_cohort` or a main conversation under `by_warm_prefix`, when the
request carries or receives a cache marker, the entry's estimate (the relay's, from the request's bytes) is above zero,
the shared prefix is at least `min_shared_prefix_tokens` for a main conversation, and the account is not at its cap,
without runway for the request (its rewrite and the burn of its sessions would take a window over `window_ceiling`
before the window's horizon, or its worst live rate-limit bucket is full for an API-key account, see [The windows the
router works from](#the-windows-the-router-works-from)), or winding down. The threshold binds a main conversation alone:
a subagent cohort lands together on any shared prefix above zero, and an entry with a zero estimate places nothing for
either. A cooling account never reaches either rung, since the eligible set excludes it. A spent account is in the
eligible set, and both rungs refuse it through the allowance check. A subagent under `with_main` lands its first request
on its parent agent's pin in place of the static walk, and one under `by_strategy` skips the static walk. Every case
that finds no live match lands by the strategy. A held request of a subagent cohort waits where its cohort already is
rather than on the strategy's own pick, and a session already on an account, under any of its agents, is never held for
a slot while that account can still serve. A token count is never placed and never held: it is bound to no cap and
occupies no slot, so its pin or the strategy decides it. It creates no pin, since it touches no cache: a count with a
pin follows it past the cap and the move rules, and one without lands by the strategy and leaves the agent's next
message to be placed under the cap. An entry's warmth lasts the marker's TTL on its tier. When every marker of the
request carried one TTL and the response reports its cache writes at the other alone, the reported TTL is the lifetime
of the message boundaries that response wrote, and of the static boundaries only when the response read nothing from the
cache, since the usage names no tier. A static tier the response read from and a tier it only read keep their marker's
lifetime, and a request with markers at both TTLs keeps each marker's.

The eligible set starts from every account that can serve the canonical model: enabled, paired or keyed, `ready` or
`funding_required` past its retry instant, off cooldown, and not already tried by this request. A request sent fast is
judged for the fast lane instead: it passes an account cooldown whose window is a plan window, and an account whose fast
lane is cooling or whose usage credits the poll reads as unavailable cannot serve it. An API-key account serves it
whenever it can serve at all.

A pin is held per agent of a conversation, where a conversation is the session id or the conversation's own messages. It
lasts `affinity_ttl_seconds` from its last use: while it is live and its account is still eligible, the request returns
to that account, and stickiness wins over priority even across a later priority change. A pin outlives its account's
membership and counts under the pool until it moves or expires. A pool with affinity off (`affinity_ttl_seconds` at `0`)
keeps no pins at all. A keepalive probe holds while `affinity_ttl_seconds` is shorter than its recipe's due time, the
cache TTL less the lead ([The five states](keepalive.md#the-five-states)). A session's creation time, the `created_at`
the Sessions view orders by, is the instant its main agent first placed a pin under the key, kept until the session's
last pin leaves for good, so a failover, a move or a second pin leaves it alone.

Narrowing then trims what is left. For a model with a week of its own (a canonical id containing `fable`), an account
whose week or either shared window reads at or above 1 drops outright, unless the pool's overage policy is `allow` and
the poll has not read the account's extra usage as blocked, in which case it stays and orders behind every plan-funded
account. An account already at the pool's `in_flight_cap` takes no fresh placement, ahead of every preference below.
With every account at its cap the request is held for a slot instead (see **In-flight caps**, below). The rest are
preferences that fall back to the wider set rather than stranding the request: the request's fit drops every account
without runway for it, and falls back to the whole set when none has it, and an account winding down toward its
subscription end (see **Subscription end**, below) is skipped for a fresh placement too. A fresh session's fit reads the
relay's estimate of the body's tokens as its rewrite and the median burn of the pool's warm sessions as its burn, each
warm session's rate at the pace it would run unheld, and reads the account's own burn as the burn the sessions already
on it would make unheld, so an account calm only because the slow-down holds its sessions reads as full (see **The
demand**, below). A window the account has not read counts as untouched, and an account inside `wind_down_minutes` of
its subscription end never fits.

What remains is ordered in tiers. With `prefer_subscription` on, the set splits into three kind groups walked in order,
plan quota, overage, then API key. Within a group, or across the whole set with the flag off, tiers order by priority
ascending, then, under `prefer_plan`, an account not in overage before one in overage. An account kept under `allow`
with a spent window joins the overage group with the flag on, and with it off sorts behind every account that is not,
whatever the priorities. The strategy picks inside the first non-empty tier. `pace` reads each account's sample as a
window map: a subscription account scores by its spare rate, the remaining allowance in its own window (the model's own
7-day Fable where the model has one, else the 7-day window) over the hours to the earlier of that window's reset and the
account's end ([Subscription end](#subscription-end)), less the burn of the sessions already pinned to the account on
that window, in share per hour. The burn it subtracts is the burn those sessions would make unheld (see **The demand**,
below), so an account calm only because the slow-down holds its sessions reads as full. An account at 0.3 used with two
heavy sessions can thus rank below an idle one at 0.6, and the score reads negative where the account spends faster than
its pace allows. On a request whose model draws the 7-day window alone, a subscription account's allowance first loses
its Fable reservation, the share of that window its expected Fable use is to take before the deadline. The expected use
is the stronger of two rates on the Fable week, each over the score's hours: the strongest clock hour of the router's
own requests on the account's Fable week among the clock hours that ended within as many hours back as the score looks
ahead (the hour still running counts at its sum so far), or the current burn of the account's Fable sessions. The Fable
allowance left caps it, and the cost model's row for the Fable week (`model_windows.seven_day_fable`) turns it into a
share of the 7-day window. The burn the score subtracts is then that of the account's other sessions, since the
reservation holds the Fable ones. The reservation orders the accounts inside the first tier, applies to those that hold
a Fable reading and refuses none, so an account whose reservation exceeds its allowance scores negative and still takes
the request when no other can. The pick line names it in the fact `fable_reservation` for each of those accounts. A
Fable request scores its own week with no reservation. An API-key account scores by one minus its worst live rate-limit
bucket's utilization alone, with no division by hours and no burn, since a bucket refills within a minute and its own
reset never enters the score. Either way a window nobody has read reads as untouched: remaining 1 over the full week. A
subscription window read past its reset reads as the router's own use since it, while an API-key bucket past its reset
is refunded and reads as untouched, with no share of the router's own. Ties are random. `priority` and `round_robin`
read no sample at all. A usage-based seat is a paid source beside the API-key accounts: it sorts in their group with the
flag on, and `pace` scores it as an API-key account, untouched since it reads no bucket.

A non-portable request, one whose body references a `file_id`, a `container` or a server-tool result, is answered by its
live pin's account or not at all. With affinity off or no live pin, it runs the normal selection instead, and the
account-scoped reference it carries may then not resolve.

Every decision is recorded in full, with what the engine read in the order it read it, and read in the console's
[routing log](console.md#routing-log). The log's words for the rules, the rungs, the partitions and the strategies are
the ones this page uses.

### The windows the router works from

The router judges a subscription account by the shared windows its window set holds (below), the 5-hour and the 7-day
window, and a model family's own week where the family table names one (its one row is Fable's `seven_day_fable`). A
weekly reading the endpoint reports under a key the table does not name (such as `seven_day_opus`) is kept, shown and
persisted, cools the account when a 429 claims it, and feeds no allowance until a row names it. The windows are read
from two sources: every response head the account answers, and a poll of the usage endpoint every five minutes per
account, each account on its own phase, backed off alone on a 429 ([Quota samples](statistics.md#quota-samples)).
Between readings the router predicts: a reading stands until its window's reset, plus the share of the window the
router's own requests consumed since the reading (priced through the subscription cost model on the account's tier). A
window the cost model cannot price for the request adds nothing and holds its reading, and a shared window the account's
set lacks takes no charge, since the account has no such window to spend. A head reports a plan window cut to the whole
percent, so inside one window and one reset a head refines the stored figure: where the router's own figure at the
head's instant, the stored reading plus the charges since, lies inside the percent the head names, the router keeps it,
and otherwise it takes the nearer edge of that percent. A poll, and a head that names another reset, land as read, so
the prediction does not fall inside a window by the head's cut. A head that reads the stored figure or more, once that
figure stands at the wall, lands as read too. At the reset it reads zero and counts the requests since, a weekly
window's reset advanced by whole weeks, the 5-hour window's next reset named by the next reading. A reading that names
no reset, taken before the reset of the reading it replaces, stands under that reset and reads zero at it. One taken at
or past that reset opens the next window and stands until the next reading, as does one where the reading it replaces
names no reset to stand under. An API-key account's rate-limit buckets stand until their reset or, without one, for a
minute. The share is priced through the account's stored tier: a tier change is read at the next profile refresh, within
a quarter hour, and applied when the router reloads, and a tier changed while the router was down is read at start. The
newest reading per account and window is kept in the registry and loaded at startup, a window a clean poll dropped
leaving the registry with its reading, so a restart, and an account whose first poll the endpoint refuses, route from
the last known windows. A reading carries the plan and the tier the account held when it was taken, which the
`window_reset` event names when its reset passes. The router never fetches usage at a selection: use of an account
outside the router shows in the next head or poll, so a decision can lag it by up to five minutes plus any backoff, and
by up to one more interval after a failed poll or a restart. The [Quota view](console.md#quota) shows when the poll last
asked the endpoint for each OAuth account and how it answered, and while an account backs off, since when and when its
next attempt comes. The `poll_backoff` condition of the [`phoebis_attention` gauge](statistics.md#conditions-and-gauges)
holds once an account has backed off for thirty minutes.

**The window set** An OAuth account's shared windows are its window set: both shared windows before its first clean
poll, and from then on the shared windows the newest clean poll's body carried, so an approved Ultra account, whose poll
carries no 5-hour window, has the 7-day window alone. A poll that lacks a shared window drops the account's reading of
it: the stored reading and the charges on it leave, the registry's row with them, and the 5-hour window's slow-down
entries end with it. A head carries some windows only and drops none, and a head's reading of a shared window the set
lacks does not land, so the window comes back with the next clean poll that carries it. A body with no shared window at
all is no reading of the account and settles nothing, and a poll older than the one that named the set neither drops a
window nor rewrites it. The set persists in the registry beside the readings and is restored at startup, so a restart
keeps the account's windows as its last clean poll named them. A model week is booked and read whatever the set holds.
The drop logs one line at info naming the account and the window and raises no routing log event of its own, though a
5-hour drop ends the account's open slow-down in the routing log at once ([Slow-down](#slow-down)). The account's cards
draw the missing window's slot as "none" with the window's words on hover, "no 5-hour window" on an approved Ultra
account ([Dashboard](console.md#dashboard), [Quota](console.md#quota)).

**Spent and cleared** A plan window read at 1.0 or above emits `window_spent` once per reset cycle, whether a head, a
poll or the router's own prediction took it there. A window read under its limit again in the same reset cycle emits
`window_cleared`, after a reading at the limit and after a predicted spent edge alike, and the clear ends the
`destination_spent` condition the spent event built ([Conditions and gauges](statistics.md#conditions-and-gauges)). The
reset's passing emits `window_reset`, by the clock walk or by the first reading whose own instant has passed it, and no
clear. A reading that names a later reset reads the stored one passed from a minute before it. An idle account's 5-hour
reset slides later with every poll, so a reading taken earlier than that moves the stored reset to the later one with no
reset event. A head or a poll that reads a plan window inside the margin, at 0.98 or above and under 1.0, while its
stored reading stands at the wall in the same reset cycle is held at the wall: the reading keeps the wall's figure under
its own instant, so it neither clears the window nor lets the next poll announce the wall again. Where neither the wall
nor the reading names a reset, the cycle runs the window's length and a minute from the wall's anchor: the instant the
wall was announced, the reset's passing for a wall that went on past its reset, or the stored reading's instant where no
announcement stands. A wall the router restored from the registry at a start keeps the anchor its stored reading gave
it, so a reading inside the margin after the restart neither moves the anchor nor holds the account past the window's
length.

**The projection** Every move and placement reads where each plan window of a subscription account is headed: its
prediction plus the burn of the sessions pinned to it under the pool's keys, priced per window on the account's tier,
the band's hours inside the horizon weighed as the band weighs them, times the time to the window's horizon. A shared
window the account's set lacks is not projected, since nothing is read or charged on it. The 5-hour window's horizon is
the earlier of its reset and the account's subscription end. A weekly window's, the 7-day window's and a model's own
week's, is the earliest of five hours from now, its reset and the end, so a weekly projection looks five hours ahead at
most. A 5-hour reading past its reset with no new reset looks five hours ahead too, since the window opens with the next
request. A shedding round and the heal of a standing mark test the projection at the horizon against the line the
horizon sets: the wall, 1.0, where the horizon is the account's end or the window's own reset, since the quota left at
either instant is lost, and `window_ceiling` where it is the five-hour bound, since no reset is known at that instant
and the quota reads on. The runways of moves and placements read the ceiling whatever the horizon. A projection's wall
instant is the instant the burn it implies over the time to its horizon takes the prediction to the wall, past the
horizon where the projection reads under the wall there, and a projection with no burn names none (see **Shed**, below).
`burn_average_seconds` (default 1800) is the time constant of each session's burn average, the figure the projection
carries forward (see **Burn**, below). A burn the cost model cannot price adds nothing, so an account whose plan the
cost model does not price (a free organization, a Team or Enterprise organization whose tier the cost model does not
hold, or a tier the profile has not named) projects its prediction alone. An API-key account has no plan window and no
projection, and neither has a usage-based seat, so the plan readings a seat kept from before it converted project
nothing. A pin whose agent ended (see **Shed**, below) adds no burn until its next request, since its agent sends no new
turn, while the pin and its cache stay.

**The demand** What the slow-down holds back is still wanted: a session held before each request burns less than it asks
to, and the moment the hold lifts it burns at its own pace again, so the router keeps two readings of a session's burn.
The achieved burn is what the window sees, the pin's decayed average priced at the band of now. The demand is that burn
over the speed the slow-down realized on the account, the burn the sessions would make unheld, one speed per account
averaged over `burn_average_seconds` as the burn is, so the two stay in step. Where a decision chooses, it reads the
demand: the fit gate on every move and placement, the `no_fit` partition, Pace's spare rate, the stretch and the factor
(see [Slow-down](#slow-down), below) and the 5-hour round under the slow-down, so an account under the slow-down takes
no new session while another fits and no move lands on an account whose calm is the hold's work. Where the router
reports a window, it reads the achieved burn: the prediction, the weekly windows' trigger and a weekly round's excess,
which are what the upstream will see. The reservation a move or a placement books carries the demand-side burn the gate
read, so the gate and the booking agree and the destination counts the session at the rate it will run at there,
unslowed until the destination's own factor says otherwise. With no slow-down in force on the account the speed is 1 and
the demand is the achieved burn. A placement's own read leaves the session's own standing row on the account it left out
of the fit and the Pace score, while every other read keeps it.

A session's runway on an account is the time until that account's projection, with the session's cold rewrite (its last
prompt as one cache write) added now and its burn added to the account's, reaches `window_ceiling`: none when the
rewrite alone takes a window there, unbounded when nothing burns, and unbounded on the Sessions route and in the
decision record's fit table where it reaches past the window's next reset, while the fit's verdict reads the computed
figure against the horizon and the strategy's tie-break ranks by it. A move reads its runway to the ceiling less half
the session's own burn over the time to the horizon, so the line that admits a moved session sits under the line that
sheds one. A session's burn average under-reads the burn its window then takes by more than half often enough that a
destination whose room covers that burn with less than half of it to spare would fill before the horizon, and the next
round would shed the session back. A fresh placement reads the ceiling. The session fits the account when the runway
reaches the horizon on every window its model draws that the account's window set holds, `five_hour` where the set has
it, `seven_day` and the model's own week where it has one, so an approved Ultra account is judged on its 7-day window
and the model week alone. An API-key account fits while its worst live rate-limit bucket reads under 1.0. A usage-based
seat fits as an API-key account does: with no bucket to read, it has room.

A move or a placement books its rewrite and its burn, on the windows its model draws that the destination's set holds,
as a reservation, which the destination's prediction and burn count until the request's usage replaces it, the request
ends without usage, or the reservation turns an hour old, so two moves in one second see each other, and a booking made
before a poll dropped a window counts nothing on it. A token count, a keepalive probe, a request sent fast and
a renewal book nothing, and a restart drops every reservation with the requests it belonged to. The pin's next request
after a landing on another account waits before its head for the first request's head, as a `held` decision with the
reason `rewrite` that names the request it waits for, up to `slow_down_hold_seconds` from that request's dispatch and
never past its own deadline, since the upstream makes a cache entry readable only once the response that writes it has
begun, so a move pays its rewrite once where two requests of the pin are in flight. A token count passes, a session's
first placement waits nowhere, a request that already waited for a slot, a reset or the slow-down is not held again, and
the wait ends at once when the first request ends without a head.

### Moves

A live pin on a portable request moves only when its session sends a request, and only by one of five rules, checked in
this order: spent, overage, return to plan, drain and shed. The first rule that finds a destination moves the pin and
names the move after itself. A pin on an account past its subscription end reaches none of them: the
`subscription_ended` exclusion releases it, as a cooldown releases a cooled account's pin. A request sent fast takes
neither the spent nor the overage rule, since it draws usage credits and no plan window.

Every rule takes its destination through one gate, the destination gate. It leaves out the source, the siblings the
rule's own test refuses and every sibling inside `wind_down_minutes` of its subscription end (see **Subscription end**,
below). It reads the moving session's fit on each sibling left (see **The projection**, above), its runway read to the
ceiling less half its own burn over the time to the horizon, the margin a move keeps, and each sibling's burn read as
its demand (see **The demand**, above), and sorts them into three orders: the siblings the session fits, else the
siblings where its rewrite lands under the ceiling but its burn runs out before a horizon, those with the longest runway
among them with every tie kept, else the rest, the least bad. The rule answers the first order that holds a sibling.
Among the siblings the session fits, the pool's strategy ranks them and the one with the longest runway on the binding
window takes it among the strategy's ties: under Pace the ties are the siblings at the best score, so the runway rarely
speaks and the session moves to the fitting sibling with the most spare rate, under Priority the whole best tier, so the
runway takes the place of the random draw inside it, and under round robin none, since the cursor picks the turn. Where
none fits, the one rewrite-only sibling with the longest runway takes the move, a tie left to the strategy, and the
least bad otherwise. A sibling at the pool's `in_flight_cap` stays out of the first two orders, so a move does not pick
a full account while a free fitting one exists. A move's fit reads the windows the destination's window set holds, so a
window the set lacks is no window. A sibling without a reading of a window the model draws that its set holds, or whose
tier the profile has not named, reads least bad for a move, since its rewrite cannot be priced there. A placement reads
such an account as untouched instead: an OAuth account without a tier fits with an unbounded runway, so of its windows
only a spent one keeps a fresh session off it, however high its utilization reads. The shed rule moves a marked pin only
to a destination it fits, in the same order, the strategy first and the longest runway among its ties, and keeps the
mark otherwise. The decision record names the order the gate took, lists every sibling it read with its projection,
rewrite, runway and verdict, and names the siblings the runway kept where it narrowed the strategy's tie.

**Spent** The pinned account's allowance for the session's model is spent: the model's own week and both shared windows
for a model with a week of its own, the shared plan windows otherwise (any of those readings at or above 1.0), while
that reading stands (until its reset, or for one that names none, until the reset of the reading it replaces when taken
before that reset, else until the next reading). With `migrate_when_spent` on (the default), the pin moves on this
request to a sibling that holds allowance in those windows, without waiting for the upstream's 429. The siblings that
hold allowance pass the destination gate's three orders, so the session lands on one it fits when one exists, else on
the one its rewrite fits with the longest runway, else on the least bad. When every spent window of the account names a
reset within `retry_in_place_seconds`, the request waits in place for the reset instead of moving cold, once per
account, and the decision reads held for the reset. The wait runs to the later of the reset and the end of any cooldown
the 429 set, so the request does not select again on a cooling account, and it holds only when that later instant falls
within the bound and the request's own deadline. The router has to know the sibling's allowance: an API-key account or a
usage-based seat has no plan window to spend, and any other sibling needs a reading of each of those windows, from a
head, the poll or the windows kept across restarts, carried forward as the windows passage says. A sibling with none
stays out of the choice. Under `prefer_plan` a sibling in overage does not receive a spent pin, since the overage move
would hand the session straight back. With the toggle off, or no sibling holding allowance, the session stays.

**Overage** Under `prefer_plan`, a pin on a subscription account in overage returns to plan quota: warm or cold, it
moves to a subscription sibling that is not in overage, never to an API-key account. The sibling has to hold allowance
too, since a spent one would move the session again on its next request. It waits while the reset the overage evidence
names falls within `overage_move_grace_minutes` (default 30, `0` for no wait), so a session whose account gets its plan
back soon keeps its cache and moves at most once. Evidence that names no reset moves the pin at once. A usage-based seat
is no destination either, since it holds no plan quota.

**Return to plan** A pin on an API-key account moves to a subscription sibling that holds allowance and is not in
overage as `return_to_plan` says. Under `always` (the default) it moves once its cache is cold, or once it has sat on
the account for `return_to_plan_dwell_seconds` (default 900, 0 to 86400) since it landed there, so a dwell of 0 returns
it at its next request, warm or cold. Under `when_cold` it moves only when the move is free: a compaction, a restart or
a lane change, which read the request cold (see **Warmth**, below). Under `never` it stays, and leaves the metered
account on expiry, a cooldown or a mark alone. An API key names no reset, so no grace applies, and the dwell is this
rule's own: `shed_dwell_seconds` paces the rounds and the no-return keep, `return_to_plan_dwell_seconds` the return of a
warm metered pin. A pin on a usage-based seat returns to plan quota as a pin on an API-key account does. No pin returns
onto a usage-based seat, which holds no plan quota.

**Drain** A cold pin on an account winding down toward its subscription end moves to an eligible sibling that is not
winding down. A warm one stays (see **Subscription end**, below).

**Shed** With the pool's `load_shedding` on (the default), a request pinned to an account starts a shedding round there
when one of the account's plan windows crosses its trigger. No round fires on a usage-based seat, whose old plan
readings project nothing (see **The projection**, above). The windows are tested in the order `five_hour`, `seven_day`,
then each model week a pin of the pool on the account draws, and the first that crosses fires and is the round's window.
A weekly window, and the 5-hour window of a pool whose `slow_down` is off, crosses when the account's projection on it
at the window's horizon reads over the line that horizon sets (see **The projection**, above), the wall where the
horizon is the account's end or the window's own reset and `window_ceiling` at the five-hour bound, at a pinned
selection and still reads over it at a pinned selection a wait later. The 5-hour window's horizon is its reset wherever
its reading names one, so on a pool whose `slow_down` is off that window sheds only once its projection at the reset
passes the wall. The wait is a quarter of the time from the first reading over `window_ceiling` to the window's wall
instant as that selection reads it, and at most twice `burn_average_seconds` (an hour at the default), the cap alone
while the projection's burn reads nothing. A burst toward a near wall instant fires early and a slow climb waits, and
one large turn's charge, which moves the prediction before the burn average has absorbed it, fires no round at the
selection it lands on, since the wait starts there. Where the line is the wall, the time a window reads between the
ceiling and the wall counts toward its wait, since the wait runs from the first reading over the ceiling. The routing
log's record of the selection names each projected window's wall instant beside its horizon, `wall_seven_day` beside
`horizon_seven_day` for the week, and none where the window's burn reads nothing. The wait's stamp, the first reading
over the ceiling, belongs to the window whose reset it was set under, so a reading whose reset lies an hour or more from
that one, earlier or later, opens a new window whose wait starts at that reading, while a smaller move is the upstream's
drift inside one window. The wait holds the round back and nothing else: the request is served as before, a mark already
standing moves at its pin's next request, and a window inside its wait yields to the next in the order. The 5-hour
window of a pool whose `slow_down` is on crosses when the stretch the slow-down reads has stood over
`slow_down_stretch_seconds` for its safety wait, and the round's horizon moves earlier by the threshold (see
[Slow-down](#slow-down), below). No 5-hour round fires while the window is younger than twice `burn_average_seconds` (an
hour at the default), whatever fired the trigger, because each session's burn average still carries the previous
window's last half hour over the reset and the projection would apply that rate to a horizon of up to five hours. While
the hold runs, the routing log's record of the selection reads the shed rule `no` with the window's age
(`age_five_hour`) against the hold (`hold_five_hour`), the stamp and the wait keep running, a weekly window past its
wait fires in the 5-hour window's place, and a standing mark still moves. The weekly windows take no hold, since the
same burst after a weekly reset is a small share of a week over a weekly horizon. A weekly window's horizon lies five
hours ahead at most, so a weekly wall is shed only in its last five hours, when the gate also stops placing new sessions
there. A round runs on an account at most once per `shed_dwell_seconds`, and a selection runs one round at most. The
excess is the burn that, removed now, lands the fired window's projection at its horizon on the line it crossed, the
wall or `window_ceiling`, and under the slow-down the demand's projection at the moved horizon on the ceiling. The
round's target is the lesser of the excess and `shed_cap` (default 0.5) of the burn of the round's pins on that window,
so the excess sets the amount and the cap bounds it.

With `load_shedding` off no round runs and nothing is marked, and a mark already standing is released at the session's
next request, which renews the pin in place. The destination gate, the reservations and `window_ceiling` on every move
and placement stay in force. The spent move has its own toggle, `migrate_when_spent`, which the switch does not touch.

The round orders the pool's pins on the account, cold ones first by burn, then warm ones by burn over rewrite cost (the
pin's last prompt size in tokens, or for a pin no call has renewed with usage yet, the prompt its move would rewrite:
the body of the request the round runs on for that request's own pin and nothing for any other pin), a pin whose burn
the cost model cannot price after every priced pin of its group, each burn read from the pin's average decayed with
`burn_average_seconds`, and as its demand under a round the stretch fired, so the round's pins add up to its excess.
`warm_sessions` bounds the order: `never` stops after the cold pins, `when_needed` (the default) goes on to the warm
ones, and `always` orders every pin by burn over rewrite cost, cold or warm. A pin a former pool left on the account is
outside the round until it ends. While the account runs on its plan, a pin whose last refreshing request was sent in
fast mode is out of the round: fast mode draws usage credits and no plan window, so the pin has no share to shed. In
dollars, on an API-key account or one in overage, it stays in the round. A session that returns to standard requests
brings its pin back with the plan burn it still carries. A pin whose model does not draw the window that fired is out of
the round too (`off_window`), so a 7-day Fable round walks the account's Fable sessions alone and an Opus session on the
account is neither counted nor moved by it. A pin whose agent ended, a subagent whose last answer stopped with
`end_turn` or an agent the companion mod released, is out of the round as well and out of every figure the router reads
an account's pins for (the burn, a fresh session's prior, the model weeks a round tests, a move's fit, the projection
and the slow-down's shares), and the round's record keeps its row under `ended`. Its pin, its account and its cache
stay, since the main session can respawn the agent with its whole context, and the agent's next request brings the pin
back into the walk and into every figure, the projection that fires a round among them (see **The projection**, above).
The mark is stored with the pin, so a restart keeps such a pin out of every figure until the agent's next request. A pin
with no priced burn on the fired window is out of the round too (`no_burn`), since it has nothing to shed. The round
then walks the pins in that order while the burn it removed, each pin's burn on the fired window, is under the target. A
pin with a standing mark counts its burn as removed and is skipped as `standing`, since its move is pending. The round
never marks a pin whose mark would leave the account's other unmarked priced pins with less than a tenth of the round's
burn sum on the window (`last_pin`), so a pool with one session burning there sheds nothing, no round takes all of an
account's burn, and a pin with almost no burn does not make its heavy neighbor markable. A pin whose burn the cost model
cannot price counts nothing and is never marked (`unpriced`), so a session is never shed on a guess, and while such a
pin stands unmarked it is the pin that stays, so the round refuses no mark as `last_pin`. A pin the shed rule's gate
finds no destination for is skipped as `no_runway` and counts nothing. Every other pin is marked with the destination
the gate planned for it, and the walk counts that destination's planned rewrite and burn for the pins after it, so two
marks do not take one sibling's headroom twice. A pin is indivisible, so the order serves a target larger than one pin's
burn, and once the burn of a pin the walk has not reached covers what the target still asks, the walk picks instead:
among the pins whose burn covers the rest and for which the gate finds a destination, it marks the one whose move costs
the least, a warm pin its rewrite as the Pro share of the pin's model week, or of the 7-day window where the model has
no week, the window the pick compares on, one figure for the pin on every destination, and a cold pin nothing, since its
cache is rewritten wherever it runs next (under `warm_sessions` `always` every pin its rewrite, as the order ignores
warmth there), the first in the order among equals, and the marking ends there with the target met. A move's cost is the
share its rewrite spends on the scarce weekly windows, since the 5-hour window refills. The pin the pick marked carries
the note `cheapest` in the round's record and event, every pin the pick read carries the runway the gate read for it,
unbounded where it passes its window's reset, and its rewrite as the Pro-week figure, the share of the Pro 7-day window
a Pro account would spend on it, with the window the pick compared on beside it, which the routing log reads in the
chosen week, and a pick that finds no covering pin with a destination leaves the walk to the order, which marks a pin
only where its burn covers at least a tenth of what the target still asks, warm or cold. A pin under that share stays as
it is (`sliver`), since its move relieves too little of the rest, and the round's record names the share as
`mark_share`. A pin whose burn carries the removed burn past the target stays marked, and the walk stops there. A round
that marks nothing records why: `no_runway` when no priced pin had a destination, `sliver` when the order left a pin
under a tenth of the rest, `off_window`, `ended` or `no_burn` when it walked no pin and a pin it left out carried that
word, `unpriced` when no burn was priced, `last_pin` when every pin the target asked for would have left too little burn
unmarked, and `standing` when every pin walked was a pending move. Either way the round is recorded, so the dwell holds.

A marked session moves at its next request, the request that ran the round included, since the mark check follows the
round. It moves only to a sibling it fits, the one the strategy ranks first with the longest runway among its ties,
never within `shed_dwell_seconds` back to an account that shed it, and with none it renews in place and keeps its mark.
A mark from a round on plan quota moves a standard request alone: a fast-mode request of the marked session stays where
it is. A pin on an account cooling for the session's model, marked or not, is replaced rather than renewed, with
`cooldown` among its reasons: the selection's renew step does not renew it, and the placement that takes its place on
another account moves the session, its mark clearing as `moved`. The round on the account still runs, since a cooldown
can hold one model alone. An unmarked pin never moves for utilization. A mark holds until the pin moves, is released or
expires, or its account heals: when the account's projection on the marked window reads under the line its horizon sets
(the wall at the account's end or the window's own reset, `window_ceiling` at the five-hour bound) at the pin's next
request, or, for a mark the stretch made, when the stretch reads at or under `slow_down_stretch_seconds`, the mark
clears and the pin stays. A later round adds its marks beside the standing ones. `shed_dwell_seconds` (default 900) thus
has two roles, the pause between two rounds on one account and the time a shed session keeps off the account that shed
it. The marks live on the pins' rows in the registry, beside each account's last round, so a restart neither forgets a
mark nor starts a fresh round inside the dwell. A held request does not survive one (the relay's open connection closes
and the client retries), and the routing log's router started row says so.

**Warmth** A pin stays warm while its last refresh is younger than the cache TTL in force for it. A `main`, `subagent`
or `compaction` request ([`x-claude-code-request-class`](client.md#claude_code_gateway_hint_headers)) refreshes it, and
any other class leaves it alone. A pin only auxiliary requests have used since its placement stays warm while its last
use is younger than the TTL, since those requests keep its prefix warm, and reads its last refresh again once a `main`,
`subagent` or `compaction` turn refreshes it. A request that would refresh the pin reads it cold instead when it is on
another cache lane than the pin's last (a speed change, a thinking change, or an effort change on a model the client
sends no per-turn control entry for), whatever its messages continue, and the pin takes the new lane. A request that
leaves the pin alone leaves its lane as it was, and a per-turn control entry is one more message and continues the
conversation. Without the class header, a request whose messages extend the session's conversation refreshes the pin,
one that repeats the session's static prefix (tools and system prompt) without extending its messages restarted the
conversation and reads cold, and one that matches neither changes nothing. Where one session holds several
conversations, a request that extends any conversation the pin has served refreshes it. A request that carries
`x-claude-code-context-compacted` reads cold whatever else it says, so a compacted session goes first in the round's
order on the request that carries the fact. A body the router cannot read refreshes the pin on every request on the
pin's lane, and the first one after a request on another lane reads it cold, as any lane change does. A keepalive probe
replays another request's headers and body, so the router judges none of them: a probe never colds a session, and its
renewal counts as a refresh while the pin still holds the lane the probe was sent on. A renewal that lands after the
session moved to another lane refreshed another cache and leaves the pin as it was. A `count_tokens` request creates no
cache entry, so it neither refreshes nor colds the pin and leaves its cache TTL as it was.

**Burn** Each pin keeps an average of its usage as typed tokens per second (input, cache read, 5-minute and 1-hour cache
writes, output) for the pin's one model, decayed with the pool's `burn_average_seconds` (default 1800) as its time
constant, in three copies: one plain and one weighed by the peak band at each renewal for its standard answers, and a
third for its fast answers, priced at the fast cells where the model has them. The registry keeps all three on the pin's
row, so a restart reads each as saved, and a move carries them onto the pin's new row, so its destination counts the
session's burn from the moment it lands. Nothing is priced until it is read: while a subscription account runs on its
plan the round orders its pins by their average share of the window that fired over the span the round covers, the
figure the walk prices and the round card shows, and by the plain copy in dollars, at the set the pin's last prompt
selects on a model priced by prompt length ([Pricing](statistics.md#pricing)), with the fast copy added in, on an
API-key account or a subscription account in overage, so leaving overage switches the reading without a reset and a
session that moves between tiers reads the right share on both. The projection, the round's walk and the spare rate
price the plain copy on the account's tier under the regime of the pin's entrypoint ([Plan
meter](statistics.md#plan-meter)), on every window the pin's model draws that the account's set holds, so an approved
Ultra pin burns no 5-hour share, the spare rate at the band in force when it reads, and the projection and the walk
weighing the band's hours inside the horizon as the band weighs them, the walk over the span its excess covers. A
keepalive probe's usage never counts. A burn the model cannot price, for a model it has no weight for or a tier it has
no capacity for, reads as none.

Any move records a `pin_migrated` event, a rule's or a failover's with reason `cooldown`, `drain`, `fast_lane`, `mark`,
`overage`, `return_to_plan`, `shed` or `spent`, and a replaced live pin's with a reason of its own (below): `cooldown`
and `mark` when a failover or a credential mark moved the session, `fast_lane` when a fast-mode rejection moved the
session, the others after the rule above that moved it. A move a rule's gate read adds the destination's fit (`fits`,
`rewrite_only` or `least_bad`), its runway, unbounded where it passes its window's reset, and the window that binds it,
and a failover's move adds its `cause`: for a cooldown its `scope` (`account`, `fast`, `fast_credits` or `model:<id>`),
its `reason` (the cooldown row's own word, such as `rate_limit`) and its `window`, and for a mark the account's `state`
and the state's `cause`. A failover with neither, a 401 whose refresh failed with a transient error the request could
not wait out, carries none. A mark that clears records `mark_cleared` with the reason `healed` (the account's projection
on the marked window read under its line again, the wall at the window's reset or the account's end and the ceiling at
the five-hour bound, which the event names as `limit`, or its stretch under the threshold for a mark the stretch made),
`moved` or `released`. A reservation that leaves without the usage that would have replaced it records
`reservation_expired`, with the relay's word for a request that ended without usage (`canceled`, `exhausted`, `refused`
or `transport_error`) or `timeout` for a booking an hour old. A pin released without a move records `pin_expired` with
`reasons`, every fact the selection found against the account: `attempted`, `budget` (the pinned member has reached one
of its spend budgets), `budget_unknown` (the ledger is degraded and the pinned member carries a budget), `cooldown`,
`deleted`, `expired`, `fast_credits` (a fast request's pinned account whose `fast_credits` row stands or whose usage
credits the poll reads as unavailable) and `fast_rate_limit` (one whose `fast` row is the blocking row that stands
latest), `overage`, `paused`, `spent`, `subscription_ended`, `unserved`, the account's state when it cannot serve
(`disconnected`, `funding_required` until its retry is admitted, `reauth_required`, `unpaired`), or `released` when the
selection found nothing else against the account, as with a move whose destination emptied. A pin whose creating request
was answered 400, 403 or 404, or was bound to its account and ended at an authentication failure that marked nothing,
with no cooldown or mark on the account and no other turn of the session renewing it meanwhile, records the single
reason `client_error`: it never served, so the session's next request is placed afresh rather than moved.

A placement that replaces a live pin and lands on another account is a move as well, recorded as `pin_migrated` with the
reason the pin left its account for (`paused`, `disconnected`, `funding_required`, `reauth_required`, `unpaired`,
`subscription_ended`, `unserved`, `cooldown`, `fast_rate_limit`, `fast_credits`, `budget_unknown`, `budget`, `overage`,
`attempted`, `spent` or `released`, the first of that order the selection found) and with `reasons`, every fact it
found. The request's `moved` and the decision's rule carry the same word, and the decision names the account the session
left (`from_account_id`). The pin of a session idle past its TTL stays an expiry beside any other reason, and so does a
pin replaced on its own account: each records `pin_expired` with `reasons` and `pin_created`, and its placement names no
source. A deleted account's pins leave when the router reloads, as `pin_expired` with `deleted`, so a session that sat
on one is placed afresh and its placement names no source.

### Slow-down

With the pool's `slow_down` on, the router holds a slowed account's sessions before each request, so the account's
5-hour window reaches its reset under `window_ceiling` instead of hitting it. At every selection of a pinned request on
a subscription account whose plan the cost model prices and whose 5-hour window has a reading, the router reads the
window's stretch: the time between the instant the account's unheld demand would pass the ceiling and the reset,
`R - (ceiling - U) / B` for the time `R` to the window's horizon, the window's prediction `U` and the demand `B` of the
pool's sessions on it (see **The demand**, above), the demand's in-band hours weighed at one over the band factor where
a base-regime span crosses a band edge, as the projection's are. A stretch at or under zero is nothing to do, and a
window already at the ceiling reads a stretch of at least the time to its reset. The stretch is the time the slow-down
adds to every session on the account when it spreads their work to the reset, and `slow_down_stretch_seconds` (60 to
18000, default 9000) is the time the operator lets it add before a round moves sessions off the account. An API-key
account has no 5-hour window and no slow-down, and neither has an approved Ultra account, whose poll carries none: the
clean poll that drops the window ends the account's slow-down entries at once, each span recorded to the drop.

From the stretch follows one speed factor per account, `(ceiling - U) / (R x B)` clamped to [0, 1], the in-band hours
weighed at one over the band factor across a band edge as above, which is `1 - S / R` for the stretch `S`: 1 at a
stretch of zero, 0 once the window is at the ceiling, and applied alike to every pin the pool keeps on the account, so
every session there gets the same stretch. The factor lives in memory beside the reservations, written at every pinned
selection on the account and read until the next write, so a quiet account reads unslowed again at the purge tick and a
restart starts from none. An entry read before its 5-hour window's passed reset answers nothing, the entry's own reset
answering where the latest reading names none, the purge tick drops it, and a span the reset ends is recorded to the
reset. The holds apply the factor in proportion to the share of one `burn_average_seconds` that has passed since the
account's slowed span opened, so a burst of a few minutes that would project past the ceiling costs holds in proportion
to how long it has stood, and the full factor applies after one burn average. The span opens at the factor's first read
under 1 and stays open through a return to 1 shorter than two minutes, so a burst that comes back inside that wait
continues the ramp where it stood, and a return that lasts the wait closes the span and its ramp, the next burst ramping
from nothing. A window already at or past its ceiling takes its full factor at once, since its standing is measured and
not projected. The ramp restarts with every window, since the reset ends the span, and the routing log's shed line shows
the share as `ramp`, 0 wherever the factor reads 1, since nothing is held then. The holds realize the factor as a speed,
which the router averages over `burn_average_seconds` beside the burn, and the demand every reader above divides by is
that speed.

The hold is served before the response head, inside the selection, as a `held` decision with the reason `slow_down`: the
request waits the pin's cycle times `1 / f - 1` for the factor `f` the ramp leaves, so a session at half speed waits one
cycle before each request and one at a quarter speed three, capped by `slow_down_hold_seconds` (1 to 300, default 120)
and never past the request's own deadline or the 5-hour window's reset. The cycle is the pin's mean time from the
dispatch of one request to the arrival of the next over about its last eight gaps, each arrival moving it by an eighth
toward its gap and the first arrivals by their plain mean, so a pin's first requests pass unheld until a gap has been
measured and a restart starts the measure over. A gap of `burn_average_seconds` or longer is a pause that enters no
cycle, the next gap measured from the arrival that ended it, so a session that resumes after a long think or an idle
hour is held for the pace it runs at and not for the pause. The deadline belongs to the pin: a retry or a sibling
request of the same pin arriving before it and before the 5-hour window's reset rejoins it for the remaining time, so
two requests of one pin wait to one instant and a hold that crossed the client's window costs one visible retry and no
doubled wait. The first selection on the account after the reset, or the purge tick before it, drops the deadlines its
pins stood in the old window, so a request after the reset waits no deadline set before it and is read afresh. Where a
newer reading replaced that reset before any selection read it and before a purge tick saw it pass, the router no longer
knows the reset passed, and such a deadline stands until it passes. A hold under a second is not served, a request is
held for the slow-down once, and a slot or reset hold after it keeps its own bound of `retry_in_place_seconds`. A pin
whose cycle is too long for the cap is held at the cap and runs faster than the factor, and the account's other pins
share the remaining reduction at one speed. The cap stands against the client's first-byte window of 180 seconds plus 1
second per 32 KiB of body: the default leaves 60 seconds for the upstream's head and the pre-commit hold, and the pool's
warning `hold_over_window` fires above 150 seconds. [When the client waits](client.md#when-the-client-waits) states the
relation and the operator's choice.

Probes, token counts, compactions, auxiliary requests (any `x-claude-code-request-class` outside `compaction`, `main`
and `subagent`), requests sent fast, requests on an API-key account and non-portable requests are never held for the
slow-down, and the burn of each counts, so the holds of the main and subagent turns are sized to cover it. A probe has
to land before the cache expires, a count costs nothing on the window, a compaction lowers the burn that follows it, and
a fast request draws usage credits and no plan window. A request a guardrail blocks never reaches the selection, so it
is never held, refreshes no pin and adds no burn ([Guardrails](guardrails.md)).

The handover to the round: with `load_shedding` on as well, the 5-hour window's trigger is the stretch in place of the
projection. The round fires when the stretch has read over `slow_down_stretch_seconds` at a pinned selection and still
reads over at a pinned selection two minutes or more later, the safety wait, during which the slow-down already runs at
the factor the stretch implies, so the wait costs no overrun and keeps one large turn's charge from firing a round on
its own. The round's horizon is the reset less the threshold, and its excess is the demand's projection over the ceiling
at that horizon, the burn whose removal brings the remaining sessions' stretch to the threshold exactly: the round
removes only what the slow-down cannot cover, the slow-down covers the rest, and the pins that stay are slowed at the
account's speed. When the reset is nearer than the threshold the moved horizon lies in the past, so no round fires
however long the stretch reads and the slow-down alone holds the window to its reset. At the default threshold a round
moves sessions only when the demand would pass the ceiling two and a half hours before the reset, so inside a window's
last two and a half hours the slow-down alone holds the account, and a shorter threshold, one that moves sessions
earlier, is a pool's own setting. A mark the stretch made heals when the stretch reads at or under the threshold at the
pin's next request. The weekly windows keep the projection trigger on the achieved burn.

The two switches are independent. With `slow_down` on and `load_shedding` off no round runs on any window and the
slow-down holds the line to the cap, where the ceiling's margin absorbs what the cap cannot. The chain of last resorts
on the 5-hour window then reads: the slow-down, the round (with `load_shedding` on), `migrate_when_spent` on a spent
window, and the wall, where the failover lands the session as a move ([Cooldowns and
failover](#cooldowns-and-failover)). With `slow_down` off the engine is the one the sections above describe, and with
both off nothing proactive runs on the 5-hour window. The end state at the ceiling is a factor of zero: every held
request of the account waits the full cap, so an account at the ceiling serves one request per pin every cycle plus the
cap until the reset while its probes, counts and compactions pass unheld. The `shedding_off` warning fires for a pool
with both switches off alone. The account's factor, its stretch against the threshold, each hold and the two events that
bracket a slowed span, `slow_down_started` and `slow_down_ended`, are on the console's [Pools](console.md#pools),
[Quota](console.md#quota) and [Routing log](console.md#routing-log) views. The account's `slow_down` on
`GET /admin/accounts/{id}` names the factor, the realized speed, the span's start and the ramp, `GET /admin/sessions`
names the factor and the realized speed of each pin's account, and `GET /admin/pools` those of each member.

## Cooldowns and failover

The relay classifies every upstream response head before a byte reaches the client, partitioned by account kind first,
`oauth`, `api_key`, or `shared` for both. Within a partition the first matching row wins. Where a row names the
fallback, it is `routing.cooldown_fallback_seconds`, default `60`, 1 to 3600, a key of the bootstrap file's
[routing](installation.md#routing) table. Every deadline a row sets carries a second of margin, so a request sent at the
reset is not refused by a clock a hair behind.

**oauth**

| Upstream result | Cooldown | Failover |
| --- | --- | --- |
| 429, the claim naming the model's own week, the `7d_oi` or the top-level `status` rejected, 5h and 7d healthy, a window the account's window set lacks included where the head names nothing of it | Model-scoped until the later of the `7d_oi` reset and the top-level reset, else `Retry-After`, else the fallback | Yes |
| 429, the `7d_oi` status rejected, no other family's week claimed, 5h and 7d healthy as in the row above | Model-scoped, the same deadline | Yes |
| 429, a shared window (5h or 7d) or `status` rejected, the 5-hour status absent beside a rejected 7-day window included | Account-wide until the later of the rejected window's reset and the top-level reset, else `Retry-After`, else the fallback | Yes |
| 429 whose unified headers reject nothing | Account-wide until `Retry-After`, else the fallback | Yes |
| 429 without unified headers | Account-wide until `Retry-After`, else the fallback | Yes |
| 200 SSE, first event `error`/`rate_limit_error` | As the 429 rows, using the response's headers | Yes |

A shared window is healthy when its status reads `allowed` or `allowed_warning`, or when the status is absent and the
window's own utilization reads below 1.0. A window the account's [window set](#the-windows-the-router-works-from) does
not hold is healthy as well where the head carries neither a status nor a utilization for it: an approved Ultra account
has no 5-hour window and its heads carry no 5-hour family, so once a clean poll has settled its set, a rejection the two
model rows above take cools the model alone. On a window the set holds, an absent status with no utilization is not
evidence that the window is fine, nor is any other status value on either window, and the rejection then cools the whole
account. Where the rejection named a window, a reading taken after it lifts the account-wide rate-limit row before its
deadline: a claim on the 5-hour or the 7-day window recovers once that window reads under 1.0, and a claim on another
week, or on a shared window the account's window set no longer holds, once every shared window the set still holds reads
under 1.0.

The two model-scoped rows apply to a model whose family has a week of its own in the family table (Fable, whose week is
`seven_day_fable`). The row cools that model on the account. Where the rejection carried a claim, the row recovers once
a reading of that week is under 1.0, and no reading lifts a row from a rejection with no claim. The claim is the
`representative-claim` header: one naming the model's own week, or `seven_day_overage_included`, or none at all leaves
the decision to these rows. A claim naming another family's week, or a week no row names (such as `seven_day_opus`),
cools the whole account whatever the `7d_oi` status reads, and so does any of these rejections for a model without a
week of its own: under the shared row when the top-level `status` reads rejected, and under the row for headers that
reject nothing otherwise.

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
success from an attempt placed under an earlier mark leaves it standing, whatever the host's clock read between them,
since the router numbers each account's marks and compares no times. A token count may reach the account under the same
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
standard requests on the account are unaffected. A fast refusal moves the session's pin unless the pin has changed lanes
after the refused request was selected: a session that has moved on to another lane is not asking for the fast lane any
more. The lane compared is the one the selection left on the pin, so a fast request that left a standard pin's lane
alone (an auxiliary class, or a request foreign to the conversation) moves that pin too.
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
| Transport failure before fully written | None | Yes, once, same account, after about a second when no connection could be opened and at once when a reused connection died |
| Transport failure after fully written, no head at all | None | No, 502 `upstream_unreachable`, the client retries |
| Transport failure before fully written, on the retry as well | None | No, 502 `upstream_unreachable`, the client retries |
| Transport failure after the head, before any byte reached the client | None | No, 502 `upstream_cut` |
| A coding or a payload the router cannot decode, before any byte reached the client | None | No, 502 `upstream_undecodable` |
| Idle bound after the head, before any byte reached the client | None | No, 502 `upstream_stalled` |
| 200 head, body ends before its end | None | No, the request reads `incomplete` |

The router classifies a rejection whose body runs past 64 KiB from its first 64 KiB, a coded body from the text those 64
KiB decode to: a 429 cools and fails over, a 401 or a 403 `authentication_error` takes the credential path, and a body
the verdict forwards reaches the client whole, the prefix first.

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
the next member, and the ledger records `model_rejected` for the refusing account and `model_permitted` for the one that
served it. A 404 naming no model, and every 400 and 422, reach the client as they came.

Each client request visits every eligible account of the pool at most once. A same-account retry after a transport
failure before the request was written, the one refresh replay, the one strip retry and the one retry in place keep
their own bounds, as the paragraph after the steps says. Each attempt goes through these steps:

1. Ask the router. `timed` answers `429` with `Retry-After: ceil(deadline - now)` and the three unified rate-limit
   headers the client's automatic continue reads. `untimed` answers `503` with error code `accounts_unavailable` and no
   `Retry-After`, since only an operator can clear it, unless the request reached it through holds alone with nothing
   dialed: then every account is at its cap, and the answer is `429` with error code `accounts_at_cap`. Each carries an
   Anthropic-shaped error body. `held` waits for a slot and asks again ([in-flight caps](#in-flight-caps)).
2. Acquire the account's credential, apply its body edits, and send on its dispatch path.
3. Classify the head. A short throttle on a portable request is waited out on the same account ([retry in
   place](#retry-in-place)). Any other failover row on a portable request records the cooldown, invalidates the matching
   pin, and returns to step 1. On a non-portable request it records the cooldown and commits the failed response
   instead. An authentication failure is the exception: it is never committed (see the paragraph after these steps), and
   the failover leaves the pin of an account it marks for the next selection to replace. Any other row commits and
   relays the response.
4. Once the committed response ends, renew the pin with the usage it carried: the served account is final for this
   request. A pin this request created and whose answer is `400`, `403` or `404` is released instead, since it never
   served, unless another turn of the session renewed it meanwhile, and so is one a non-portable request created before
   an authentication failure that marked nothing. A `429` keeps the cooldown path of step 3.

An upstream `401`, or a `403` whose body is an `authentication_error`, never reaches the client. On an OAuth account the
router refreshes the token and replays the request on a new token. A refresh the token endpoint refused without rotating
anything is tried a second time within the transient backoff, about five seconds unless the endpoint asked for longer.
After a token endpoint's `5xx` or a lost answer the request is held on the same account for up to a minute, then
refreshed a second time, so a session keeps its warm account through the endpoint's bad minute, which is waited out once
per request. A refusal whose `Retry-After` is over five seconds and within the minute counts as that minute, so a
request that already held its minute on another account fails over at once. A `Retry-After` past that minute, a second
failure, or an API-key account's failure fails the request over to the next eligible account, and a request bound to its
account, or one with no account left, is answered the router's own `503 accounts_unavailable` with no account named. An
account the failover marks, or finds disconnected, keeps its pin until the reselect moves it to another account, under
the account's state or `attempted` ([Moves](#moves)). Any other failover moves the pin, and the move is recorded as
`pin_migrated` with its cause ([Moves](#moves)): the landing reads the session's own burn, the cooled and the refused
accounts are out, and the strategy ranks the accounts the session fits as a move's gate does, then the one where the
rewrite alone fits with the longest runway, then the least bad, then any account at all, so a session whose every
candidate is inside the wind-down is still served. The failover, its waits included, starts no new leg past the
request's own deadline, ten minutes less a margin: past it the request is answered `503 accounts_unavailable` rather
than tried on another account. The attempt rows read `auth_failed` with the upstream status, and the account reads
`reauth_required` after a conclusive failure of the refresh (an `invalid_grant`, a token endpoint's `401`) or after a
rejected replay, which the Accounts view and the `reauth_required` attention condition show. Whoever lands the mark
reports it at once: the relay for a failover or a rejected replay, and the usage poll, its profile read and the catalog
refresh when their own token refresh lands it. The routing writer's fenced write and the reload after it then take the
account out of selection. Until that reload the credential cache refuses the marked credential, so no request dials the
account on the token it still held. A rejected replay's mark refuses the replayed token's lineage in the credential
cache the same way, so the token the upstream just refused is not handed to another request.

A stream commits at its `message_start` event or its first `error` event, whichever comes first. Earlier events are
buffered, up to 64 KiB, and relayed after commit in order, and a stream that exceeds the buffer or sends no committing
event within 30 seconds commits as received. The failover visits every eligible account once per request, since every
failover row (a credit-balance 400, a 401, a 403, a model 404, a 429) refuses before any inference and a visit costs no
tokens. The same-account retries keep their bounds: one transport retry after a failure that happened before the request
was fully written, one refresh replay, one strip retry when an upstream answers 400 naming a beta or field the router
itself added (the router strips it and retries once on the same account, correcting its own edit), and one retry in
place for a short throttle. The connect and the TLS handshake toward the upstream have their own bound of 10 seconds. A
handshake that stalls is retried once on a fresh connection after a wait of about a second and then answered
`502 upstream_unreachable`, so a stalled upstream costs at most 21 seconds before the client hears.

The Accounts view shows what an account is cooling under in a cooling-down block, one line per scope, the whole account
or one model, with the reason, the deadline and a clear button ([Accounts](console.md#accounts)).

### Retry in place

A throttle on a portable request whose `Retry-After` is at or under the pool's `retry_in_place_seconds` (0 to 300,
default 60) is waited out on the same account and retried there once, with the same credential. The relay sleeps the
accepted delay before any byte of the answer's head reaches the client, so a bound above the client's first-byte window
makes the client end the request and send it again. [When the client waits](client.md#when-the-client-waits) states the
relation and the operator's choice. The cooldown row is still recorded, so a sibling request selecting meanwhile skips
the account, and the session's pin stays where it is. The retry is a wire submission like any other and runs once per
leg. A second throttle on the retry, a `Retry-After` past the bound or past the request's deadline, a throttle that
names no `Retry-After`, and a request bound to its account all take the failover path above.

### In-flight caps

Every wire submission counts against its account from its charge until its response ends: a buffered answer when it is
returned, a stream for its whole length, whether it finishes, fails or the client hangs up. The first attempt on an
account is charged by the selection that chose it, so two selections answered together never read the same free slot,
and a retry on the same account is charged at its dispatch. The count is kept by the router itself, and no report is
involved. A token count is not counted: it occupies no slot, is never passed over at the cap and is never held for one.
An account at the pool's `in_flight_cap` (default 16) is passed over while a sibling is under it, even a sibling the
request does not fit: the cap is a bound, the fit a preference. Only when every account that could serve is at its cap
is the request held: it waits for a slot on the account it would have chosen, and every hold of one request for a slot
or a reset shares one bound of `retry_in_place_seconds` from the first hold, whatever the pool's size, while the
slow-down's hold and the wait for a moved pin's first head have their own, `slow_down_hold_seconds`
([Slow-down](#slow-down)). A slot that frees inside it lands the request there, and past it the request is selected
again with that account passed over. With every account passed over it is answered `429` with the error code
`accounts_at_cap` and a `Retry-After` of the pool's `retry_in_place_seconds` held between 1 and 19 seconds: the client
waits out a `Retry-After` under 20 seconds and treats a longer one as a block it does not retry. A request is held at
most once per account and never past the request's one bound: a hold that runs out, or whose freed slot another request
takes first, adds the account to the request's attempted set and the selection runs again. A request that dialed an
account before its holds ran out, and a pool with no account to hold for, keep the `503 accounts_unavailable` below. A
request whose session already holds a pin under any agent is never held for a slot, and a move whose every destination
is at its cap is not made: the pin renews in place and the next request judges the move again. A held subagent waits
where its cohort is.

### When nothing can serve

Each answer below has one cause, and this section states the cause. The answer's status, its error type and its message
are the refusal table's ([Refusal codes](client.md#refusal-codes)).

With every account in the pool rate limited, the answer is `429` `accounts_exhausted` with `retry-after`,
`anthropic-ratelimit-unified-status: rejected`, `anthropic-ratelimit-unified-reset` and
`anthropic-ratelimit-unified-representative-claim` naming the window. A cooldown that came from a bare `Retry-After` is
rendered as `five_hour`, because the client treats an unknown claim as no claim. A usage-based seat held out by its
spend cap names the window `spend`, which the client arms no continue on, as suits a wait that can run to the month's
end. With every account at its in-flight cap past the hold, the answer is `429` `accounts_at_cap` with a `Retry-After`
of at most 19 seconds ([in-flight caps](#in-flight-caps)), since the pool frees on its own. For anything only an
operator can clear, such as a pool whose accounts are all paused, the answer is `503` `accounts_unavailable` with no
`retry-after`. A request whose key moved to another pool between its admission and its selection gets the same `503`
with `Retry-After: 1` rather than being dispatched under the old pool's policy: the router logs a warning naming the
key, both pools and the obstacle `pool_moved`, the request row reads rejected rather than exhausted, and the next
request admits under the new pool. A request admitted under a key that was deleted, or deleted and created again under
its id, before it was routed is refused with the `401` admission answers a key the registry does not hold ([What the
proxy accepts](client.md#what-the-proxy-accepts)). A pool whose every member has reached its spend budget answers `429`
with the `overage` claim, the same one an account in overage carries ([Pools](#pools)), and a probe pinned to a member
at its budget is answered with the end of the latest period whose budget is reached as its reset, whatever its siblings
hold. A probe whose pinned account is disabled, past its subscription's end, or a member with a budget while the ledger
is degraded is answered `503` `accounts_unavailable` whatever its siblings hold, and so is one that already tried its
pinned account while a sibling could take it, where moving would otherwise land the pin on that sibling. Such a probe
never moves the pin, and its request row reads rejected rather than exhausted, so the `pool_exhausted` condition of the
`phoebis_attention` gauge does not count it. A probe that already tried its pinned account with no sibling to take it
gets the pool's own answer above.

Every answer the router commits loses the upstream's rate-limit headers, the `anthropic-ratelimit-*` families and the
`anthropic-fast-*` token family, so an organization's standing never reads as the key holder's own and a client sees one
quota, the router's. A rate limit keeps them: the upstream's own when the relay forwards a 429 or a stream whose first
event is a `rate_limit_error`, and the three unified headers above when the router composes the exhaustion answer, so
the client's continue arms on the reset. The upstream's `anthropic-organization-id` and `set-cookie` are never
forwarded, and `request-id` passes either way.

A request sent fast that no account's fast lane can take is answered locally, before any dial, with a `429`. While any
account holds only a fast rate limit, the code is `fast_lane_exhausted` and `retry-after` the earliest fast rate-limit
reset, in whole seconds rounded up and at least one. When every account the pool could use is refused usage credits, the
code is `fast_credits_unavailable` and `anthropic-ratelimit-unified-overage-disabled-reason` carries the session's
pinned account's reason, else the first account's. The router never waits for a fast reset in place: a fast `429` timed
by `Retry-After` alone retries in place under the pool's bound as any throttle does ([retry in place](#retry-in-place)),
and any other moves the request to a sibling or ends in one of these answers.

A model every account in the pool has rejected within the rejection window answers `404` `model_rejected`, and the
[cooldowns table](#cooldowns-and-failover) says how a rejection is recorded. A pool with no member answers `503`
`accounts_unavailable`, since only an operator can add one. A request routed to an OAuth account whose stored uuid is
malformed is answered `503` `accounts_unavailable` on that account's leg, since the uuid cannot be written into the
body, and its request row reads rejected.

While the ledger's writer channel is at least half full, a request, a keepalive probe included, is refused before any
account is chosen with `503` `ledger_behind` and `Retry-After: 5`, so the router never accepts a request the ledger
cannot record. The writer's standing is on [The ledger writer](statistics.md#the-ledger-writer).

Claude Code resumes a subscriber session on its own, after a usage-limit reset, only when the `429` carried all three of
`anthropic-ratelimit-unified-status: rejected`, `anthropic-ratelimit-unified-reset` (Unix seconds) and
`anthropic-ratelimit-unified-representative-claim` naming the window (`five_hour`, `overage`, `seven_day`,
`seven_day_opus`, `seven_day_overage_included` or `seven_day_sonnet`), with the reset at most 24 hours away.
`Retry-After` alone arms nothing, and the client also skips arming when the claim names a model-scoped window for a
model the session is not using.

## Overage

A pool's `overage_policy` chooses what happens once an account starts billing extra usage:

- **Prefer plan quota** (`prefer_plan`): a pinned session on this account moves to a plan-funded subscription sibling
  with allowance, warm or cold, once the reset the overage evidence names is further away than
  `overage_move_grace_minutes` (default 30). An API-key sibling is never its destination, since a move onto an API-key
  account is kept for a hard reason. A hard reason on the account instead, such as a spent window or a cooldown, takes
  the move it already has, which may land on an API-key sibling. An API-key member returns to plan quota independent of
  this policy, as the pool's `return_to_plan` says: under `always` once its pin is cold or has sat on the account for
  `return_to_plan_dwell_seconds`, under `when_cold` on a free move alone, and under `never` only once an expiry, a
  cooldown or a mark ends its pin ([Moves](#moves)). A pin on a usage-based seat returns as an API-key member's does,
  and no pin moves onto a usage-based seat under either rule.
- **Allow** (`allow`): ignore extra usage.
- **Never** (`never`): treat an account in extra usage as unavailable until its window resets.

A usage-based Enterprise seat ([Two kinds of account](pairing.md#two-kinds-of-account)) never reads as in overage,
whatever its heads' overage headers say, so none of the three policies applies to it. It is no destination of the
overage hand-back or of the return to plan, as an API-key account is not. Its spend cap holds it out instead: with its
spend share at one or more the seat leaves the eligible set for every model, a live pin's renewal included, until the
spend window's reset, which is the reset a head's `anthropic-ratelimit-unified-overage-reset` names while that is ahead,
else 00:00 UTC on the first of the next calendar month. The share expires at the reset it was read under, a head's reset
where the router holds one observed at or before the reading and that reset is still ahead of the reading, and the
estimate over the reading's instant otherwise, so once that reset passes with no newer poll or head the hold lifts, and
the window is drawn with its amounts and no meter until the next reading. The account view reads the same lapse from the
router wherever the router holds the reading. The fast lane's credits read follows the same expiry: past the reset
`org_spend_cap_reached` refuses none of the seat's fast requests, while the poll's own reason and a disabled flag still
do.

Under `allow`, a subscription account whose five-hour window, seven-day window or model week is spent stays a candidate
on the overage path for every model, since extra usage funds every model: the router sends the request, and a `429`
cools the account. For a model with a week of its own, whose narrowing reads those windows, the account orders behind
plan-funded siblings and drops only while the poll reads its extra usage as blocked (disabled, a reason named, or the
spend cap reached). A credits state the poll has not read yet drops nothing. A credits reading with room, taken after
the `429`, lifts the account's spent-window cooldowns (the account-wide rate limits that name a plan window and the
model-scoped week rows) as it lifts the fast lane's credits refusal. Otherwise the rows end on their own at the window's
reset, at `Retry-After` or after the fallback. Under `prefer_plan` and `never`, a model with a week of its own passes
the account over until its spent window resets.

An account is in overage when its latest quota evidence says so, and only newer evidence, by the upstream's own clock,
takes its place. A response head sets the flag when `anthropic-ratelimit-unified-overage-in-use` is true, and clears it
when the header is present and false, or absent while the shared window headers are present. The poll's snapshot sets
the flag when `extra_usage.is_enabled` is true and the 5-hour or 7-day utilization has reached 1.0, and clears it when
both shared windows are below 1.0. An account with extra usage enabled keeps serving with the flag set once its plan
quota empties, so `prefer_plan` and `never` act through this flag rather than through a cooldown. Once the
organization's spend cap is reached it answers `429` with the overage window rejected and the claim
`seven_day_overage_included`. Both shared windows still read healthy, so for a model with a week of its own (the 7-day
Fable) the oauth table's model-scoped rows, which stand ahead of the shared row, take that rejection and cool that model
alone, and for any other model it cools the account.

Whether the flag still counts is one rule every reader applies alike, the selector and the keepalive scheduler included:
an account counts as in overage while the reset its evidence names is ahead of the present instant. Evidence that names
none stands for the cooldown fallback (`routing.cooldown_fallback_seconds`) past its observation, and is then re-read
against the same rule.

The poll's `extra_usage` state also decides fast eligibility: an account whose credits read disabled, name a reason or
stand at their limit serves no fast request until a later reading shows room. The newest reading per account is kept in
the registry beside its windows and loaded at startup, so a restart judges fast requests on the credits the last poll
read. It goes with the windows when the account is deleted, and an account created again under the same id starts with
none. A request sent fast draws usage credits and no plan window, so neither `prefer_plan` nor `never` holds it off an
account in overage.

## Subscription end

An operator can mark an OAuth account canceled and save the end it stops serving at. The account form opens the end
empty for a running plan, and the operator types the date the provider's billing page shows once the plan is canceled.
A trial end still ahead counts as an end the same way, even on an account nobody has canceled.

Inside `wind_down_minutes` (default 30) of its end, an account is no destination for any move, leaves the set a fresh
placement keeps for its fit, takes a fresh session only as the last resort, and a cold pin moves off it once another
account can serve ([Moves](#moves)). A warm session stays (the **Drain** rule of [Moves](#moves)). With
`wind_down_minutes` at 0 nothing happens before the end, apart from the keepalive, which holds a probe whose interval
passes the end ([Keepalive](keepalive.md)). Past the end, a canceled account is the obstacle `subscription_ended`, and
the upstream refuses the dead credential, so the session's next request fails over to a sibling.

An end that falls before a window's horizon becomes that horizon ([The windows the router works
from](#the-windows-the-router-works-from)). Such a window sheds only once the account's projection at the end passes the
wall, 1.0, in place of `window_ceiling`, since the quota left at the end is lost, and a standing mark on it heals once
the projection there reads under the wall ([Moves](#moves)). A move's fit and a placement keep reading the ceiling.
