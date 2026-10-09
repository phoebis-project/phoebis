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

Shortly before the retained request's prompt cache would expire, the router sends a probe built from it through the same
failover loop a real request takes: the same body with `max_tokens` set to `0`, `stream` set to `false` and the client's
`safeguards` member removed (the request for a server-side auto mode check on the turn's tool use, which a replayed turn
has no use for, whatever the pool's `auto_mode_server` says); the request's `anthropic-beta` line travels as kept, and a
pool or key set off drops the `dangerous-tool-use-2026-09-03` beta from a probe as from any request. The probe follows
the session's pin and renews it. A probe whose pinned member is at its spend budget, on an account the operator
disabled, past its subscription's end, or already tried by this probe while a sibling could take it is answered rather
than placed on a sibling, so a probe never moves the pin for those standings, and the agent reads `budget`, `paused` (a
disabled account or an ended subscription) or `retrying` until the next real request lands. A probe whose pinned account
was deleted is held as `paused` before it is sent, as one on a disabled account is. A 429 on the pinned account, a
credits block, overage under `never`, an account that is disconnected or unpaired, and the pool's move rules move it as
they move a real request.

A probe costs a cache read at list price. The ledger credits the gross saving a later kept-warm request earns
against that cost, per pool and per hour, and `GET /admin/keepalive/overview` reports the net of the two, which
can go negative when probes outspend what they save. The credit goes to the probe whose entry the request read, a
probe that rewrote the entry included, in that probe's unit.

A probe of a request sent fast runs fast too, since the replay keeps the body as sent: it draws usage credits at the
fast rates, settles in dollars whatever the account's funding, and counts toward the dollar cap and no share cap; a
probe the upstream served at standard speed is priced at the standard cells. A request it kept warm saves the input cell
less the cache-read cell of the speed its own answer was served at, the fast cells when it was served fast and the
standard cells when the upstream answered it at standard speed. The upstream shares no cached prefix across speeds, so a
probe keeps a request of its own speed warm alone: a session that switches between fast mode and standard requests earns
no credit from the other speed's probe.

A pool's cache TTL policy applies to a probe the way it applies to the request it replays, because the probe goes
through the same edit path: a marker-less recipe body takes the top-level member only where the policy's
`inject_when_absent` is on. The engine learns the TTL an entry was written with from the response's usage and
from what the router sent, never from the marker the client wrote. The first probe after a turn follows that
turn's cache TTL; the probe itself writes with the TTL its agent's branch holds in the rule set, which the next probe
then follows.

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

The companion holds a lease over the session, 90 seconds long. Once the lease lapses, the session keeps being probed
under the pool's idle limits until they run out. A `session_end` event, a replacement registration, the deletion of the
key that registered it, the session's own expiry, or an operator's release discards its recipes at once; a deleted key's
sessions end within a second of the deletion rather than at their expiry.

The Sessions view names the companion's state two ways for a session it is watching: reporting, meaning "a
cache-keepalive companion is registered for this session and its agents are reporting their lifecycle," and
silent, meaning the companion stopped reporting at the time shown, so the retained prompts stay and are probed
under the pool's idle limits, counted from the last real request, until the companion registers again or the
limits run out. A session the router saw but no companion ever reported for shows none: the router still pins it
and still sees its requests.

The bootstrap file's `keepalive` block holds the process-wide record and byte bounds alone and is read once at start;
changing it takes a restart. The lease and the four probe workers are fixed. See
[the bootstrap file](installation.md#the-bootstrap-file).

## Turning it on

Enabling `keepalive.enabled` on a pool needs `affinity_ttl_seconds` of at least 300 seconds, refused otherwise with
"keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter
than the shortest cache TTL cannot keep the session on the account it warmed." A probe follows the session's pin to a
subscription account as to an API-key one. A key with `keepalive_opt_out` is never retained.

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

The kept-warm credit follows the same rule: only a request that re-armed the recipe earns it, and it goes to the
probe whose entry that request read, a probe that rewrote the entry included, in that probe's unit. An auxiliary or a
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
| `max_idle_ticks` | `0` | 0 to 1000000 | The same limit counted in probes served or failed, instead of seconds. |
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
even written, and a lead plus its jitter must stay under that TTL, refused as `keepalive.lead_seconds_5m plus
keepalive.jitter_seconds_5m must stay under the cache TTL they apply to` (the 1h pair the same). A lead of 0 with a
jitter of 0 never probes: the due time falls on the expiry, and the entry is dead at its expiry. `GET /admin/pools`
answers the full policy with every default filled in, so a pool created with `{}` shows every value above.

Both daily caps count per UTC day and are kept in `router.sqlite` as each probe settles, so a restart resumes them
where they stood. A probe settled in dollars counts toward its pool's `probe_cost_cap_usd_per_day`: one on an api_key
account or a subscription account in overage, a plan-funded one the cost model cannot price (see [The cost
reference](#the-cost-reference)), and a fast-mode session's whatever the account's funding. A probe settled in the
window share counts toward its account's `probe_cap_window_share_per_day`. A migrated probe is priced from its usage
like any other and counts against the daily caps.

## The cost reference

The Cost reference dialog shows what a million tokens costs, in dollars at list price and as a share of a subscription
window, on three tabs. The Sessions view's head and the pool dialog's Warm sessions hint open it on Plan rates, the pool
dialog's Write cost and Daily probe cost cap hints on API prices, and the subtitle of the Dashboard's 5-hour ring and
the note of the Analytics Burn view's Plan per hour chart on Tiers and peak band. It reads the two tables in force: the
shipped price table (`crates/ledger/api_prices.yaml`), or the file `stats.api_prices_path` names (default
`./api_prices.yaml`) when that file exists, and the subscription cost model (`crates/ledger/subscription_costs.yaml`),
or the file `stats.subscription_costs_path` names (see [Statistics](statistics.md)).

**Tiers and peak band.** Windows by tier draws a row per tier the cost model sizes, its name over the tier key the
profile reports, with the 5-hour and the 7-day window each as a bar labeled as a multiple of the smallest tier's window
(Pro's in the shipped model), and the week counted in full 5-hour windows. A Peak band switch, outside the band until
turned, redraws the bars and the count for a request completing inside the band, where each window holds its capacity
factor of its capacity. Peak band draws a week, Monday to Sunday in the browser's time zone: this week, or next week
when the start of today's band (the next band day's on a day the band is off) falls in it. It draws the band's hours on
each day it covers and a marker at now when that week holds it, under a caption that names this week's hours here and
in the zone the cost model defines them in. Beside the grid it says whether the band runs now and until when, names the
next change of the local hours that opens within ten weeks, from its first day to the last before the hours return, or
as lasting when they have not returned 26 weeks on, and draws each window's capacity factor. A zone the browser's time
zone data lacks leaves the band's state unknown: the card gives the band's hours in that zone alone, says the browser
cannot read it, and draws no grid and no change.

**Plan rates.** The share of a tier's 5-hour and 7-day window that a token count of each type spends, per model, for a
request completing outside or inside the band. The tab opens on the cost model's anchor tier, Max 20x, outside the band,
at one million tokens; with a zone the browser cannot read it opens inside the band and asks the reader to pick the
state. A tier chooser, the band control, and a token field in millions with quick picks for 100k, 1M and 10M redraw the
table and its caption. A weight column draws each model's weight on the 5-hour window against the lightest weighted
model's, and reads "by window" for a model whose shared weight is `null` and that an override weighs on some windows,
its title naming each window's weight. A Fable model with a weight on the Fable week carries a Fable week row under its
7-day group, priced on that week, half the 7-day window. On the plan a cache read costs 0.025 of an input token in the
shipped model, and a 1-hour write spends the same share as a 5-minute one. A cell past a full window takes a warning
tint. The models the cost model does not weigh on any window follow a divider, each cell a dash and the weight "not
weighed".

**API prices.** The dollar list prices per million tokens: input, cache read, cache write 5m, cache write 1h and output,
with a fast-mode row under each model that serves fast mode, at its fast cells, what an answer served fast costs.

Both tables list the models family by family, Fable, Opus, Sonnet, Haiku, the newest version first, the order both
shipped files keep, and a file kept in another order lists the same. Plan rates lists the weighted models first and the
others under the divider. The dialog's footer names each table's date and source, `shipped` or `file`, with the key and
the path that set or would replace it in the title.

API prices carries the caption "USD per million tokens at list price. An API-key account pays these. A
subscription account pays them in overage and for fast-mode answers." A recipe's own net follows the same split: the
share of the account's 5-hour window, the band inside each probe's price, on a subscription account outside overage, and
dollars on an api_key account or one in overage; a plan-funded probe the cost model cannot price on the account's tier
(no weight on the 5-hour window, or a tier with no capacity on it), or on an account whose tier the model does not size,
settles in dollars too, counts toward the dollar cap and never toward the window-share cap. A probe goes out for an hour
when the pool's cache TTL policy is on and the rule set the account's funding selects holds `1h` for the probe's agent,
since the relay rewrites every marker of the probe to that TTL (and adds one where `inject_when_absent` is on); with the
policy off, when the recipe keeps an hour's cache or marks any part of its prefix for an hour. A probe settled in
dollars on a model the price table does not list prices at the table's dearest cell of each type, never at zero, so the
dollar cap and the net still bound it; a request such a probe kept warm saves at the same cells, so its net stays
positive while the cache saves more than the probes cost. Whatever the account's funding, a fast-mode session's net is a
dollar net, since its probes draw usage credits. Each agent line of the Sessions view shows its net, as "N% of the
5-hour window", in dollars, or both for an agent whose account changed funding while it was warmed.

The eight shipped rows, all prices in USD per million tokens, read from `crates/ledger/api_prices.yaml` in its order
(the three Opus models' fast cells are twice each figure):

| Model | Input | Cache read | Cache write 5m | Cache write 1h | Output |
| --- | --- | --- | --- | --- | --- |
| `claude-fable-5-1` | 10.00 | 0.25 | 12.50 | 20.00 | 50.00 |
| `claude-fable-5` | 10.00 | 0.25 | 12.50 | 20.00 | 50.00 |
| `claude-opus-5-5` | 4.00 | 0.20 | 5.00 | 8.00 | 20.00 |
| `claude-opus-5` | 5.00 | 0.50 | 6.25 | 10.00 | 25.00 |
| `claude-opus-4-8` | 5.00 | 0.50 | 6.25 | 10.00 | 25.00 |
| `claude-sonnet-5-5` | 2.00 | 0.20 | 2.50 | 4.00 | 10.00 |
| `claude-sonnet-5` | 2.00 | 0.20 | 2.50 | 4.00 | 10.00 |
| `claude-haiku-4-5` | 1.00 | 0.10 | 1.25 | 2.00 | 5.00 |

`GET /admin/sessions/costs` adds a sixth figure per model, `reads_per_write_5m`, the 5-minute cache write price
over the cache read price: 50 for the two Fable models, 25 for `claude-opus-5-5` and 12.5 for the other five.

## Recipe states

A recipe is `armed`, its probe scheduled or arming from the first stream event of the request in flight;
`probing`, with a probe in flight; or `held`, with no probe scheduled and one of twenty reasons attached.

### The guard field

Every request the recipe observes also updates `last_guard`, `null` before the first one: `rearm` or `restart`
for a request that continued or restarted the conversation, `auxiliary` or `foreign` for one the guard set aside
(see [Which requests re-arm a recipe](#which-requests-re-arm-a-recipe)). `last_guard` never holds the recipe by
itself; an auxiliary or a foreign request leaves `state` and the reason below exactly as they were, so a recipe
`held` under `net_negative` with `last_guard: rearm` is a real request that re-armed it, followed later by a
probe that made its net negative, not a contradiction.

| Reason | Console label | What it means |
| --- | --- | --- |
| `affinity_too_short` | affinity too short | The pool's affinity ends before this recipe's probe is due (see below). |
| `awaiting_cache_evidence` | awaiting cache evidence | The request's stream has not yet reported cache usage. |
| `budget` | at budget | The pinned member is at its spend budget until the period ends or the budget is raised. |
| `cap_reached` | cap reached | The pool spent its daily probe cap, or a subscription account its 5-hour window share. |
| `cold_probes` | paused: cold probes | Repeated cache-miss probes passed the pool's cold probe limit. |
| `cooled` | cooled down | The pinned account is under a cooldown; the probe waits it out while the entry lasts. |
| `evicted` | evicted | The engine reached its retained-bytes cap and this recipe was the oldest idle one. |
| `expired` | entry expired | The cache entry expired before its probe was due; no probe was sent. |
| `idle_limit` | idle limit | The pool's idle time or idle probe budget ran out since the last real request. |
| `ledger_behind` | ledger behind | The router's ledger was behind; the probe is sent again after the answer's wait. |
| `net_negative` | net negative | Probes cost more than they saved; a real request grants the grace probes again. |
| `overage` | overage | The account is on extra usage and the pool policy skips probes there. |
| `paused` | paused | The account is paused or its subscription has ended, the pool is disabled, or the key opted out. |
| `pool_exhausted` | pool exhausted | Every account of the pool was exhausted or cooled when the probe was selected. |
| `probe_error` | probe error | The last probe failed on the transport or was refused. |
| `quota_exhausted` | quota exhausted | The account's usage window is spent; probes resume when it resets. |
| `reauth` | needs re-auth | The account needs re-pairing or funding before anything can be sent on it. |
| `retrying` | retrying | The last probe failed with a retryable error; the next attempt has backoff. |
| `storage_limit` | storage limit | The engine is at its retained-request or session limit. |
| `unsupported_shape` | unsupported shape | The last request had no cacheable prefix the probe could replay. |

A recipe holds under `affinity_too_short` while the pool's affinity ends before its probe is due, the entry's TTL less
the lead: the session could move away from the account the probe would warm, so raise `affinity_ttl_seconds` past
that. A five-minute recipe needs about four minutes of pin and a one-hour recipe about fifty-four; the 300-second floor
that `keepalive.enabled` asks of the pool when it is saved stays the one refusal.

An entry the engine knows has expired is never probed: its recipe ends under `expired`, its unanswered probes are
settled wasted, and the engine logs one line naming the key, the agent, the expiry and how late the tick was. A recipe
held under any other reason, a gate's hold among them (`affinity_too_short`, `budget`, `cap_reached`, `cooled`,
`net_negative`, `overage`, `paused` or `reauth`), leaves as `expired` once its entry has expired; the next real request
starts a new one. While the request a recipe follows is still open, the lapse waits for its completion. At the
retained-bytes cap the engine evicts before it refuses: unattended sessions' agents first, oldest activity first, then
attended sessions' idle agents, never one with a probe or a request in flight. A lapsed or evicted recipe leaves the
recipe list, as one stopped under `idle_limit` or `storage_limit` does, and shows on the wasted counter; a lapse also
shows in that log line.

A request whose `system`, `text`, `stream` or `max_tokens` member the replay's edits cannot reach, such as one whose key
is spelled with a JSON escape, is not retained and holds the recipe under `unsupported_shape`: a probe built from it
would travel with a stale stamp or a doubled member.

A probe answered `429 accounts_at_cap`, every account of its pool at the in-flight cap, is `retrying` rather than paused
under `quota_exhausted`: its next attempt waits out that answer's `Retry-After` in place of the backoff, since a slot
frees within seconds. A probe answered `503 accounts_unavailable` because its key moved to another pool while it was in
flight is `retrying` rather than paused under `pool_exhausted`: its next attempt admits under the new pool.

A probe the router refused with `503 ledger_behind`, its ledger catching up, holds under `ledger_behind` rather than
`retrying`: its next attempt waits out that answer's `Retry-After`, or the pool's `retry_seconds` without one, and
counts no failure toward `cold_probe_limit`, so a ledger that stays behind never pauses the recipe. A held re-send
counts no tick toward `max_idle_ticks`. The entry's expiry and `max_idle_seconds` still bound the hold. Past the expiry
the recipe ends under `expired`, as under any other hold.

A wasted probe settles on its own with one of seven reasons: when the recipe stops with a probe unanswered, when it
lapses or is evicted, or when the next request after the probe reads nothing from the entry it kept:

| Reason | What settles it |
| --- | --- |
| `cold_return` | The next request read no cache, or on another account, model or speed, inside the probe's entry. |
| `evicted` | The engine evicted the recipe at its retained-bytes cap. |
| `expired` | The probe's entry expired before the next request arrived, or the recipe's before its probe was due. |
| `idle_limit` | The pool's idle limits ran out before a real request answered the probe. |
| `released` | By an operator's release, a key's opt-out, or the companion reporting the agent released. |
| `replaced` | A replacement registration arrived for the same agent. |
| `session_ended` | By an explicit `session_end` report or the session's own expiry with no report. |

The analytics view puts it plainly: "a high idle-limit share means the pool's max idle time is set too high for
how these sessions are used."

## What the ledger records

Per pool and per hour, the ledger's rollup carries the keepalive fields the console reads: `kept_warm_requests` and
`kept_warm_tokens`, the requests and tokens a probe kept off a cache miss on any model the request named;
`saved_usd_gross`, what those kept-warm tokens saved against paying them as uncached input every turn; `probe_cost_usd`,
what sending the probes cost; both at the dearest listed cells for a model the price table does not know;
`probe_migrated`, a probe that landed on a different account than the recipe's; `wasted_probes` and `wasted_probe_usd`,
probes settled wasted by one of the reasons above, and what they cost; `wasted_probe_reasons`, those wasted probes
counted by the reason; and `probes_skipped`, a due probe a gate or a lapsed entry withheld rather than sending, for
example because the cost cap or a cooldown already held the recipe. `probes_held` counts a probe the router refused with
`503 ledger_behind` while its ledger caught up and sent again, outside the failed probes since no account answered it.

`GET /admin/keepalive/overview` sums a week of these into `net_saved_7d_usd` (gross saved minus probe cost),
`probes_24h` (`ok`, `failed`, `held`, `migrated`, `skipped`), and `cost_today` per pool against its daily cap. Its
`bus_gaps` counts the times the engine found it had missed bus events; each gap discarded every observation still
open, so those agents wait for their next request while an armed recipe keeps its schedule.

## In the console

![The Sessions view: summary tiles, the cost reference link, and the session table with its Columns control and a forked
session's fork badge](assets/sessions-light.png#only-light)
![The Sessions view: summary tiles, the cost reference link, and the session table with its Columns control and a forked
session's fork badge](assets/sessions-dark.png#only-dark)

The Sessions view opens with tiles for the session count, the cache-warming agents by state, cache probes in the last 24
hours by outcome, sessions whose cache expired between turns, the keepalive engine's bus gaps, net savings over 7 days,
and the day's probe cost in total and per pool against its cap; a tile whose read failed draws a dash with the reason on
hover, and Try again under the tiles reads them again, as a line above the table does for a failed account read. Its
table lists one row per session, the session id first and the row's menu at the row's end, and offers ten more
columns through the Columns control, seven shown by default (last request, agents and next probe start
hidden): key, model, account, last used, requests, last request, agents, companion, cache warming, and
next probe. The session column cuts a long id with an ellipsis and shows the whole id on hover. The model column reads
the family and version, with the whole id on hover, and the account cell names the account's kind on hover and to a
screen reader. Opening a row expands into the recipe each of its agents carries. A session's menu offers "View in
analytics" and "View in observe" once it has ledgered traffic, "View in routing log" whenever the row has a
session, which opens the log on that session's decisions, and Release. The analytics view opens on the requests of the
key holder the row belongs to. The view reads its sessions again every 15 seconds, so a session that keeps sending
requests stays on it, and its tiles once a minute.

A session whose first request branched from another conversation carries a fork badge under its id,
`fork of 4ef130f1-90e0… at 3, on another account` when the session it branched from is listed on a different account
and `same account` when it is listed on the row's own, 3 being the depth where the two part, with the base's account on
hover and read out to a screen reader. Following the badge scrolls to the row of the session it branched from and marks
that row. Where that session is not listed the badge names it alone, as `fork of 4ef130f1-90e0…`.

![The Analytics view's Cache warming tab: probes by outcome, savings, wasted probes, and kept-warm
requests](assets/analytics-cache-warming-light.png#only-light)
![The Analytics view's Cache warming tab: probes by outcome, savings, wasted probes, and kept-warm
requests](assets/analytics-cache-warming-dark.png#only-dark)

The Analytics view's Cache warming section draws probes by outcome, savings (the gross saved and the probe cost
per bucket, with their difference as the line), wasted probes by reason, kept-warm requests and tokens, and cache
warming by pool. Prometheus carries `phoebis_keepalive_agents`, `phoebis_keepalive_probes_total`, and
`phoebis_keepalive_saved_usd_total`; see [Statistics](statistics.md).
