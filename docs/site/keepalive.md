# Keepalive

A prompt cache entry expires five minutes, or one hour, after its last use. With keepalive enabled on a pool, the router
keeps a replayable copy of every eligible request a session sends and, shortly before the entry would expire, sends a
probe that reads it. This page covers how the keepalive mod tells the engine which sessions are alive, when a probe may
run, the policy's four fields and the ceilings they set, the five states a recipe reads, what the ledger records for
every probe, and what a router restart keeps.

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
the entry a probe kept, a probe that rewrote the entry included, is counted as kept warm, per pool and per hour.

A probe of a request sent fast runs fast too, since the replay keeps the body as sent: it draws usage credits at the
fast rates, and its ledger row is priced at the fast cells. A probe the upstream served at standard speed is priced at
the standard cells. The upstream shares no cached prefix across speeds, so a probe keeps a request of its own speed warm
alone: a session that switches between fast mode and standard requests earns no credit from the other speed's probe.

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

## The mod

The engine learns which sessions are alive, which agents they run and what each agent is doing from the keepalive mod,
a Claude Code plugin that runs inside the client's own process. The mod reports facts and holds no policy: every
ceiling and switch is a pool field or a constant of the router. It installs from its marketplace, inside Claude Code:

```text
/plugin marketplace add daystrom-labs/phoebis-marketplace
/plugin install phoebis-keepalive@phoebis
```

The mod needs Claude Code 2.1.287 or later and does nothing on an older client. With its options unset it reports to
`ANTHROPIC_BASE_URL` under the router key in `ANTHROPIC_AUTH_TOKEN`, then `CLAUDE_CODE_OAUTH_TOKEN`, so the
[client block](client.md#for-the-keepalive-mod) needs no line of its own for it, and without a URL or a key it reports
nothing. It behaves the same in the terminal, the Desktop app, the VS Code extension's chat panel, `claude -p` and the
Agent SDK.

The mod reports on the main conversation, whose agent id is `main`, and on each subagent the client lists. An agent's
open set holds the background commands and the subagents it started that have not ended yet, and the session's crons
and wakeups belong to the main conversation:

| Fact | Report |
| --- | --- |
| The session starts | `session_start`, once the owner header below is in place |
| The person sends a prompt | `activity` kind `prompt` for the main conversation |
| The person types at the prompt | `activity` kind `presence` for the main conversation, at most once a minute |
| An agent's model request or tool call starts | `state busy` for that agent when it was not busy |
| A background command starts | none: the command joins its agent's open set with its deadline |
| A cron or a wakeup is scheduled, or a cron is deleted | none: the session's crons change |
| A cron or a wakeup fires | none: it leaves the crons or moves to its next fire |
| A subagent starts | `agent_start` for the subagent, which joins its parent's open set |
| The notification turn that closes a background command or a subagent | none: the entry leaves its open set |
| A subagent's turn ends | `state parked` on its open set, or `state released` |
| The main conversation's turn ends | `state idle`, or `state parked` with the park's kind, wake time and deadline |
| The session ends, or `/clear` or `/resume` gives it a new id | `session_end`, one request |
| The heartbeat timer fires | `heartbeat`, after the session id check and the reconcile against the client's agents |

The [mod's README](https://github.com/daystrom-labs/phoebis-keepalive/blob/main/README.md#what-the-mod-reports) names
the client's own events, prompt origins and notification tags behind each row.

A presence report says the person is at the terminal. It resets the main conversation's idle clock and nothing else,
so no agent turns busy and no state changes (see [The ceilings](#the-ceilings)). The mod sends at most one a minute.

At session start the mod mints two ids for its process, the owner id and the instance id. It sends the owner id as
`owner_id` on `session_start` and writes it as one `X-Keepalive-Owner-Id` line into `ANTHROPIC_CUSTOM_HEADERS`, keeping
the variable's other lines, so the client sends the header on every model request of the process. The router strips
the header before a request leaves and reads it to tell that process's requests from another process's on the same
session id: a request whose header names another owner leaves the session's recipes as they were. Every report names
the instance. A `/clear` or a `/resume` keeps both ids, since the owner is the process, and a child process mints its
own pair and registers as its own instance.

A registration holds a lease over the session, 90 seconds long, which each heartbeat renews. The mod sends its
heartbeat at a third of the remaining lease, between 1 and 25 seconds. The Sessions view names the mod's state for each
session. It reads `reporting` while the mod reports the session's agents, their parks and the person's presence, and
`silent` once the lease lapsed, with the time the mod stopped reporting. A silent session's retained prompts stay and
are probed under the idle window from the last real request, until the mod reports again or the window runs out. A
session the router saw but no mod registered reads `none`: the router still pins it and still sees its requests. A
`session_end` report, a replacement registration, the deletion of the key that registered the session, the session's
own expiry, or an operator's release discards its recipes at once. A deleted key's sessions end within a second of the
deletion rather than at their expiry.

Every report is one `POST /v0/keepalive/events` on the proxy listener, a JSON body under the same key the client sends
for model traffic. A session is scoped to the key that reports it, and to the generation that key was created under: an
id deleted and created again reads and drives none of the previous holder's sessions. Each acknowledgment names the
session, the instance, the sequence and the lease's end, and an agent event's also names the agent and its
`request_generation`. A registration's acknowledgment carries every agent the router holds for the session, so the mod
reconciles from that answer after a lapse or a router restart. A report the route refuses is answered
`{"code": "<code>"}` rather than with an Anthropic-shaped error body, and a key it refuses is answered as `/v1/messages`
answers it. The mod's README describes
[the protocol](https://github.com/daystrom-labs/phoebis-keepalive/blob/main/README.md#the-protocol): the six events,
their fields, the acknowledgment and the codes.

The bootstrap file's `keepalive` block holds the process-wide bounds alone: `max_sessions`, `max_agents` and
`max_retained_bytes`. It is read once at start, so changing it takes a restart, and the Sessions page shows each bound
beside the count it bounds (see [In the console](#in-the-console)). The lease and the four probe workers are fixed. See
[the bootstrap file](installation.md#the-bootstrap-file).

## Turning it on

Enabling `keepalive.enabled` on a pool needs `affinity_ttl_seconds` of at least 300 seconds, refused otherwise with
"keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter
than the shortest cache TTL cannot keep the session on the account it warmed." A probe follows the session's pin to a
subscription account as to an API-key one. A key with `keepalive_opt_out` is never retained.

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

The kept-warm credit follows the same rule: only a request that re-armed the recipe earns it, and it goes to the
probe whose entry that request read, a probe that rewrote the entry included. An auxiliary or a foreign request may
still read a warm cache, but it never resets the recipe's schedule and settles nothing.

## The four fields

The pool's `keepalive` object carries four fields:

| Field | Default | Bounds | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Probing on for the pool's sessions, every member kind. |
| `max_idle_seconds` | `3600` | 0 to 31536000 | The main conversation's idle window, at most the break-even. |
| `max_parked_seconds` | `null` | null or 60 to 604800 | Time a park on a background command or a subagent is probed. |
| `scheduled_horizon_seconds` | `null` | null or 60 to 604800 | The farthest wakeup a scheduled park is probed toward. |

The break-even of a recipe is the pause length at which its chain of probes costs one re-write of its prefix,
`(write - read) / (cadence * read)`. The read and write factors are those of the recipe's model under its account's
funding. A plan-funded recipe reads the cost model's type multipliers, the model's own where it sets them. A token costs
the model's weight times the type's multiplier, so a plan recipe's break-even does not depend on the model's weight, and
a model the cost model leaves unweighted breaks even where a weighted one does. A dollar-funded recipe reads its model's
row of the price table, and a model the price table does not price reads its tightest row. The cadence is the TTL's: 15
probes an hour on a five-minute entry and 1.11 on a one-hour entry. On the shipped cost model a plan-funded recipe
breaks even after 44.1 hours on a one-hour entry and 3 hours 16 minutes on a five-minute entry.

A `max_idle_seconds` of 0, a `null` ceiling and any number above the break-even resolve to the break-even, so a field's
number takes effect only below the break-even. `GET /admin/pools` answers `null` where null was set. An out-of-range
value is refused with `keepalive.{field} must be between {min} and {max}, not {value}`, for example
`keepalive.max_parked_seconds must be between 60 and 604800, not 59`, and a field the policy does not hold is refused as
any unknown field, `keepalive is not a policy object: unknown field ...`.

For the console, `GET /admin/pools` also answers each pool's `keepalive_resolved`: the break-even and the three ceilings
in seconds and the idle window's cost as a share of one re-write, each as the lowest and the highest figure over the
pool's combinations. An OAuth member brings the cost model's factors and an API-key member the price table's. The pool's
cache TTL policy gives the main conversation's TTL under each, and a policy that leaves the markers as sent brings both
TTLs. The models are the price table's and those the cache TTL policy's per-model rows and the model rules' entries
without a wildcard name, which carry the catalog's dated ids, each kept where the pool's model rules allow it. Rules
that allow none of them leave a model priced nowhere. A deny entry naming a dated id leaves the model's undated price
row in the range, so for a model the catalog lists only dated, such as Haiku, the range keeps a figure no request on the
pool reaches. A pool with no member answers `null`.

The schedule's numbers are fixed. A probe is due 60 seconds before a five-minute entry expires and 360 seconds before
a one-hour entry, less a random jitter of up to 5 and 30 seconds. Two cold probes in a row pause the recipe until a real
request. The third transient failure pauses it, after retries that wait 30 seconds and double up to 300. No limit
counts idle probes, and every subagent's recipe is warmed.

## The ceilings

Three rules bound how long a retained recipe is probed, one for each way a session comes back, and the recipe is probed
while any of them says so. Every park has a certain return: a cron fires, a command exits or reaches its deadline, a
subagent completes. The two park rules are loss bounds. The idle window is the one bet, and it runs in every state of
the main conversation, because the user can type while a background command, a subagent or a cron is open.

**The idle window.** The main conversation is probed until `max_idle_seconds` after the latest of its last real request,
its last presence report and the end of a park without a request, a deadline or a wakeup that passed. The window never
runs past the recipe's break-even. A wakeup is a real request, so a cron that fires or a command that exits restarts the
window. A silent session, one whose lease lapsed, is probed under the window from its last real request for every agent,
and the park rules do not apply to it. A session restored at a router restart reads silent but keeps its park rules for
one lease length after the load or until its registration arrives, and the silent rule applies after that.

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
request that continued or restarted the conversation, the next window starts at the request's own instant, except on a
main conversation reported `unknown`, where the request moves no start. After any other close, the next window opens
only once its start moves past the one in force at that close, at a park's end, a presence report, a prompt, a request
or the session's revival. A busy report pauses the count without closing the window, and a subagent or a silent session
opens none.

No rule reads the counters yet: the idle window is still the pool's `max_idle_seconds`, capped at the break-even. The
pool dialog shows them by bucket.

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
  `ledger_behind`, `no_probe_cache_evidence`, `overage`, `paused`, `pool_disabled`, `reauth`,
  `retryable_probe_error`, `slow_probe`.
- `paused`: `invalid_probe_usage`, `missing_account`, `partial_cache_coverage`, `probe_authentication_error`,
  `probe_pool_exhausted`, `probe_quota_exhausted`, `probe_retry_limit`, `probe_transport_error`,
  `repeated_cold_probe`, `request_canceled`, `request_failed`, `request_rejected`, `unexpected_output`,
  `unsupported_probe`.
- `not_retained`: `awaiting_request`, `fixed_thinking`, `forced_tool`, `headers_too_large`, `invalid_cache_control`,
  `invalid_model`, `invalid_request`, `no_cache_evidence`, `owner_mismatch`, `request_limit`, `request_too_large`,
  `storage_limit`, `structured_output`, `unsupported_shape`.
- `ended`: `agent_released`, `agent_unknown`, `bus_gap`, `evicted`, `expired`, `expired_at_restart`, `idle_limit`,
  `instance_replaced`, `key_opted_out`, `opted_out`, `parked_limit`, `released`, `reset`, `session_ended`,
  `session_expired`.

A `held` or `retained` cause reads `paused` while the engine's own pause holds. A probe's error answer pauses the recipe
and clears its schedule, and a gate's hold that lands on it at a later tick (`budget`, `cooled`, `ineligible_credential`
or `paused`) re-arms nothing, so the row stays `paused` with the hold's cause until the next real request. The paused
prompt itself leaves when its entry goes cold: at the entry's expiry the engine drops it, settles its unanswered probes
under `expired` and sends nothing, and the row still reads `paused`. The same hold on an armed recipe reads `held`,
since the gate reads it again at every tick and probing resumes when it clears. A recipe held on a disabled pool reads
`retained` again at the first tick after the pool is enabled. A `held` or `retained` cause reads `not_retained` when the
agent holds no prompt, as a request refused at its account selection does.

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
expiry and the ceilings still bound the re-sends: once the entry expires, the recipe ends under `expired`, as under any
other hold, and a ceiling that runs out first ends it under its own reason.

A wasted probe settles on its own with one of nine reasons: when the recipe stops with a probe unanswered or is held
under `horizon`, when it lapses or is evicted, or when the next request after the probe reads nothing from the entry it
kept:

| Reason | What settles it |
| --- | --- |
| `cold_return` | The next request read no cache, or on another account, model or speed, inside the probe's entry. |
| `evicted` | The engine evicted the recipe at its retained-bytes cap. |
| `expired` | The probe's entry expired before the next request arrived, or the recipe's before its probe was due. |
| `horizon` | The recipe was held under `horizon` before a real request answered the probe. |
| `idle_limit` | The idle window ran out before a real request answered the probe. |
| `parked_limit` | The parked ceiling ran out before a real request answered the probe. |
| `released` | By an operator's release, a key's opt-out, or the mod reporting the agent released. |
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
cooldown already held the recipe. `probes_held` counts a probe the router refused with `503 ledger_behind` while its
ledger caught up and sent again, outside the failed probes since no account answered it.

`GET /admin/keepalive/overview` sums the last 24 hours of these into `probes_24h` (`ok`, `failed`, `held`, `migrated`,
`skipped`), and per pool into the probes served, the requests they kept warm and the wasted probes by reason, beside the
pool's [idle-age counters](#the-return-rate-by-idle-age). It also answers the engine's session count, agent count and
retained bytes, each beside its cap. Its `bus_gaps` counts the times the engine found it had missed bus events. Each gap
discarded every observation still open, so those agents wait for their next request while an armed recipe keeps its
schedule.

Prometheus carries `phoebis_keepalive_agents`, the agents holding a recipe by the mod's lifecycle word,
`phoebis_keepalive_probes_total` by pool and outcome (`landed`, `cold`, `failed` or `held`), and
`phoebis_keepalive_skipped_total`, the withheld probes by pool and recipe state (see
[Statistics](statistics.md#metrics)).

## In the console

![The Sessions view: summary tiles, the cost reference link, and the session table with its Columns control and a forked
session's fork badge](assets/sessions-light.png#only-light)
![The Sessions view: summary tiles, the cost reference link, and the session table with its Columns control and a forked
session's fork badge](assets/sessions-dark.png#only-dark)

The Sessions view opens with a strip of tiles. Sessions, Agents and Retained read the keepalive engine's sessions, agent
records and retained prompt bytes, each as a count of the cap the bootstrap file sets (`3 of 128`, `12 of 1,024`,
`40 MiB of 256 MiB`). The Sessions tile's note counts the listed rows whose mod reports or went silent and those with a
recipe, and the Agents tile's note the agents holding a recipe by state. Then come cache probes in the last 24 hours by
outcome, sessions whose cache expired between turns, and the keepalive engine's bus gaps, the times it missed bus
events. A tile whose read failed draws a dash with the reason on hover, and Try again under the tiles reads them again,
as a line above the table does for a failed account read. Its table lists one row per session, led by the row's menu,
and offers eleven columns, eight shown by default (last request, agents and next probe start hidden, and
the Columns control turns any on): key, session, model, account, last used, requests, last request,
agents, companion, cache warming, and next probe. The model column reads the family and version, with the whole id on
hover, and the account cell names the account's kind on hover and to a screen reader. The companion column reads the
mod's state, `reporting`, `silent` or `none` (see [The mod](#the-mod)), and under `reporting` how long ago the person
was last seen at the terminal, as `presence 12s ago`. Opening a row expands into one line per agent that has sent a real
request: its model, pinned account and TTL, the last request, the last and the next probe, and the probes sent today
with the requests they kept warm, counted from midnight UTC. The line ends on its state word with the cause beside it,
the cause's sentence on hover (see [The five states](#the-five-states)). Under the columns it reads the mod's lifecycle
word for the agent, with a parked agent's park kind and its deadline or wake time
(`parked · background · deadline in 1h 12m`), and repeats the state's and the cause's sentences. A session's menu
offers "View in analytics" and "View in observe" once it has ledgered traffic, "View in routing log" whenever the row
has a routing session, which opens the log on that session's decisions, and Release. The analytics view opens on the
requests of the key holder the row belongs to. The view reads its sessions again every 15 seconds, so a session that
keeps sending requests stays on it, and its tiles once a minute.

A session
whose first request that is not auxiliary branched from another conversation at depth 2 or deeper carries a `fork at 3`
badge beside its id, 3 being the depth where the two part; following the badge scrolls to the row of the session it
branched from and marks that row, and where that session is not listed the badge names it instead, as
`fork of 4ef130f1-90e0…`.

A pool card that keeps caches warm reads the pool's three resolved ceilings in whole hours after its `keepalive` chip:
`idle 1h · parked 44h · horizon 44h` for a plan pool on the one-hour entry, its 44.1-hour break-even floored to `44h`,
and a range such as `44h to 71h` where the pool's combinations differ. The pool dialog's Keepalive group holds the
switch and the three numbers. A ceiling left off reads the break-even it resolves to, `Break-even: 1d 20h` or
`Break-even: 1d 20h to 2d 23h`, with what the break-even means on hover. Under `Max idle (s)` the group reads the idle
window's cost as a share of one re-write, `Costs 2% of one re-write`, and while the field reads 0 or more than the
break-even, the window in effect, `capped at the break-even, 46m`. The router resolves the figures for the saved pool
(see [The four fields](#the-four-fields)), so an unsaved edit of the idle window moves only that note, and the dialog
then names the share as the saved window's, `at the saved 1h 0m`, until the save. Below the fields, Return rate by idle
age lists the pool's ten buckets (see [The return rate by idle age](#the-return-rate-by-idle-age)), each with the
windows that returned out of those that reached it and a bar of the rate, or "No idle windows recorded yet" before the
first.

![The Analytics view's Cache warming tab: probes by outcome, wasted probes, and kept-warm
requests](assets/analytics-cache-warming-light.png#only-light)
![The Analytics view's Cache warming tab: probes by outcome, wasted probes, and kept-warm
requests](assets/analytics-cache-warming-dark.png#only-dark)

The Analytics view's Cache warming section draws the probe cost and the kept-warm count, probes by outcome, wasted
probes by reason, kept-warm requests and tokens, and cache warming by pool.

### The cost reference

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
with a fast-mode row under each model that serves fast mode, at its fast cells, what an answer served fast costs, and a
row under each model priced by prompt length, at its prices over the threshold.

Both tables list the models family by family, Fable, Opus, Sonnet, Haiku, the newest version first, the order both
shipped files keep, and a file kept in another order lists the same. Plan rates lists the weighted models first and the
others under the divider. The dialog's footer names each table's date and source, `shipped` or `file`, with the key and
the path that set or would replace it in the title.

API prices carries the caption "USD per million tokens at list price. An API-key account pays these. A subscription
account pays them in overage and for fast-mode answers." A probe's own row in the request ledger is priced from it in
dollars, whatever the account's funding, and a probe on a model the price table does not list prices at the table's
dearest cell of each type, never at zero. On a model priced by prompt length, a probe prices at the set its own prompt
selects. The two tables also set a recipe's break-even, which the pool's ceilings resolve to (see
[The four fields](#the-four-fields)). A probe goes out for an hour when the pool's cache TTL policy is on and the rule
set the account's funding selects holds `1h` for the probe's agent, since the relay rewrites every marker of the probe
to that TTL (and adds one where `inject_when_absent` is on). With the policy off, it goes out for an hour when the
recipe keeps an hour's cache or marks any part of its prefix for an hour.

The nine shipped rows, all prices in USD per million tokens, read from `crates/ledger/api_prices.yaml` in its order
(the three Opus models' fast cells are twice each figure):

| Model | Input | Cache read | Cache write 5m | Cache write 1h | Output |
| --- | --- | --- | --- | --- | --- |
| `claude-fable-5-1` | 10.00 | 0.25 | 12.50 | 20.00 | 50.00 |
| `claude-fable-5` | 10.00 | 1.00 | 12.50 | 20.00 | 50.00 |
| `claude-opus-5-5` | 4.00 | 0.20 | 5.00 | 8.00 | 20.00 |
| `claude-opus-5` | 5.00 | 0.50 | 6.25 | 10.00 | 25.00 |
| `claude-opus-4-8` | 5.00 | 0.50 | 6.25 | 10.00 | 25.00 |
| `claude-sonnet-5-5` | 2.00 | 0.10 | 2.50 | 4.00 | 10.00 |
| `claude-sonnet-5` | 2.00 | 0.20 | 2.50 | 4.00 | 10.00 |
| `claude-haiku-5-5` | 0.10 | 0.01 | 0.125 | 0.20 | 0.50 |
| `claude-haiku-5-5`, prompt over 100,000 tokens | 0.50 | 0.05 | 0.625 | 1.00 | 2.50 |
| `claude-haiku-4-5` | 1.00 | 0.10 | 1.25 | 2.00 | 5.00 |

`GET /admin/sessions/costs` adds a sixth figure per model, `reads_per_write_5m`, the 5-minute cache write price
over the cache read price: 50 for `claude-fable-5-1`, 25 for `claude-opus-5-5` and `claude-sonnet-5-5`, and 12.5 for
the other six.

## A router restart

The engine writes its state to the registry every 30 seconds and at shutdown: the sessions, every agent with its state
and its park, the retained recipes and the idle-age counters. A recipe's prompt is sealed under the storage key, as the
credentials are. A probe in flight is not saved. The shutdown stops taking reports and the events of requests in flight
before its last save, so that save holds the state the engine had when it stopped, and a report that arrives later is
answered `quiesced`. A save that fails is logged once and retried at every flush, and the engine keeps running from
memory.

At start the engine reads that state back before its first tick. Every session returns silent, its lease at zero and its
sequence and its agents' generations kept. The mod's next heartbeat answers `instance_ended`, the mod registers the same
instance again, and the session is re-adopted with its recipes, so its next probe lands warm. A session saved attended
keeps its park rules for one lease length after the load or until its registration arrives, then the silent-session rule
applies. A recipe whose entry expired while the router was down ends with `expired_at_restart`, and a session the mod
had ended is dropped. The probes such a recipe left unanswered settle as wasted only after the registry has dropped them
from the agent: a save that fails sends nothing, and a crash between that save and the ledger's write leaves them
uncounted rather than counted twice. A prompt that no longer opens under the storage key is dropped too, and its agent
keeps everything else. The bootstrap file's `keepalive` limits in force at the start bound what comes back: when they
are lower than before, the newest sessions, agents and recipes are restored and the rest are evicted.
