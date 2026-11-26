# Keepalive

This page is for the operator turning cache warming on for a pool and reading the words the Sessions view uses for it. A
prompt cache entry expires five minutes, or one hour, after its last use, and with keepalive enabled on a pool the
router keeps a replayable copy of every eligible request a session sends and, shortly before the entry would expire,
sends a probe that reads it. The page covers how a probe works, what the companion mod tells the engine, how to turn
probing on, which requests re-arm a recipe, the policy's four fields and the ceilings they set, the five states a recipe
reads, what the ledger records for every probe, where the console shows all of it and what a router restart keeps.

## How a probe works

A request is eligible for a replayable copy when its thinking is adaptive or disabled, its tool choice is not forced,
and its output is neither structured nor bound to a strict tool. A request that fails one of those checks, whose body
runs over `proxy.max_request_bytes`, or whose header block runs over the replay's budget (8192 bytes for one value,
16384 for the block) is never retained, and the Sessions view reads it `not_retained` with the check it failed as the
cause (see [The five states](#the-five-states)). A request whose body references account state is retained like any
other, and its probe goes out only while the session's pin stands on the account that served it.

Shortly before the retained request's prompt cache would expire, the router sends a probe built from it through the same
failover loop a real request takes, under the headers the bus published for the request: the same body with `max_tokens`
set to `0`, `stream` set to `false` and the client's `safeguards` member removed (the request for a server-side auto
mode check on the turn's tool use, which a replayed turn has no use for, whatever the pool's `auto_mode_server` says).
The request's `anthropic-beta` line travels as kept, and a pool or key set off drops the `dangerous-tool-use-2026-09-03`
beta from a probe as from any request (see [The betas](relay.md#the-betas)). The probe follows the session's pin and
renews it, and it never moves the pin. A probe goes out only while the pin stands on the account that last answered its
recipe. Once a real request has moved the pin, the recipe reads `pin_moved` under `held` and waits for the session's
next request that writes the cache on the pin's account. Where the pinned member can serve, the probe goes to it, and a
rule that would move the pin, a standing shed mark among them, waits for the session's next real request. Where the
pinned member cannot serve, the probe is answered rather than placed on a sibling, since a sibling would move the
session's pin onto a member no request asked for. The agent then reads `budget` at a spend budget, `paused` on an
account the operator disabled, on a deleted account and on one in a state other than ready that the probe did not try,
`ineligible_credential` past the subscription's end, `retryable_probe_error` on a member this probe already tried or on
one with a budget while the ledger is degraded (`budget_unknown`), and `probe_quota_exhausted` under a cooldown, a
credits block, overage under `never` or a seat at its spend cap that the hold below did not read in time, with the wait
the answer names. With no eligible sibling the probe gets the pool's own answer ([When nothing can
serve](routing.md#when-nothing-can-serve)), and the wait that answer names holds up to the ladder's five-minute cap. A
probe whose session holds no pin, because the pin idled out or its account left the pool, places nothing either, and the
recipe pauses under `no_pin` until the next real request lands. A probe's own 429 cools the account and leaves the pin,
which the session's next real request then moves, unless the session holds account state: its bound request waits for
the account or leaves the pin as [Selection](routing.md#selection) describes. A probe whose pinned account was deleted
is held as `paused` before it is sent, as one on a disabled account is. A probe whose pinned account's subscription
ended is held before it is sent too, and so is one due nearer to the end than its own interval, the entry's TTL less the
lead. The agent reads `ineligible_credential` under that hold as well, and the hold is a live one: it ends when the term
clears, and the recipe leaves as `expired` once its entry has expired. That last interval falls inside the pool's
wind-down, or reaches before it where the interval is longer than `wind_down_minutes`, so no probe writes a cache the
end discards and the pin turns cold at the end ([Subscription end](routing.md#subscription-end)). A probe pinned on a
usage-based seat at its spend cap is held before it is sent as `spent`, and a fast recipe whose pinned member's usage
credits are blocked as `credits`, each read at every tick, so neither writes a rejected request row while the state
stands.

A usage-based Enterprise seat's probes hold under its member budget as an API-key member's do, and never for overage:
the router reads no overage on such a seat ([Overage](routing.md#overage)).

A probe costs a cache read at list price, which its own row in the request ledger carries under the kind `probe`. A
later request that read the entry a probe kept, a probe that rewrote the entry included, is counted as kept warm, per
pool and per hour.

A probe of a request sent fast runs fast too, since the replay keeps the body as sent: it draws usage credits at the
fast rates, and its ledger row is priced at the fast cells. A probe the upstream served at standard speed is priced at
the standard cells. The upstream shares no cached prefix across speeds, so a probe keeps a request of its own speed warm
alone: a session that switches between fast mode and standard requests earns no credit from the other speed's probe.

A pool's cache TTL policy applies to a probe the way it applies to the request it replays, because the probe goes
through the same edit path: a marker-less recipe body takes the top-level member only where the policy's
`inject_when_absent` is on and the recipe's class is not auxiliary. The engine learns the TTL an entry was written with
from the response's usage and from what the router sent, never from the marker the client wrote. A usage that reports a
cache write without saying which TTL it was written at, as the stream's running totals do, proves nothing about the TTL
and leaves what the engine knew in place. The first probe after a turn follows that turn's cache TTL. The probe itself
writes with the TTL its agent's branch holds in the rule set, which the next probe then follows.

Cache diagnostics apply to a probe the same way. With the pool's `cache_diagnostics` option on, a probe carries the
cache-diagnosis beta and a `diagnostics.previous_message_id` member only when the replayed body carries a cache marker
of the client's and a tools list, the gate every request passes. The member a probe carries names no previous message
(`null`), whether the router added it or the client's body held one ([The body edits](relay.md#the-body-edits)), so the
upstream has no earlier answer to compare the probe against. Where the probe's answer names no miss reason, the request
log reads `not_returned` for it, as for any attempt whose body carried the member. The keepalive tells a hit from a miss
by the probe's cache tokens, the read and the write of its usage. A probe's own answer is never remembered as a previous
turn, so the next real request on the same account names the last real answer. A probe's 400 naming an injected item
strips it and retries the probe but never latches the account, since only a real request's diagnostics latch. The next
real request re-sends the item, takes its own 400, and latches.

The thinking-binding report applies to a probe the same way. A probe replays its turn's thinking blocks whole and the
request's headers as the bus published them, the client's `thinking-binding-controls-2026-08-01` token among them, and
with the pool's `thinking_diagnostics` on the router adds the token where the replayed body's thinking is on and its
line lacks it. The probe's answer files its `input_transformations` on the probe's own row, as a real request's does.

## The mod

The engine learns which sessions are alive, which agents they run and what each agent is doing from the companion mod, a
Claude Code plugin that runs inside the client's own process, reports session liveness and agent state to the router and
holds no policy: every ceiling and switch is a pool field or a constant of the router. Installing the mod, its settings,
the order it reads its credential in and what it reports are on [The companion mod](../companion/index.md), and the
route it reports to, its eight events, the acknowledgment and the refusal codes are on [The
relay](relay.md#the-companion-protocol). With its settings unset the mod reports under the URL and the key of [the
environment block](client.md#the-environment-block). On the router's side, a registration holds a lease over the
session, 90 seconds long, which each heartbeat renews, and the Sessions view names the mod's state for each session:
`reporting` while the mod reports the session's agents, their parks and the person's presence, `silent` once the lease
lapsed, with the time the mod stopped reporting, and `none` for a session the router saw but no mod registered, which
the router still pins and whose requests it still sees. The router reads the instance header the mod has the client send
on every model request of its process, `X-Phoebis-Instance-Id`, to tell that process's requests from another process's
on the same session id. While the holder's lease is live, a request that names another instance leaves the session's
recipes as they were. The router strips the header before a request leaves. Once the holder's lease has lapsed, a real
request that names another instance arms its recipe as the holder's does. A registration starts main `busy` under the
cause `awaiting_request` until its first request.

## Turning it on

Enabling `keepalive.enabled` on a pool needs `affinity_ttl_seconds` of at least 300 seconds, refused otherwise with
`keepalive.enabled requires affinity_ttl_seconds of at least 300: a probe follows the session pin, and a pin shorter
than the shortest cache TTL cannot keep the session on the account it warmed`. A probe follows the session's pin to a
subscription account as to an API-key one. A key without the `keepalive` companion permission is never retained, and the
Sessions view names the cause for such a session as `key <name> permits no cache warming`. Taking the permission away
reaches a session that is already idle: its held prompt is dropped under `key_opted_out` rather than kept, since nothing
on that key may be probed again until the permission is back. A new key holds the permission, and the key dialog edits
it as "Cache warming" ([The key's permissions](companion-mod.md#the-keys-permissions)). A session whose companion mod
registers it with its own cache warming option off is never retained either, and the Sessions view names the cause as
`cache warming off for the session`. A registration that turns the option off reaches a held prompt in the same step:
the prompt is dropped under `session_opted_out`.

In the console the switch is in the pool dialog: Pools, Edit, the Keepalive group under Caching, "Keep prompt caches
warm", with the policy's three numbers under it (see [Pools](console.md#pools)).

The bootstrap file's `keepalive` block holds the process-wide byte cap alone, `max_retained_bytes`, and its `sessions`
block holds the two caps of the session engine, `max_agents` and `max_sessions`. Each is read once at start, so changing
one takes a restart (see [keepalive](installation.md#keepalive) and [sessions](installation.md#sessions)). The lease and
the four probe workers are fixed. A session holds a slot of `max_sessions` from its first real request until its end,
and its agents hold slots of `max_agents` from that request until their records leave. A first request over the session
cap is not kept and reads `session_limit`, and one over the agent cap reads `agent_limit`, the words a registration over
either cap answers.

## Which requests re-arm a recipe

A recipe's cache stays warm only for a request that continues the conversation it was built from. The engine reads the
request's class first: `main` and `subagent` re-arm the recipe, and a completed compaction stops it until the next main
turn. Any other class, a title or a summary request among them, leaves the recipe, its schedule and its credit
untouched, and shows as `last_guard: auxiliary`.

A `compaction` request reads the conversation's old prefix to write the context the next turn starts from, so a recipe
armed from it would keep warm a body no turn reads again. While the compaction runs, the recipe waits as it stood with
no probe sent, and shows as `last_guard: compaction`. Once the compaction completes, it answers the probes before it as
a turn does, and the recipe ends under `compacted`: no probe goes out until the next main turn arms a recipe for the
new context. A compaction that failed or was canceled keeps the previous recipe, since the old context stays the live
one, and the engine resumes its schedule on it.

Without a class, the request's own two boundary-hash chains decide. A boundary that holds the recipe's last message hash
re-arms it, since the request continues the same conversation. The same static prefix, the tools array and the system
blocks, on the same model, with no continuation restarts the recipe, replacing its cached body. A request matching
neither the chain nor the prefix shows as `last_guard: foreign`, and its recipe is left alone the same way an auxiliary
one is. A body carrying no chain at all re-arms on every request, the fallback the router's own warmth check applies for
a client that sends none.

The kept-warm credit follows the same rule: only a request that re-armed the recipe or a completed compaction earns it,
and it goes to the probe whose entry that request read, a probe that rewrote the entry included, the newest probe that
covered the entry taking it over a later one that did not cover it. An auxiliary or a foreign request may still read a
warm cache, but it never resets the recipe's schedule and settles nothing. A failed turn decides no probe and is no
evidence about the cache, so a retry inside the probe's entry earns the credit the failed turn could not, its gap
measured from the last turn that answered. A turn whose completion the engine missed, whether the next request's arrival
or a bus gap ends it, counts as a turn that answered at its start. Probes on one model, speed and account form a chain
while each later probe reads the whole entry back, since that read extends the entry, and a probe that reads nothing or
only part of the prefix starts a chain of its own. A request on that model, speed and account that reads the cache
inside the chain's last entry answers every probe of the chain, and one that arrives past that entry settles them all
`expired`.

A request a guardrail blocks is a refused request to the engine. A blocked `main` or `subagent` request, or one with no
class whose chain re-arms the recipe, ends the armed recipe and its schedule as any refusal does. A blocked `compaction`
request is a compaction that did not complete, so it keeps the recipe as a failed one does, and a blocked auxiliary
request leaves the recipe, its schedule and its credit untouched ([Guardrails](guardrails.md#what-a-guardrail-is)).

## The four fields

The pool's `keepalive` object carries four fields:

| Field | Default | Bounds | Meaning |
| --- | --- | --- | --- |
| `enabled` | `false` | | Probing on for the pool's sessions, every member kind. |
| `max_idle_seconds` | `3600` | 0 to 31536000 | The main conversation's idle window, taken as set, 0 the break-even. |
| `max_parked_seconds` | `null` | null or 60 to 604800 | Time a park on a background command or a subagent is probed. |
| `scheduled_horizon_seconds` | `null` | null or 60 to 604800 | The farthest wakeup a scheduled park is probed toward. |

The break-even of a recipe is the pause length at which its chain of probes costs one re-write of its prefix,
`(write - read) / (cadence * read)`. The read and write factors are those of the recipe's model under its account's
funding. A plan-funded recipe reads the cost model's type multipliers, the model's own where it sets them. A token costs
the model's weight times the type's multiplier, so the weight enters the probes' cost and the re-write they save alike
and cancels. A model without a weight prices nothing and breaks even where a weighted one does. A plan-funded probe and
the saving it keeps are priced in the 7-day window of the account's tier, the window every plan keeps, and the wire
carries each as the Pro-week figure the console reads in the chosen week. A dollar-funded recipe reads its model's row
of the price table, and a model the price table does not price reads its tightest row, the one with the smallest
break-even among the priced rows. On a row priced by prompt length, the recipe reads the set its request's prompt length
selects, as the probe's own cost does. The cadence is the TTL's: 15 probes an hour on a five-minute entry and 1.11 on a
one-hour entry. On the shipped cost model, a plan-funded recipe breaks even after 44.1 hours on a one-hour entry and 3
hours 16 minutes on a five-minute entry. Every tick reads the price table and the cost model at the revisions in force
then, so an entry that takes effect moves the break-even from the next tick, without a restart.

A `max_idle_seconds` of 0 resolves to the break-even and any other number is taken as set, above the break-even too,
where the pool dialog notes it. A `null` parked or horizon ceiling, and a parked or horizon number above the break-even,
resolve to the break-even, so those two numbers take effect only below it. `GET /admin/pools` answers `null` where null
was set. An out-of-range value is refused with `keepalive.{field} must be between {min} and {max}, not {value}`, for
example `keepalive.max_parked_seconds must be between 60 and 604800, not 59`. A value that is not an object, or a field
the policy does not hold, is refused with `keepalive is not a policy object: {detail}`, the detail naming the fault.

For the console, `GET /admin/pools` also answers each pool's `keepalive_resolved`: the break-even and the three ceilings
in seconds and the idle window's cost as a share of one re-write, each as the lowest and the highest figure over the
pool's combinations. An OAuth member brings the cost model's factors and an API-key member the price table's. A
usage-based seat brings the price table's too, since its probes settle in dollars. The pool's cache TTL policy gives the
main conversation's TTL under each, and a policy that leaves the markers as sent brings both TTLs. The models are the
price table's and those the cache TTL policy's per-model rows and the model rules' entries without a wildcard name,
which carry the catalog's dated ids, each kept where the pool's model rules allow it. Rules that allow none of them
leave a model priced nowhere. An undated price row the router's catalog maps to a dated id is judged under that id, as a
request on the model is, so a deny entry naming the dated id drops the row. On a router without a catalog reading the
row is judged under its own id, and a deny entry naming a dated id leaves the model's undated price row in the range, so
for a model the catalog lists only dated, such as Haiku, the range keeps a figure no request on the pool reaches. A pool
with no member answers `null`. A fast recipe is dollar-funded at the fast cells whatever funds the account, and
`keepalive_resolved` describes standard speed, so a fast recipe's break-even reads the fast cells and not the figures
the pool dialog shows.

The schedule's numbers are fixed. A probe is due 60 seconds before a five-minute entry expires and 360 seconds before a
one-hour entry, less a random jitter of up to 5 and 30 seconds that spreads a pool's probes so one tick never bursts. At
most four probes run at once, and one exchange is bounded at 300 seconds, so a worker never waits on an upstream for
longer than the schedule it serves. Two cold probes in a row pause the recipe until a real request. A transient failure
of a probe, a `429`, a `5xx`, an exhausted pool or a transport error, retries after 30 seconds and doubles the wait up
to 300 seconds while the retry still lands before the entry's expiry, and a failure whose next retry would land at or
past the expiry pauses the recipe until a real request.

## The ceilings

Three rules bound how long a retained recipe is probed, one for each way a session comes back, and the recipe is probed
while any of them says so. Every park has a certain return: a cron fires, a command exits or reaches its deadline, a
subagent completes. The two park rules are loss bounds. The idle window is the one bet, and it runs in every state of
the main conversation, because the user can type while a background command, a subagent or a cron is open.

**The idle window.** The main conversation is probed until `max_idle_seconds` after the latest of its last real request,
its last presence report and the end of a park without a request, a deadline or a wakeup that passed. The window runs to
the set limit, above the recipe's break-even too, and a `max_idle_seconds` of 0 runs it to the break-even. The pool
dialog notes a window set past the break-even, whose share of one re-write can read above one. A wakeup is a real
request, so a cron that fires or a command that exits restarts the window. A silent session, one whose lease lapsed, is
probed under the window from its last real request for every agent, and the park rules do not apply to it. A session
saved attended reads silent after a router restart but keeps its park rules until its registration arrives or one lease
length passes, and the silent rule follows only when the lease length passes first.

**Parked on a background command or a subagent.** The main conversation or a subagent reported `parked` with kind
`background` is probed until the return, at most `max_parked_seconds` after the park began. A real request or a state
other than `parked` ends the park. A command's deadline ends the park when it passes, because the harness stops the
command and wakes nobody, and the idle window restarts at that moment.

**Parked on a scheduled wakeup.** With kind `scheduled`, a due probe goes out while the wakeup is at most
`scheduled_horizon_seconds` away. Beyond that the recipe is held with the reason `horizon`, its probes stopped and its
prompt kept while the entry lasts. A later `parked` report with a nearer wakeup reopens it, as does a horizon raised on
the pool, and probing resumes if the entry is still warm. A held recipe whose entry expires ends as `expired`. A wake
time that passes without a request ends that part of the park, as a passed deadline ends a command's part, and the idle
window restarts at that moment, so a park on a cron that never fires ends at the wake time it named.

A park of kind `both` is probed while either part says so, and a hold or an end applies only once neither does. When
neither does and the wakeup lies beyond the horizon, the recipe is held rather than ended, since the wakeup's return is
certain. A subagent's park follows the two park rules on its own, and the idle window never applies to a subagent, which
is busy, parked or released. A busy agent that stays silent past the lead is probed with no ceiling of its own: a long
foreground tool call on a five-minute entry stays covered, and the lease and the idle window bound a client that died.

At a ceiling the recipe ends with the reason `idle_limit` or `parked_limit`, or is held with `horizon`. At the idle
limit its unanswered probes stay pending until a real request reads them, which credits them inside their entry, or
until their entry lapses, which counts them as wasted under `idle_limit`. Under `parked_limit` and `horizon` they count
as wasted under that reason at once. The next real request re-arms the recipe.

### The return rate by idle age

The idle window is a bet whose right size differs by pool, so the router records, per pool, how long the main
conversation stays idle and whether it comes back to the same conversation. A window opens once the main conversation
reports idle or parked, from the idle window's start above, and two counters per idle-age bucket record it:

- `reached`, the windows that lasted to the bucket's start.
- `returned`, the windows whose next request from the main conversation continued the conversation inside the bucket:
  one of the request's message boundaries is one of the last two of the request before the window opened or the deepest
  boundary before them that this earlier request marked for the cache.

The router hashes a content array of text blocks as its blocks up to the last one carrying the cache marker, a single
remaining block as its text, so the last message the client sends again in the next request holds its boundary (see [The
boundary chains](relay.md#the-boundary-chains)). A return also holds where the request carries the deepest boundary
before the last two that the earlier request marked for the cache, since the upstream reads the cache through it, so the
request after an interrupted turn counts though it holds neither of the last two. A main conversation's request of class
`main` or `subagent`, neither a compaction nor the first request after one, whose chain holds neither of the last two
boundaries of main's previous admitted request nor its deepest marked one while its answer read at least nine tenths of
that request's prompt from the cache contradicts the router's chain model. The router logs it at warn once per client
version per hour, naming the version, the session and the two token counts, counts each one in
`phoebis_keepalive_chain_contradictions_total` by client version, and changes no return counter and no recipe for it.
The watch compares against the prompt of main's last turn that it marked or the keepalive followed, so a request refused
at its arrival on a keepalive-off pool or from a key without the `keepalive` permission leaves that prompt in place
unless the watch marked it, and a session whose requests were all refused that way has no prompt and marks nothing.

The ten buckets run 0 to 5, 5 to 15, 15 to 30 and 30 to 60 minutes, then 1 to 2, 2 to 4, 4 to 8, 8 to 16 and 16 to 32
hours, and above. Windows are counted whether or not a recipe is retained, so one that outlives the idle window still
records when the user came back, and a pool with keepalive off or a key without the `keepalive` permission records its
returns too. A window that opens before the session's first request has no conversation to compare against and counts no
return. A window counts under the pool its key was in when it opened. A return's age is read at the prompt that
announced it, however long the request takes to follow. A lapsed lease, a park that ends without a request, a release of
the main conversation, a request on a new conversation and the session's end close a window with no return. A presence
report leaves an open window open, so a return typed after a long gap counts at the gap's age. After a request that
continued or restarted the conversation, the next window starts at the request's own instant, except on a main
conversation reported `unknown`, where the request moves no start. After any other close, the next window opens only
once its start moves past the one in force at that close, at an `agent_start` of a released main, a park's end, a
presence report, a prompt, a request or the session's revival. A busy report pauses the count without closing the
window, and a subagent or a silent session opens none.

No rule reads the counters: the idle window is the pool's `max_idle_seconds`, taken as set. The pool dialog shows them
by bucket.

The counters are kept per pool and UTC day. A bucket counts as reached on the day a tick or the request that ends the
window reached it. A return counts on the day its bucket was reached, so a window that returns after midnight in a
bucket it reached before midnight counts the return on the earlier day, and no day shows a bucket with more returns than
windows that reached it. The pool dialog reads the counters over its range of 7 or 30 days, the current UTC day and the
6 or 29 before it, and the router drops a day older than 30 days.

## The five states

The Sessions view reads each agent's recipe as one of five states, with the engine's own reason beside it as the cause.
The cause's sentence shows on hover over the state word and again in the row's expanded detail. Every agent that has
sent a real request keeps its record in the engine until its session leaves it, and its line in the view with it, so a
recipe that ended, or a request that was not kept, still shows with its cause. The one record that leaves earlier is a
released one, a subagent the mod released while its session lives or any agent its session's end released: the view
drops a released line under a live session at its next re-read, while the engine keeps the record for one pool cache TTL
after the release, the longest TTL the pool's cache TTL policy names, refusing a late request and a late probe result
under that id and holding its slot of the agent cap, and then drops it, its line leaving the view with it. A later start
under the id registers anew.

The console's words for the causes are in the labels table under [In the console](#in-the-console).

| State | What it means |
| --- | --- |
| `retained` | The prompt is kept and its probe is scheduled, in flight, or waiting for the first stream event. |
| `held` | The prompt is kept and no probe is sent while a condition holds that clears by itself or by an operator. |
| `paused` | The engine paused the recipe after a probe's answer or a rejected request, until the next real request. |
| `not_retained` | The last request was not kept, for its shape or size, the engine's limits, no cache evidence, or a failure with no recipe armed before it. |
| `ended` | The recipe was discarded: a ceiling, a completed compaction, an end, an eviction, a lapse or a release. |

Each state reads these causes:

- `retained`: `awaiting_cache_evidence`, `awaiting_selection`, `lease_expired`, `probing`, `request_canceled`,
  `request_failed`, `scheduled`.
- `held`: `affinity_too_short`, `budget`, `cooled`, `credits`, `horizon`, `ineligible_credential`, `keepalive_off`,
  `ledger_behind`, `no_probe_cache_evidence`, `overage`, `paused`, `pin_moved`, `pool_disabled`, `probe_pool_exhausted`,
  `probe_quota_exhausted`, `probe_transport_error`, `reauth`, `retryable_probe_error`, `slow_probe`, `spent`.
- `paused`: `invalid_probe_usage`, `missing_account`, `no_pin`, `partial_cache_coverage`, `probe_authentication_error`,
  `probe_retry_limit`, `repeated_cold_probe`, `request_rejected`, `unexpected_output`, `unsupported_probe`.
- `not_retained`: `agent_limit`, `awaiting_request`, `fixed_thinking`, `forced_tool`, `headers_too_large`,
  `instance_mismatch`, `invalid_cache_control`, `invalid_model`, `invalid_request`, `no_cache_evidence`,
  `request_limit`, `request_too_large`, `session_limit`, `storage_limit`, `structured_output`, `unsupported_shape`.
- `ended`: `agent_released`, `agent_unknown`, `bus_gap`, `compacted`, `evicted`, `expired`, `expired_at_restart`,
  `idle_limit`, `instance_replaced`, `key_opted_out`, `opted_out`, `parked_limit`, `reset`, `session_ended`,
  `session_expired`, `session_opted_out`.

A `held` or `retained` cause reads `paused` while the engine's own pause holds. A probe's structural error
(`invalid_probe_usage`, `missing_account`, `partial_cache_coverage`, `probe_authentication_error`,
`repeated_cold_probe`, `unexpected_output`, `unsupported_probe`) pauses the recipe and clears its schedule, as a
rejected request does. `no_pin` names a probe whose session held no pin to renew, and it pauses the recipe the same way.
`probe_retry_limit` names a ladder that ran to the entry's expiry still failing. A real request that fails or is
canceled leaves the last answered recipe armed on its schedule where one was armed at its arrival, the row reading
`retained` under the request's own word and `not_retained` where none was, while a rejected request pauses it. A gate's
hold that lands on a paused recipe at a later tick (`budget`, `cooled`, `ineligible_credential` or `paused`) re-arms
nothing, so the row stays `paused` with the hold's cause until the next real request. The paused prompt itself leaves
when its entry goes cold: at the entry's expiry the engine drops it, writes off its probes under `expired`, each as its
chain's last entry ends, and sends nothing, and the row still reads `paused`. The same hold on an armed recipe reads
`held`, since the gate reads it again at every tick and probing resumes when it clears. A recipe held on a disabled pool
reads `retained` again at the first tick after the pool is enabled. A `held` or `retained` cause reads `not_retained`
when the agent holds no prompt, as a request refused at its account selection does. `keepalive_off`, `opted_out` and
`session_opted_out` are recorded when the request arrives and no body is kept, while `ineligible_credential` is read at
the account selection. A request refused at its arrival credits no probe, so a probe outstanding when the pool's
keepalive is turned off or the key's `keepalive` permission is taken away is judged at the agent's next followed turn or
completed compaction, or settles as wasted.

`partial_cache_coverage` names a probe that did not leave the whole prefix in the cache. The measure is the count of the
agent's last request, the tokens it read and wrote. A probe on the recipe's own account covers by reading that many
tokens back. One that read part of the prefix, or that read or wrote some of it and stayed under the count, pauses the
recipe, and one that read nothing and rewrote the prefix counts as a cold probe.

A session's recipes end together when the session does. A `session_end` report, a registration that replaces the
instance and the session's own expiry each discard every recipe of the session at once, and the deletion of the key that
registered it, or the same key id created again, takes the previous holder's sessions and their recipes with it within a
second of the deletion, rather than leaving them to the session expiry.

A request whose `system`, `text`, `stream` or `max_tokens` member the replay's edits cannot reach, such as one whose key
is spelled with a JSON escape, is not retained and reads `not_retained` under `invalid_request`: a probe built from it
would travel with a stale stamp or a doubled member.
A request whose billing line holds a `cch=` the replay cannot read or reach is not retained under the same reason.

The engine reads a request's cache markers at the five positions the Messages API takes one from, a block inside a tool
result's content among them: a marker that is not ephemeral or names a TTL other than `5m` or `1h` leaves the request
`not_retained` under `invalid_cache_control` wherever it sits, and a readable one sets the TTLs the recipe's probes
follow.

### The guard field

Every request the recipe observes also updates `last_guard`, `null` before the first one: `rearm` or `restart` for a
request that continued or restarted the conversation, `compaction` for a compaction request, which leaves the recipe as
it stood and stops it at its completion, and `auxiliary` or `foreign` for one the guard set aside (see [Which requests
re-arm a recipe](#which-requests-re-arm-a-recipe)). `last_guard` never holds the recipe by itself. An auxiliary or a
foreign request leaves `state` and `cause` exactly as they were, so a recipe `held` under `cooled` with
`last_guard: rearm` is a real request that re-armed it, followed later by a cooldown on its account, not a
contradiction.

### Holds and lapses

A recipe holds under `affinity_too_short` while the pool's affinity ends before its probe is due, the entry's TTL less
the lead: the session could move away from the account the probe would warm, so raise `affinity_ttl_seconds` past that.
A five-minute recipe needs about four minutes of pin and a one-hour recipe about fifty-four, and the 300-second floor
that `keepalive.enabled` asks of the pool when it is saved stays the one refusal.

An entry the engine knows has expired is never probed: its recipe ends under `expired`, its probes are written off, each
at once or, where a probe sent while a request was open keeps its chain alive, at the chain's end, and the engine logs
one line naming the key, the agent, the expiry and how late the tick was. A recipe held under any other reason, a gate's
hold among them (`affinity_too_short`, `budget`, `cooled`, `credits`, `ineligible_credential`, `keepalive_off`,
`overage`, `paused`, `pin_moved`, `reauth` or `spent`), leaves as `expired` once its entry has expired, and the next
real request starts a new one. A recipe held under `horizon` or on a disabled pool leaves the same way, and one reopened
or enabled again while its entry is warm probes on. While the request a recipe follows is still open, the lapse waits
for its completion. At the retained-bytes cap the engine evicts before it refuses: unattended sessions' agents first,
oldest activity first, then attended sessions' idle agents, never one with a probe or a request in flight. A lapsed or
evicted recipe reads `ended`, as one stopped under `idle_limit` does. An evicted one shows on the wasted counter at
once, while the probes of a lapsed one reach it at once where their chains have ended and otherwise as their chains'
last entries lapse with no real request, as the probes of one stopped under `idle_limit` do. A lapse also shows in that
log line.

A quota `429`'s `Retry-After` counts only where it is longer than the ladder's step. A probe answered
`503 accounts_unavailable` because its key moved to another pool while it was in flight is held under
`retryable_probe_error`: its next attempt admits under the new pool.

A probe the router refused with `503 ledger_behind`, its ledger catching up (see [Refusal
codes](client.md#refusal-codes)), holds under `ledger_behind` rather than `retryable_probe_error`: its next attempt
waits out that answer's `Retry-After`, or the 30-second retry wait without one, and counts no failure on the ladder, so
a ledger that stays behind never pauses the recipe. The entry's expiry and the ceilings still bound the re-sends: once
the entry expires, the recipe ends under `expired`, as under any other hold, and a ceiling that runs out first ends it
under its own reason.

A probe the router refused with `403 client_contract` or `403 client_drift` (see [Refusal
codes](client.md#refusal-codes)) pauses under `unsupported_probe`: the router refused the request the probe replays, not
its credential, and any other `401` or `403` pauses the recipe under `probe_authentication_error`.

### The wasted probes

A wasted probe settles on its own with one of nine reasons: when the recipe stops with a probe unanswered (at the idle
limit, once the last entry of the probe's [chain](#which-requests-re-arm-a-recipe) lapses before a real request reads
it) or is held under `horizon`, when the recipe is evicted, or lapses with the probe's chain ended (a lapsed recipe's
probe whose chain is still alive settles like a probe at the idle limit, once the chain's last entry lapses with no real
request), or when the next request after the probe reads nothing from the entry its chain kept:

| Reason | What settles it |
| --- | --- |
| `cold_return` | The next request read no cache, or on another account, model or speed, inside the last entry of the probe's chain. |
| `evicted` | The engine evicted the recipe at its retained-bytes cap. |
| `expired` | The last entry of the probe's chain expired before the next request arrived, or the recipe's before its probe was due. |
| `horizon` | The recipe was held under `horizon` before a real request answered the probe. |
| `idle_limit` | The idle window ran out and the last entry of the probe's chain lapsed before a real request read it. |
| `parked_limit` | The parked ceiling ran out before a real request answered the probe. |
| `released` | The key's `keepalive` permission taken away, a registration that turns the session's cache warming off, or the mod reporting the agent released. |
| `replaced` | A replacement registration arrived for the same agent. |
| `session_ended` | By an explicit `session_end` report or the session's own expiry with no report. |

The analytics view puts it plainly: a high idle-limit share says the pool's idle window outlasts how these sessions are
used, and a high parked-limit or horizon share says the parked ceilings do.

## What the ledger records

Every probe is a row of the request ledger, of kind `probe`, priced as any request is. Per pool and per hour, the
ledger's rollup carries the keepalive fields the console reads. `kept_warm_requests`, `kept_warm_5m_tokens` and
`kept_warm_1h_tokens` count the requests a probe kept off a cache miss on any model the request named and the tokens
they read warm, booked under the TTL the credited request's own cache evidence settled on. The ledger's analytics read
prices the saving from the two counts at read time, and the console reads it: the write each count avoided at its TTL's
rate less the read it paid. The credit row is keyed under the client of the request it credits, so the saving prices
under that client's regime as the request's own row does. It also files under the account tier of the request's own row,
the one the router's snapshot holds when the credit is booked. A probe's cost is its own request row's counts priced
when read. `wasted_probes` counts the probes settled wasted by one of the reasons above, and `wasted_probe_reasons`
counts them by the reason. The wasted probes' five token counts sit on the hourly rollup beside their count, priced the
same way. Each wasted probe files beside its own cost row, in the hour the relay received the probe and under the
funding that row took (the kind of the account that served it, whether that account is a usage-based seat, the speed its
answer reported and whether its answer reported overage in use), so the wasted part of the probes' figure, read in the
chosen week, is the plan-funded probes' waste against their own cost. `probes_skipped` counts a due probe a gate or a
lapsed entry withheld rather than sending, for example because a cooldown already held the recipe. `probes_held` counts
a probe the router refused with `503 ledger_behind` while its ledger caught up and sent again, outside the failed probes
since no account answered it.

`GET /admin/keepalive/overview` sums the last 24 hours of these into `probes_24h` (`failed`, `held`, `ok`, `plan_ok`,
`skipped`, where `plan_ok` lies inside `ok` and counts the answered probes a subscription window paid for), and per pool
into the probes served, the requests they kept warm and the wasted probes by reason, beside the pool's [idle-age
counters](#the-return-rate-by-idle-age). It also answers the engine's session count, agent count and retained bytes,
each beside its cap, and `expired_between_turns_24h`, the requests of the last 24 hours that missed the cache because
their entry expired between two turns, the misses probing exists to prevent: each carried a cache marker, arrived after
its entry's lifetime and read back under half the prompt of the session's previous request on the same account, under no
miss reason or one of the router's own words, and the count leaves out the agents that had ended or been released
([`expired_between_turns`](statistics.md#conditions-and-gauges) names the turns left out). Its `bus_gaps` counts the
times the engine found it had missed bus events. Each gap discarded every observation still open, so those agents wait
for their next request while an armed recipe keeps its schedule.

Prometheus carries `phoebis_keepalive_agents`, the agents holding a recipe by the mod's lifecycle word,
`phoebis_keepalive_chain_contradictions_total`, the [chain contradictions](#the-return-rate-by-idle-age) by client
version, `phoebis_keepalive_probes_total` by pool and outcome (`landed`, `cold`, `failed` or `held`), and
`phoebis_keepalive_skipped_total`, the withheld probes by pool and recipe state (see [Metrics](statistics.md#metrics)).

## In the console

The Sessions view reads each session's standing, the mod's word for it and a recipe summary: main's recipe, or the
newest other agent's where main holds none. A row's Cache cell reads `warm` while the row's pin is within its cache TTL
or `cold` past it, with that summary's state word under it. A row's expanded detail reads each agent's state and cause
with the sentence of each on hover, and its Cache warming details read each agent's TTL, last request, last and next
probe, and the probes sent and the requests they kept warm since midnight UTC (see [Sessions](console.md#sessions)). The
Dashboard's Sessions card reads the engine under "Engine caps": the sessions, the agent records and the retained bytes
against their caps, the cache probes of the last 24 hours by outcome, and the requests whose entry expired between turns
in that day while the keepalive still held their agent, beside the bus gaps since the start (see
[Dashboard](console.md#dashboard)). A pool card that keeps caches warm reads the pool's three resolved ceilings after
its `keepalive` chip, and the pool dialog's Keepalive group reads, under each field, the break-even a ceiling left off
resolves to and the figure in effect under one switched on past it (see [Pools](console.md#pools)). The Analytics view's
Cache warming tab draws the probes by outcome and the wasted probes by reason (see [Analytics](console.md#analytics)).

A session's row reads main's conversation pin, the newest by last use where main holds two, else the newest conversation
pin of another agent of the session, else no pin, and never a pin only auxiliary requests have used, such as the one a
title or a quota call placed on a model of its own.

The state's cause on the Sessions view reads a label in place of the engine's id, and the table maps the two.

| Label | Cause id |
| --- | --- |
| affinity too short | `affinity_too_short` |
| agent limit | `agent_limit` |
| agent released | `agent_released` |
| agent unknown | `agent_unknown` |
| awaiting cache evidence | `awaiting_cache_evidence` |
| awaiting request | `awaiting_request` |
| awaiting selection | `awaiting_selection` |
| at budget | `budget` |
| bus gap | `bus_gap` |
| compacted | `compacted` |
| cooled down | `cooled` |
| credits blocked | `credits` |
| evicted | `evicted` |
| entry expired | `expired` |
| expired at restart | `expired_at_restart` |
| fixed thinking | `fixed_thinking` |
| forced tool | `forced_tool` |
| headers too large | `headers_too_large` |
| wake too far | `horizon` |
| idle limit | `idle_limit` |
| ineligible credential | `ineligible_credential` |
| instance mismatch | `instance_mismatch` |
| instance replaced | `instance_replaced` |
| invalid cache control | `invalid_cache_control` |
| invalid model | `invalid_model` |
| invalid probe usage | `invalid_probe_usage` |
| invalid request | `invalid_request` |
| keepalive off | `keepalive_off` |
| key permits no cache warming | `key_opted_out` |
| lease expired | `lease_expired` |
| ledger behind | `ledger_behind` |
| missing account | `missing_account` |
| no cache evidence | `no_cache_evidence` |
| no pin | `no_pin` |
| no probe cache evidence | `no_probe_cache_evidence` |
| cache warming not permitted | `opted_out` |
| overage | `overage` |
| parked limit | `parked_limit` |
| partial cache coverage | `partial_cache_coverage` |
| account paused | `paused` |
| pin moved | `pin_moved` |
| pool disabled | `pool_disabled` |
| probe authentication error | `probe_authentication_error` |
| pool exhausted | `probe_pool_exhausted` |
| quota exhausted | `probe_quota_exhausted` |
| retry limit | `probe_retry_limit` |
| transport error | `probe_transport_error` |
| probing | `probing` |
| needs re-auth | `reauth` |
| cold probes | `repeated_cold_probe` |
| request canceled | `request_canceled` |
| request failed | `request_failed` |
| request limit | `request_limit` |
| request rejected | `request_rejected` |
| request too large | `request_too_large` |
| reset | `reset` |
| retrying | `retryable_probe_error` |
| scheduled | `scheduled` |
| session ended | `session_ended` |
| session expired | `session_expired` |
| session limit | `session_limit` |
| cache warming off for the session | `session_opted_out` |
| slow probe | `slow_probe` |
| spend cap reached | `spent` |
| storage limit | `storage_limit` |
| structured output | `structured_output` |
| unexpected output | `unexpected_output` |
| unsupported probe | `unsupported_probe` |
| unsupported shape | `unsupported_shape` |

<a id="the-cost-reference"></a>
The Cost reference dialog, what a million tokens costs in dollars and as a share of a subscription window, is described
with the Sessions view under [Sessions](console.md#sessions). The price table's rows and how a probe is priced are on
[Pricing](statistics.md#pricing), and the cost model's weights on [Plan meter](statistics.md#plan-meter).

## A router restart

The engine writes its state to the registry every 30 seconds and at shutdown: the sessions, every agent with its state
and its park, the retained recipes and the idle-age counters. The engine also starts a save right after a report of the
companion mod that changes what the registry holds of a session's or an agent's reported facts, so those facts do not
wait for the 30 seconds. Each settlement in that state names the window it was priced in, and a state an earlier build
saved reads the 5-hour window for it. A recipe's prompt is sealed under the storage key, as the credentials are. A probe
in flight is not saved. The shutdown stops taking reports and the events of requests in flight before its last save, so
that save holds the state the engine had when it stopped, and a report that arrives later is answered `quiesced`. A save
that fails is logged once and retried at every flush, and the engine keeps running from memory.

At start the engine reads that state back before its first tick. Every session returns silent, its lease at zero and its
sequence and its agents' generations kept. The mod's next heartbeat answers `instance_ended`, the mod registers the same
instance again, and the session is re-adopted with its recipes, so its next probe lands warm. A session saved attended
keeps its park rules until its registration arrives or one lease length after the load passes, and the silent-session
rule follows only when the lease length passes first. A recipe whose entry expired while the router was down ends with
`expired_at_restart`, and a session the mod had ended is dropped. The probes such a recipe left unanswered settle as
wasted only after the registry has dropped them from the agent: a save that fails sends nothing, and a crash between
that save and the ledger's write leaves them uncounted rather than counted twice. A prompt the storage key cannot open
is dropped too, and its agent keeps everything else. The caps of the bootstrap file's `sessions` and `keepalive` blocks
in force at the start bound what comes back: when they are lower than before, the newest sessions, agents and recipes
are restored and the rest are evicted.
