# Keepalive

A prompt cache entry expires five minutes, or one hour, after its last use. With keepalive enabled on a pool, the
router keeps a replayable copy of every eligible request a session sends and, shortly before the entry would
expire, sends a probe that reads it. This page covers how the engine learns which sessions are alive, when a probe
may run, every tuning field, and how the ledger accounts what a probe cost against what it saved.

## How a probe works

With keepalive enabled on a pool, the router keeps a replayable copy of every eligible request a session sends: a
request whose thinking is adaptive or disabled, whose tool choice is not forced, and whose output is neither
structured nor bound to a strict tool. A request that fails one of those checks is never retained; the Sessions
view shows it as `unsupported shape`, or as `storage limit` when the body or a header block ran over its size
limit.

Shortly before the retained request's prompt cache would expire, the router sends a probe built from it: the same
body with `max_tokens` set to `0` and `stream` set to `false`, through the same failover loop a real request
takes. The probe follows the session's pin, renews it, and migrates to another account on a 429. A probe whose pinned
member is at its spend budget is not sent, and one that reaches the selector anyway is answered with the budget's
deadline rather than placed on a sibling, so a probe never moves a session's pin on that account.

A probe costs a cache read at list price. The ledger credits the gross saving a later kept-warm request earns
against that cost, per pool and per hour, and `GET /admin/keepalive/overview` reports the net of the two, which
can go negative when probes outspend what they save.

A pool's cache TTL policy applies to a probe the way it applies to the request it replays, because the probe goes
through the same edit path: a marker-less recipe body takes the top-level member only where the policy's
`inject_when_absent` is on. The engine learns the TTL an entry was written with from the response's usage and
from what the router sent, never from the marker the client wrote. The first probe after a turn follows that
turn's cache TTL; the probe itself writes with the rule set's keepalive TTL, which the next probe then follows.

Cache diagnostics apply to a probe the same way: with the option on for the pool, a probe carries the
cache-diagnosis beta and a `diagnostics.previous_message_id` naming the session's last real answer on that
account, the turn the probe keeps warm, so the upstream's report says whether that entry was still there. A
probe's own answer is never remembered as a previous turn: the next probe and the next real request still name
the last real answer. Its cache-miss reason lands in the request log like any other request's. A probe's 400
naming an injected item strips it and retries the probe but never latches the account, since only a real
request's diagnostics latch; the next real request re-sends the item, takes its own 400, and latches.

## The companion

The engine learns which sessions are alive from `cc-cache-keepalive`, a Claude Code hook plugin that reports the
session lifecycle to two routes on the proxy listener, `GET /v0/resource/plugins/cache-keepalive/events` and
`GET /v0/resource/plugins/cache-keepalive/snapshot`, under the same key the client sends for model traffic. A
session is scoped to the key that reports it, and to the generation that key was created under: an id deleted
and created again reads and drives none of the previous holder's sessions.

The companion holds a lease over the session, `keepalive.lease_seconds` long (90 seconds by default). Once the
lease lapses, the session keeps being probed under the pool's idle limits until they run out. A `session_end`
event, a replacement registration, the deletion of the key that registered it, the session's own expiry, or an
operator's release discards its recipes at once; a deleted key's sessions end within a second of the deletion
rather than at their expiry.

The Sessions view names the companion's state two ways for a session it is watching: reporting, meaning "a
cache-keepalive companion is registered for this session and its agents are reporting their lifecycle," and
silent, meaning the companion stopped reporting at the time shown, so the retained prompts stay and are probed
under the pool's idle limits, counted from the last real request, until the companion registers again or the
limits run out. A session the router saw but no companion ever reported for shows none: the router still pins it
and still sees its requests.

The bootstrap file's `keepalive` block holds the process-wide limits alone (the lease, the record and byte
bounds, the worker cap) and is read once at start; changing it takes a restart. See
[the bootstrap file](installation.md#the-bootstrap-file).

## Turning it on

Enabling `keepalive.enabled` on a pool needs `affinity_ttl_seconds` of at least 300 seconds, refused otherwise
with "keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin
shorter than the shortest cache TTL cannot keep the session on the account it warmed." Probes on `oauth` accounts
stay off until `keepalive.oauth_prewarm_verified` is set in the bootstrap file, the operator's statement that the
expiry-crossing keepalive experiment (see [Operations](operations.md)) passed for the combinations in use. A key
with `keepalive_opt_out` is never retained.

In the console: Pools, Edit, the Keepalive group under Caching for "Keep prompt caches warm" and the daily probe cost
cap, and the Probe tuning group beside it.

## Which requests re-arm a recipe

A recipe's cache stays warm only for a request that continues the conversation it was built from. The engine
reads the request's class first: `main`, `subagent`, and `compaction` re-arm the recipe; any other class, a
title or a summary request among them, leaves the recipe, its schedule, and its credit untouched, and shows as
`last_guard: auxiliary`.

Without a class, the request's own two boundary-hash chains decide. A boundary that holds the recipe's last
message hash re-arms it, since the request continues the same conversation. The same static prefix, the tools
array and the system blocks, on the same model, with no continuation restarts the recipe, replacing its cached
body the way a compaction does. A request matching neither the chain nor the prefix shows as `last_guard: foreign`,
and its recipe is left alone the same way an auxiliary one is. A body carrying no chain at all re-arms on every
request, the fallback the router's own warmth check applies for a client that sends none.

The kept-warm credit follows the same rule: only a request that re-armed the recipe earns it. An auxiliary or a
foreign request may still read a warm cache, but it never resets the recipe's schedule and settles nothing.

## Tuning fields

The pool's `keepalive` object carries fifteen fields, each with its own default and bound:

| Field | Default | Accepts | What it does |
| --- | --- | --- | --- |
| `cold_probe_limit` | `2` | 1 to 20 | Cache-miss probes before the recipe pauses until a real request arrives. |
| `enabled` | `false` | | Turns keepalive on for the pool. |
| `jitter_seconds_1h` | `30` | 0 to 3599 | Random seconds trimmed off a 1h probe's due time. |
| `jitter_seconds_5m` | `5` | 0 to 299 | Random seconds trimmed off a 5m probe's due time. |
| `lead_seconds_1h` | `360` | 0 to 3599 | How long before a 1h entry expires its probe is due. |
| `lead_seconds_5m` | `60` | 0 to 299 | How long before a 5m entry expires its probe is due. |
| `max_idle_seconds` | `3600` | 0 to 31536000 | Stops probing the main agent while reporting, every agent once silent. |
| `max_idle_ticks` | `0` | 0 to 1000000 | The same limit counted in probes instead of seconds. |
| `net_negative_grace_probes` | `2` | 0 to 100 | Probes still granted once the net turns negative. |
| `probe_cap_window_share_per_day` | unset | unset or 0 to 1 | A subscription account's daily cap, a window share. |
| `probe_cost_cap_usd_per_day` | unset | unset or a positive number | Daily probe spend cap per pool. |
| `retry_max_seconds` | `300` | 1 to 604800 (a week), at least `retry_seconds` | Ceiling the retry backoff doubles to. |
| `retry_seconds` | `30` | 1 to 3600 | Base backoff after a failed probe, doubling each retry. |
| `stop_when_net_negative` | `true` | | Pauses the recipe once probes cost more than they saved. |
| `warm_subagents` | `true` | | Off retains only the main conversation's recipe; a subagent is never probed. |

An out-of-range value is refused with `keepalive.{field} must be between {min} and {max}, not {value}`, except
for three fields with their own message: `probe_cap_window_share_per_day` answers
"keepalive.probe_cap_window_share_per_day must be null or between 0 and 1," `probe_cost_cap_usd_per_day` answers
"keepalive.probe_cost_cap_usd_per_day must be null or a positive number," and `retry_max_seconds` answers
"keepalive.retry_max_seconds must be at least keepalive.retry_seconds." The lead and jitter bounds stop short of
their cache TTL because a lead or jitter at or past the TTL would put a probe's due time before the cache was
even written. `GET /admin/pools` answers the full policy with every default filled in, so a pool created with
`{}` shows every value above.

Both daily caps count per UTC day and are kept in `router.sqlite` as each probe settles, so a restart resumes them
where they stood. A probe settled in dollars, on an api_key account, a subscription account in overage, or a model
the plan table lacks, counts toward its pool's `probe_cost_cap_usd_per_day`; a plan-funded probe counts toward its
account's `probe_cap_window_share_per_day`, and a migrated probe toward neither.

## The cost reference

The Sessions view's Cost reference dialog builds its table from the shipped price table (`crates/ledger/pricing.yaml`,
effective 2026-09-08), or the file `stats.pricing_path` names (default `./pricing.yaml`) when that file exists (see
[Statistics](statistics.md)). Per model it lists uncached input, cache write 5m, cache write 1h, cache read, and output,
each in USD per million tokens, plus six derived comparisons explained below the table:

<dl markdown="1">
<dt>Per prefix token</dt>
<dd>Every column is per prefix token, so the comparison leaves out the new tokens each turn writes.</dd>
<dt>Turns to pay off</dt>
<dd>The number of later turns that must read the prefix before writing it into the cache was cheaper than sending
it uncached each turn.</dd>
<dt>Hold warm</dt>
<dd>The cost of keeping one prefix token warm across one idle hour (one write plus the probe reads), as a
multiple of sending that token once as uncached input; 2.75x means holding it warm costs 2.75 times sending it
cold once. A 5-minute entry needs a probe about every 4 minutes to survive an idle hour, 15 in all; a 1-hour
entry needs one.</dd>
<dt>Payoff</dt>
<dd>The idle span after which the larger 1-hour write has been repaid by the probes a 5-minute entry would have
spent by then.</dd>
<dt>Fable 5.1</dt>
<dd>On Fable 5.1, where a cache read costs 0.025x, a 5-minute entry kept warm by probes stays cheaper than a
1-hour entry for any idle span; a session with cache warming on may set `CLAUDE_CODE_PROMPT_CACHE_TTL=5m` on the
client. Without warming, the 1-hour entry wins after a 5-minute pause.</dd>
<dt>Window share</dt>
<dd>Each plan column is the share of a Max 5x subscription's 5-hour or 7-day window that one million tokens of the type
consume, from the cost table the ledger ships. An account on another tier spends that share over its tier's factor from
the tier factor table, so a Pro account spends more of its own window and a Max 20x account less, by the multiples a
group header's tooltip gives. A share marked with an asterisk is not yet measured on its window and carries a stand-in
derived from earlier meter readings. A 1-hour write spends the same share as a 5-minute one, so on plan-funded traffic
the long TTL is free and the pool's plan rule set keeps `1h` for the main conversation and subagents alike; while a
write costs money a 1-hour write is 2x against 1.25x, so the metered rule set's `keepalive` TTL defaults to `5m`.</dd>
</dl>

Beside the prices, two column groups read the cost table the ledger ships (`crates/ledger/cost_table.yaml`): the share
of a Max 5x subscription's 5-hour window and of its 7-day window that one million tokens of each type consume, per
model. An account on another tier spends the share over its tier's factor (`crates/ledger/tier_factors.yaml`: Pro 0.215
and 0.18, Max 20x 4.03 and 2.14 of Max 5x's two windows). A cell no measurement has filled is marked with an asterisk
and carries a stand-in derived from earlier meter readings. A dated model id reads the cells of its undated name; a
model the table has no cells for leaves both groups blank.

The prices carry one caveat: "API list prices. A subscription upstream on plan quota spends the window share columns
instead of money." A recipe's own net follows the same split: meter tokens on a subscription account outside overage,
dollars on an api_key account or one in overage. Each agent line of the Sessions view shows its net, as "N plan tokens",
in dollars, or both for an agent whose upstream changed funding while it was warmed.

The four shipped rows, all prices in USD per million tokens:

| Model | Uncached input | Cache write 5m | Cache write 1h | Cache read | Output |
| --- | --- | --- | --- | --- | --- |
| `claude-fable-5-1` | 10.00 | 12.50 | 20.00 | 0.25 | 50.00 |
| `claude-haiku-4-5-20251001` | 1.00 | 1.25 | 2.00 | 0.10 | 5.00 |
| `claude-opus-5` | 5.00 | 6.25 | 10.00 | 0.50 | 25.00 |
| `claude-sonnet-5` | 2.00 | 2.50 | 4.00 | 0.20 | 10.00 |

`GET /admin/sessions/costs` adds a sixth figure per model, `reads_per_write_5m`, the 5-minute cache write price
over the cache read price: 50 for `claude-fable-5-1`, 12.5 for the other three.

## Recipe states

A recipe is `armed`, its probe scheduled or arming from the first stream event of the request in flight;
`probing`, with a probe in flight; or `held`, with no probe scheduled and one of eighteen reasons attached.

### The guard field

Every request the recipe observes also updates `last_guard`, `null` before the first one: `rearm` or `restart`
for a request that continued or restarted the conversation, `auxiliary` or `foreign` for one the guard set aside
(see [Which requests re-arm a recipe](#which-requests-re-arm-a-recipe)). `last_guard` never holds the recipe by
itself; an auxiliary or a foreign request leaves `state` and the reason below exactly as they were, so a recipe
`held` under `net_negative` with `last_guard: rearm` is a real request that re-armed it, followed later by a
probe that made its net negative, not a contradiction.

| Reason | Console label | What it means |
| --- | --- | --- |
| `affinity_too_short` | affinity too short | The pool's affinity expires before the cache this probe would warm. |
| `awaiting_cache_evidence` | awaiting cache evidence | The request's stream has not yet reported cache usage. |
| `budget` | at budget | The pinned member is at its spend budget until the period ends or the budget is raised. |
| `cap_reached` | cap reached | The pool spent its daily probe cap, or a subscription account its 5-hour window share. |
| `cold_probes` | paused: cold probes | Repeated cache-miss probes passed the pool's cold probe limit. |
| `cooled` | cooled down | The pinned account is under a cooldown; the probe waits for it to end. |
| `idle_limit` | idle limit | The pool's idle time or idle probe budget ran out since the last real request. |
| `net_negative` | net negative | Probes have cost more than they saved; held until a request lands warm. |
| `oauth_unverified` | subscription probes off | Off until the account's expiry-crossing experiment passes. |
| `overage` | overage | The account is on extra usage and the pool policy skips probes there. |
| `paused` | paused | The account is paused, the pool is disabled, or the key opted out. |
| `pool_exhausted` | pool exhausted | Every account of the pool was exhausted or cooled when the probe was selected. |
| `probe_error` | probe error | The last probe failed on the transport or was refused. |
| `quota_exhausted` | quota exhausted | The account's usage window is spent; probes resume when it resets. |
| `reauth` | needs re-auth | The account needs re-pairing or funding before anything can be sent on it. |
| `retrying` | retrying | The last probe failed with a retryable error; the next attempt has backoff. |
| `storage_limit` | storage limit | The engine is at its retained-request or session limit. |
| `unsupported_shape` | unsupported shape | The last request had no cacheable prefix the probe could replay. |

A probe answered `429 accounts_at_cap`, every account of its pool at the in-flight cap, is `retrying` rather than paused
under `quota_exhausted`: its next attempt waits out that answer's `Retry-After` in place of the backoff, since a slot
frees within seconds.

A wasted probe settles with one of four reasons of its own, counted when the recipe stops with one already sent
and unanswered:

| Reason | What settles it |
| --- | --- |
| `idle_limit` | The pool's idle limits ran out before a real request answered the probe. |
| `released` | By an operator's release, a key's opt-out, or the companion reporting the agent released. |
| `replaced` | A replacement registration arrived for the same agent. |
| `session_ended` | By an explicit `session_end` report or the session's own expiry with no report. |

The analytics view puts it plainly: "a high idle-limit share means the pool's max idle time is set too high for
how these sessions are used."

## What the ledger records

Per pool and per hour, the ledger's rollup carries the keepalive fields the console reads: `kept_warm_requests`
and `kept_warm_tokens`, the requests and tokens a probe kept off a cache miss on any model the request named;
`saved_usd_gross`, what those kept-warm tokens saved against paying them as uncached input every turn, zero for
a model the price table does not know; `probe_cost_usd`, what sending the probes cost; `probe_migrated`, a
probe that landed on a different account than the recipe's; `wasted_probes` and `wasted_probe_usd`, probes
still unanswered when the recipe was discarded, and what they cost; `wasted_probe_reasons`, those wasted
probes counted by the reason above; and `probes_skipped`, a due probe a gate withheld rather than sending, for
example because the cost cap or a cooldown already held the recipe.

`GET /admin/keepalive/overview` sums a week of these into `net_saved_7d_usd` (gross saved minus probe cost),
`probes_24h` (`ok`, `failed`, `migrated`, `skipped`), and `cost_today` per pool against its daily cap.

## In the console

![The Sessions view: summary tiles, the cost reference link, and the session table with
a forked session's fork badge](assets/sessions-light.png#only-light)
![The Sessions view: summary tiles, the cost reference link, and the session table with
a forked session's fork badge](assets/sessions-dark.png#only-dark)

The Sessions view opens with tiles for the session count, the cache-warming agents by state, cache probes in the last 24
hours by outcome, sessions whose cache expired between turns, net savings over 7 days, and the day's probe cost per pool
against its cap. Its table lists one row per session (key, session, model, upstream, last used, requests,
last request, agents, companion, cache warming, next probe), and opening a row expands into the recipe each of its
agents carries. A session's menu offers Release, and, once it has ledgered traffic, "View in analytics" and "View in
observe"; the analytics view opens on the requests of the key holder the row belongs to. The view reads its sessions
again every 15 seconds, so a session that keeps sending requests stays on it.

A session whose first request branched from another
conversation carries a `fork at 3` badge beside its id, 3 being the depth where the two part; following the badge
scrolls to the row of the session it branched from and marks that row, and where that session is not listed the badge
names it instead, as `fork of 4ef130f1-90e0…`.

![The Analytics view; the Cache warming section draws probes by outcome, savings, wasted probes, and kept-warm
requests](assets/analytics-light.png#only-light)
![The Analytics view; the Cache warming section draws probes by outcome, savings, wasted probes, and kept-warm
requests](assets/analytics-dark.png#only-dark)

The Analytics view's Cache warming section draws probes by outcome, savings (the gross saved and the probe cost
per bucket, with their difference as the line), wasted probes by reason, kept-warm requests and tokens, and cache
warming by pool. Prometheus carries `phoebis_keepalive_agents`, `phoebis_keepalive_probes_total`, and
`phoebis_keepalive_saved_usd_total`; see [Statistics](statistics.md).
