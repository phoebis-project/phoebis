# Keepalive

A prompt cache entry expires five minutes, or one hour, after its last use. With keepalive enabled on a pool, the
router keeps a replayable copy of every eligible request a session sends and, shortly before the entry would
expire, sends a probe that reads it. This page covers how the engine learns which sessions are alive, when a probe
may run, the policy's four fields and the ceilings they set, and what the ledger records for every probe.

## How a probe works

With keepalive enabled on a pool, the router keeps a replayable copy of every eligible request a session sends: a
request whose thinking is adaptive or disabled, whose tool choice is not forced, and whose output is neither
structured nor bound to a strict tool. A request that fails one of those checks, or whose body or header block runs
over its size limit, is never retained, and the Sessions view reads it `not_retained` with the check it failed as the
cause (see [The five states](#the-five-states)).

Shortly before the retained request's prompt cache would expire, the router sends a probe built from it through the same
failover loop a real request takes: the same body with `max_tokens` set to `0`, `stream` set to `false` and the client's
`safeguards` member removed (the request for a server-side auto mode check on the turn's tool use, which a replayed turn
has no use for, whatever the pool's `auto_mode_server` says); the request's `anthropic-beta` line travels as kept, and a
pool or key set off drops the `dangerous-tool-use-2026-09-03` beta from a probe as from any request. The probe follows
the session's pin and renews it. A probe whose pinned member is at its spend budget, on an account the operator
disabled, past its subscription's end, or already tried by this probe while a sibling could take it is answered rather
than placed on a sibling, so a probe never moves the pin for those standings, and the agent reads `budget`, `paused` (a
disabled account), `ineligible_credential` (an ended subscription) or `retryable_probe_error` until the next real
request lands. A probe whose pinned account was deleted is held as `paused` before it is sent, as one on a disabled
account is. A 429 on the pinned account, a credits block, overage under `never`, an account that is disconnected or
unpaired, and the pool's move rules move it as they move a real request.

A probe costs a cache read at list price, which its own row in the request ledger carries. A later request that read
the entry a probe kept is counted as kept warm, per pool and per hour.

A probe of a request sent fast runs fast too, since the replay keeps the body as sent: it draws usage credits at the
fast rates and settles in dollars whatever the account's funding. A probe the upstream served at standard speed is
priced at the standard cells. The upstream shares no cached prefix across speeds, so a probe keeps a request of its own
speed warm alone: a session that switches between fast mode and standard requests earns no credit from the other speed's
probe.

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

The engine learns which sessions are alive from `cc-cache-keepalive`, a Claude Code plugin that reports the session
lifecycle to one route on the proxy listener, `POST /v0/keepalive/events`, under the same key the client sends for model
traffic. A session is scoped to the key that reports it, and to the generation that key was created under: an id deleted
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
subscription account as to an Anthropic-key one. A key with `keepalive_opt_out` is never retained.

In the console: Pools, Edit, the Keepalive group under Caching for "Keep prompt caches warm" and the policy's three
numbers.

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

## The four fields

The pool's `keepalive` object carries four fields:

| Field | Default | Bounds | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Probing on for the pool's sessions, every member kind. |
| `max_idle_seconds` | `3600` | 0 to 31536000 | The main conversation's idle window, at most the break-even. |
| `max_parked_seconds` | `null` | 60 to 604800 | How long a park on a background command or a subagent is probed. |
| `scheduled_horizon_seconds` | `null` | 60 to 604800 | How far off a wakeup may be for a scheduled park to be probed. |

The break-even of a recipe is the pause length at which its chain of probes costs one re-write of its prefix,
`(write - read) / (cadence * read)`. The read and write factors are those of the recipe's model under its account's
funding. A plan-funded recipe reads the cost model's factors, a model the cost model leaves unweighted reads its row of
the price table, and a model neither table prices reads the price table's tightest row. The cadence is the TTL's: 15
probes an hour on a five-minute entry and 1.11 on a one-hour entry. On the shipped cost model a plan-funded recipe
breaks even after 44.1 hours on a one-hour entry and 3 hours 16 minutes on a five-minute entry.

A `max_idle_seconds` of 0, a `null` ceiling and any number above the break-even resolve to the break-even, so a number
in a field is a cap below it. `GET /admin/pools` answers `null` where null was set. An out-of-range value is refused
with `keepalive.{field} must be between {min} and {max}, not {value}`, for example `keepalive.max_parked_seconds must
be between 60 and 604800, not 59`, and a field the policy does not hold is refused as any unknown field, `keepalive is
not a policy object: unknown field ...`.

The schedule's numbers are fixed. A probe is due 60 seconds before a five-minute entry expires and 360 seconds before
a one-hour entry, less a random jitter of up to 5 and 30 seconds. Two cold probes in a row pause the recipe until a real
request. The third transient failure pauses it, after retries that wait 30 seconds and double up to 300. No limit
counts idle probes, and every subagent's recipe is warmed.

## The ceilings

Three rules bound how long a retained recipe is probed, one for each way a session comes back, and the recipe is probed
while any of them says so. Every park has a certain return: a cron fires, a command exits or reaches its deadline, a
subagent completes. The two park rules are loss bounds. The idle window is the one bet, and it runs in every state of
the main conversation, because the user can type while a background command, a subagent or a cron is open.

**The idle window.** The main conversation is probed until `max_idle_seconds` after the latest of its last real
request, its last presence report and the end of a park without a request, a deadline or a wakeup that passed. The
window never runs past the recipe's break-even. A wakeup is a real request, so a cron that fires or a command that exits
restarts the window. A silent session, whose companion's lease lapsed, is probed under the window from its last real
request for every agent, and the park rules do not apply to it.

**Parked on a background command or a subagent.** The main conversation or a subagent reported `parked` with kind
`background` is probed until the return, at most `max_parked_seconds` after the park began. A real request or a state
other than `parked` ends the park. A command's deadline ends the park when it passes, because the harness stops the
command and wakes nobody, and the idle window restarts at that moment.

**Parked on a scheduled wakeup.** With kind `scheduled`, a due probe goes out while the wakeup is at most
`scheduled_horizon_seconds` away. Beyond that the recipe is held with the reason `horizon`, its probes stopped and its
prompt kept while the entry lasts. A later `parked` report with a nearer wakeup reopens it, as does a horizon raised on
the pool, and probing resumes if the entry is still warm. A held recipe whose entry expires ends as `expired`. A wakeup
that passes ends the park as a deadline does, which bounds a deleted or expired cron and covers the scheduler's jitter
of up to 15 minutes.

A park of kind `both` is probed while either part says so, and a hold or an end applies only once neither does. When
neither does and the wakeup lies beyond the horizon, the recipe is held rather than ended, since the wakeup's return is
certain. A subagent's park follows the two park rules on its own, and the idle window never applies to a subagent, which
is busy, parked or released. A busy agent that stays silent past the lead is probed with no ceiling of its own: a long
foreground tool call on a five-minute entry stays covered, and the lease and the idle window bound a client that died.

At a ceiling the recipe ends with the reason `idle_limit` or `parked_limit`, or is held with `horizon`, and its
unanswered probes count as wasted under that reason. The next real request re-arms it.

### The return rate by idle age

The idle window is a bet whose right size differs by pool, so the router records, per pool, how long the main
conversation stays idle and whether it comes back to the same conversation. A window opens once the main conversation
reports idle or parked, from the idle window's start above, and two counters per idle-age bucket record it:

- `reached`, the windows that lasted to the bucket's start.
- `returned`, the windows whose next request from the main conversation continued the conversation inside the bucket:
  one of the request's message boundaries is the last one of the request before the window opened.

The ten buckets run 0 to 5, 5 to 15, 15 to 30 and 30 to 60 minutes, then 1 to 2, 2 to 4, 4 to 8, 8 to 16 and 16 to 32
hours, and above. Windows are counted whether or not a recipe is retained, so one that outlives the idle window still
records when the user came back, and a pool with keepalive off or a key that opted out records its returns too. A window
that opens before the session's first request has no conversation to compare against and counts no return. A window
counts under the pool its key was in when it opened. A return's age is read at the prompt that announced it, however
long the request takes to follow. A lapsed lease, a park that ends without a request, a presence report, a release of
the main conversation, a request on a new conversation and the session's end close a window with no return. After a
request the guard reads as a return or a restart, the next window starts at the request's own instant, except on a main
conversation reported `unknown`, where the request moves no start. After any other close, the next window opens only
once its start moves past the one in force at that close, at a park's end, a presence report, a prompt, a request or the
session's revival. A busy report pauses the count without closing the window, and a subagent or a silent session opens
none.

No rule reads the counters yet: the idle window is still the pool's `max_idle_seconds`, capped at the break-even. The
pool dialog shows them by bucket.

## A router restart

The engine writes its state to the registry every 30 seconds and at shutdown: the sessions, every agent with its state
and its park, the retained recipes and the idle-age counters. A recipe's prompt is sealed under the storage key, as the
credentials are. A probe in flight is not saved. The shutdown stops taking reports and the events of requests in flight
before its last save, so that save holds the state the engine had when it stopped, and a report that arrives later is
answered `quiesced`. A save that fails is logged once and retried at every flush, and the engine keeps running from
memory.

At start the engine reads that state back before its first tick. Every session returns silent, its lease at zero and
its sequence and its agents' generations kept. The mod's next heartbeat answers `instance_ended`, the mod registers the
same instance again, and the session is re-adopted with its recipes, so its next probe lands warm. A recipe whose entry
expired while the router was down ends with `expired_at_restart`, and a session the mod had ended is dropped. A prompt
that no longer opens under the storage key is dropped too, and its agent keeps everything else. The bootstrap file's
`keepalive` limits in force at the start bound what comes back: when they are lower than before, the newest sessions,
agents and recipes are restored and the rest are evicted.

## The cost reference

The Cost reference dialog shows what a million tokens costs, in dollars at list price and as a share of a subscription
window, on three tabs. The Sessions view's head and the pool dialog's Warm sessions hint open it on Plan rates, the pool
dialog's Write cost hint on API prices, and the subtitle of the Dashboard's 5-hour ring and the note of the Analytics
Burn view's Plan per hour chart on Tiers and peak band. It reads the two tables in force: the shipped price table
(`crates/ledger/api_prices.yaml`), or the file `stats.api_prices_path` names (default `./api_prices.yaml`) when that
file exists, and the subscription cost model (`crates/ledger/subscription_costs.yaml`), or the file
`stats.subscription_costs_path` names (see [Statistics](statistics.md)).

**Tiers and peak band.** Windows by tier draws a row per tier the cost model sizes, its name over the tier key the
profile reports, with the 5-hour and the 7-day window each as a bar labeled as a multiple of the smallest tier's window
(Pro's in the shipped model), and the week counted in full 5-hour windows. A Peak band switch, outside the band until
turned, redraws the bars and the count for a request completing inside the band, where each window holds its capacity
factor of its capacity. Peak band draws this week, Monday to Sunday in the browser's time zone (on a day the band is
off, the week of the next day it covers), the band's hours on each day it covers and a marker at now when that week
holds it, under a caption that names this week's hours here and in the zone the cost model defines them in. Beside the
grid it says whether the band runs now and until when, names the next change of the local hours that opens within ten
weeks, from its first day to the last before the hours return, or as lasting when they have not returned 26 weeks on,
and draws each window's capacity factor. A zone the browser's time zone data lacks leaves the band's state unknown: the
card gives the band's hours in that zone alone, says the browser cannot read it, and draws no grid and no change.

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

API prices carries the caption "USD per million tokens at list price. An Anthropic-key account pays these. A
subscription account pays them in overage and for fast-mode answers." A probe's own row in the request ledger is priced
from it in dollars, whatever the account's funding, and a probe on a model the price table does not list prices at the
table's dearest cell of each type, never at zero. The two tables also set a recipe's break-even, which the pool's
ceilings resolve to (see [The four fields](#the-four-fields)). A probe goes out for an hour when the pool's cache TTL
policy is on and the rule set the account's funding selects holds `1h` for the probe's agent, since the relay rewrites
every marker of the probe to that TTL (and adds one where `inject_when_absent` is on). With the policy off, it goes out
for an hour when the recipe keeps an hour's cache or marks any part of its prefix for an hour.

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

## The five states

The Sessions view reads each agent's recipe as one of five states, with the engine's own reason beside it as the
cause. The cause's sentence shows on hover over the state word and again in the row's expanded detail. Every agent
that has sent a real request keeps its row until its session leaves the engine, so a recipe that ended, or a request
that was not kept, still shows with its cause.

| State | What it means |
| --- | --- |
| `retained` | The prompt is kept and its probe is scheduled, in flight, or waiting for the first stream event. |
| `held` | The prompt is kept and no probe is sent while a condition holds that clears by itself or by an operator. |
| `paused` | The engine paused the recipe after a probe's answer or a failed request, until the next real request. |
| `not_retained` | The last request was not kept, for its shape or size, the engine's limits, or no cache evidence. |
| `ended` | The recipe was discarded and the agent's row stays: a ceiling, a lapse, an eviction, a release or an end. |

Each state reads these causes:

- `retained`: `awaiting_cache_evidence`, `awaiting_selection`, `lease_expired`, `probing`, `scheduled`.
- `held`: `affinity_too_short`, `budget`, `cooled`, `horizon`, `ineligible_credential`, `keepalive_off`,
  `ledger_behind`, `overage`, `paused`, `pool_disabled`, `reauth`, `retryable_probe_error`, `slow_probe`.
- `paused`: `invalid_probe_usage`, `missing_account`, `partial_cache_coverage`, `probe_authentication_error`,
  `probe_pool_exhausted`, `probe_quota_exhausted`, `probe_retry_limit`, `probe_transport_error`,
  `repeated_cold_probe`, `request_canceled`, `request_failed`, `request_rejected`, `unexpected_output`,
  `unsupported_probe`.
- `not_retained`: `awaiting_request`, `fixed_thinking`, `forced_tool`, `headers_too_large`, `invalid_cache_control`,
  `invalid_model`, `invalid_request`, `no_cache_evidence`, `no_probe_cache_evidence`, `owner_mismatch`,
  `request_limit`, `request_too_large`, `storage_limit`, `structured_output`, `unsupported_shape`.
- `ended`: `agent_released`, `agent_unknown`, `bus_gap`, `evicted`, `expired`, `expired_at_restart`, `idle_limit`,
  `instance_replaced`, `key_opted_out`, `opted_out`, `parked_limit`, `released`, `reset`, `session_ended`,
  `session_expired`.

A `held` or `retained` cause reads `paused` while the engine's own pause holds. A probe's error answer pauses the
recipe and clears its schedule, and a gate's hold that lands on it at a later tick (`budget`, `cooled`,
`ineligible_credential` or `paused`) re-arms nothing, so the row stays `paused` with the hold's cause until the next
real request. The same hold on an armed recipe reads `held`, since the gate reads it again at every tick and probing
resumes when it clears. A recipe held on a disabled pool reads `retained` again at the first tick after the pool is
enabled.

### The guard field

Every request the recipe observes also updates `last_guard`, `null` before the first one: `rearm` or `restart`
for a request that continued or restarted the conversation, `auxiliary` or `foreign` for one the guard set aside
(see [Which requests re-arm a recipe](#which-requests-re-arm-a-recipe)). `last_guard` never holds the recipe by
itself. An auxiliary or a foreign request leaves `state` and `cause` exactly as they were, so a recipe `held` under
`cooled` with `last_guard: rearm` is a real request that re-armed it, followed later by a cooldown on its account, not a
contradiction.

A recipe holds under `affinity_too_short` while the pool's affinity ends before its probe is due, the entry's TTL less
the lead: the session could move away from the account the probe would warm, so raise `affinity_ttl_seconds` past
that. A five-minute recipe needs about four minutes of pin and a one-hour recipe about fifty-four; the 300-second floor
that `keepalive.enabled` asks of the pool when it is saved stays the one refusal.

An entry the engine knows has expired is never probed: its recipe ends under `expired`, its unanswered probes are
settled wasted, and the engine logs one line naming the key, the agent, the expiry and how late the tick was. A recipe
held under any other reason, a gate's hold among them (`affinity_too_short`, `budget`, `cooled`, `overage`, `paused` or
`reauth`), leaves as `expired` once its entry has expired, and the next real request starts a new one. A recipe held
under `horizon` or on a disabled pool leaves the same way, and one reopened or enabled again while its entry is warm
probes on. While the request a recipe follows is still open, the lapse waits for its completion. At the retained-bytes
cap the engine evicts before it refuses: unattended sessions' agents first, oldest activity first, then attended
sessions' idle agents, never one with a probe or a request in flight. A lapsed or evicted recipe reads `ended`, as one
stopped under `idle_limit` does, and shows on the wasted counter. A lapse also shows in that log line.

A request whose `system`, `text`, `stream` or `max_tokens` member the replay's edits cannot reach, such as one whose key
is spelled with a JSON escape, is not retained and reads `not_retained` under `invalid_request`: a probe built from it
would travel with a stale stamp or a doubled member.

A probe answered `429 accounts_at_cap`, every account of its pool at the in-flight cap, is held under
`retryable_probe_error` rather than paused under `probe_quota_exhausted`: its next attempt waits out that answer's
`Retry-After` in place of the backoff, since a slot frees within seconds. A probe answered `503 accounts_unavailable`
because its key moved to another pool while it was in flight is held under `retryable_probe_error` rather than paused
under `probe_pool_exhausted`: its next attempt admits under the new pool.

A probe the router refused with `503 ledger_behind`, its ledger catching up, holds under `ledger_behind` rather than
`retryable_probe_error`: its next attempt waits out that answer's `Retry-After`, or the 30-second retry wait without
one, and counts no failure toward the failure limit, so a ledger that stays behind never pauses the recipe. The entry's
expiry still bounds the re-sends: once it passes, the recipe ends under `expired`, as under any other hold.

A wasted probe settles with one of nine reasons of its own: when the recipe stops with a probe unanswered or is held
under `horizon`, when it lapses or is evicted, or when the next request after the probe reads nothing from the entry it
kept:

| Reason | What settles it |
| --- | --- |
| `cold_return` | The next request read no cache, or on another account, model or speed, or after the entry lapsed. |
| `evicted` | The engine evicted the recipe at its retained-bytes cap. |
| `expired` | The recipe's cache entry expired before its probe was due. |
| `horizon` | The recipe was held under `horizon` before a real request answered the probe. |
| `idle_limit` | The idle window ran out before a real request answered the probe. |
| `parked_limit` | The parked ceiling ran out before a real request answered the probe. |
| `released` | By an operator's release, a key's opt-out, or the companion reporting the agent released. |
| `replaced` | A replacement registration arrived for the same agent. |
| `session_ended` | By an explicit `session_end` report or the session's own expiry with no report. |

The analytics view puts it plainly: a high idle-limit share says the pool's idle window outlasts how these sessions are
used, and a high parked-limit or horizon share says the parked ceilings do.

## What the ledger records

Every probe is a row of the request ledger, of kind `probe`, priced as any request is. Per pool and per hour, the
ledger's rollup carries the keepalive fields the console reads. `kept_warm_requests` and `kept_warm_tokens` count the
requests and tokens a probe kept off a cache miss on any model the request named. `probe_cost_usd` is what sending the
probes cost, at the dearest listed cells for a model the price table does not know, and `probe_migrated` counts a probe
that landed on a different account than the recipe's. `wasted_probes` and `wasted_probe_usd` are the probes settled
wasted by one of the reasons above and what they cost, and `wasted_probe_reasons` counts those wasted probes by the
reason. `probes_skipped` counts a due probe a gate or a lapsed entry withheld rather than sending, for example because a
cooldown already held the recipe.

`GET /admin/keepalive/overview` sums the last 24 hours of these into `probes_24h` (`ok`, `failed`, `migrated`,
`skipped`). Its `bus_gaps` counts the times the engine found it had missed bus events; each gap discarded every
observation still open, so those agents wait for their next request while an armed recipe keeps its schedule.

## In the console

![The Sessions view: summary tiles, the cost reference link, and the session table with its Columns control and a forked
session's fork badge](assets/sessions-light.png#only-light)
![The Sessions view: summary tiles, the cost reference link, and the session table with its Columns control and a forked
session's fork badge](assets/sessions-dark.png#only-dark)

The Sessions view opens with tiles for the session count, the cache-warming agents by state, cache probes in the last 24
hours by outcome, sessions whose cache expired between turns, and the keepalive engine's bus gaps. A tile whose read
failed draws a dash with the reason on hover, and Try again under the tiles reads them again, as a line above the table
does for a failed account read. Its table lists one row per session, led by the row's menu, and offers eleven columns,
eight shown by default (last request, agents and next probe start hidden, and the Columns control turns
any on): key, session, model, account, last used, requests, last request, agents, companion, cache
warming, and next probe. The model column reads the family and version, with the whole id on hover, and the account cell
names the account's kind on hover and to a screen reader. The companion column reads `reporting`, `silent` or `none`,
and under `reporting` how long ago the person was last seen at the terminal, as `presence 12s ago`. Opening a row
expands into one line per agent that has sent a real request: its model, pinned account and TTL, the last request, the
last and the next probe, and the probes sent today with the requests they kept warm, counted from midnight UTC. The line
ends on its state word with the cause beside it, the cause's sentence on hover (see
[The five states](#the-five-states)). Under the columns it reads the mod's lifecycle word for the agent, with a parked
agent's park kind and its deadline or wake time (`parked · background · deadline in 1h 12m`), and repeats the state's
and the cause's sentences. A session's menu offers "View in analytics" and "View in observe" once it has ledgered
traffic, "View in routing log", which opens the log on that session's decisions,
and Release. The analytics view opens on the requests of the key holder the row belongs to. The view reads its sessions
again every 15 seconds, so a session that keeps sending requests stays on it, and its tiles once a minute.

A session
whose first request branched from another conversation carries a `fork at 3` badge beside its id, 3 being the depth
where the two part; following the badge scrolls to the row of the session it branched from and marks that row, and where
that session is not listed the badge names it instead, as `fork of 4ef130f1-90e0…`.

![The Analytics view's Cache warming tab: probes by outcome, savings, wasted probes, and kept-warm
requests](assets/analytics-cache-warming-light.png#only-light)
![The Analytics view's Cache warming tab: probes by outcome, savings, wasted probes, and kept-warm
requests](assets/analytics-cache-warming-dark.png#only-dark)

The Analytics view's Cache warming section draws the probe cost and the kept-warm count, probes by outcome, wasted
probes by reason, kept-warm requests and tokens, and cache warming by pool. Prometheus carries
`phoebis_keepalive_agents` and `phoebis_keepalive_probes_total` (see [Statistics](statistics.md)).
