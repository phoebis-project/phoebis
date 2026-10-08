# Statistics

Every request that finished lands in the ledger, a second SQLite file beside the registry, and the console, the
analytics routes and the metrics listener read from it or from the same events. This page describes what is recorded,
how long it is kept, what each view shows, and which conditions the router exports as gauges.

## The ledger

Every request that finished lands in the ledger: one row for the client request and one row for each upstream
attempt it took. The relay waits for the ledger's queue when it must, so a row is never lost to a busy moment;
captured bodies are the one best-effort part, since a stream may outrun its recorder, and a capture whose chunks
were lost is stored as truncated with a `ledger_gap` event naming the request. Once a request finishes, it is
folded into the hour's rollup, which is what the analytics routes report, so a long range costs one row per hour
rather than one per request.

Every routing decision lands as its own row in a decisions table beside the attempt rows: one zstd-compressed record per
decision holding the request's facts, the settings in force, every pool member's standing at decision time, the rule
walk and the outcome, keyed by the request and the decision's place in it, so a hold and the placement after it are two
rows. The small trace on the attempt row stays for the analytics. The router emits the record as it emits its events,
and the ledger stores it.

A `ledger_gap` event names one of these reasons:

| Reason | Meaning |
| --- | --- |
| `attempt_missing` | The bus dropped an account's selection, so the trail is short an attempt row. |
| `capture_budget` | Past the 64 MiB capture budget; the largest capture, oldest among equals, gave up its bodies. |
| `chunk_dropped` | The completion counted more published chunks than arrived; the tail was dropped at the bus. |
| `chunk_skipped` | An interior chunk of a capture was dropped at the bus; the capture is truncated from there on. |
| `expired` | The completion never reached the ledger within the in-flight window; the live table shows it lost. |
| `in_flight_cap` | The 16,384 requests in flight was reached; the oldest entry was evicted to make room. |
| `poison` | A record failed alone after its batch's retries; a priced one is parked (`parked_priced`). |

The request row names the key, the pool, the session, the agent, and the model; the kind and transport of the call; the
status and outcome (`canceled`, `exhausted`, `failed`, `incomplete` for a 200 answer whose body ended before its end,
`rejected`, `succeeded`); the error code (`stream_cut`, `stream_incomplete` or `stream_stalled` under `incomplete`, and
`stream_cut` or `stream_stalled` under `failed` for a forwarded answer of another status whose body was cut or went
silent); the time to first byte and to first token; the duration; the bytes sent and received; whether the response was
served kept warm; whether its body was captured; and the client's user agent and class (`claude-code/<entrypoint>`
or `other`), the gateway hints it sent (the agent type, the compaction and context-compacted markers, and
the summed tool durations of the turn before), the turn origin off its attribution line, its auto-mode standing (as
admission read it, a server-side check asked for under an on setting or the `safeguards` member carried under an off
setting, which the dispatch drops; or a refusal for the body's shape before any upstream saw it), and the request's
shape: the effort, the thinking mode and budget, the output limit, the tool, system-block, and message counts, the
context edits, the temperature, and the requested speed and service tier.
A session that
branched from another conversation carries that conversation's root and the matched depth as well. The session it names
is the client's own,
and the Sessions view groups a row by it. The attempt row names
the account and its kind; the upstream status, request id and message id; the outcome; the cooldown the attempt set; the
stop reason, the model the answer names when it differs from the one the router resolved, the speed, the region
inference ran in, the tokens a context edit cleared, and a refusal's category and recommended model; the five token
counts, with the thinking share of the output and the web search and fetch calls the answer made; the cache miss reason
the diagnostics beta reports; the unified rate-limit window readings; overage; the cost split across input, cache read,
cache write, and output; and the pricing version the estimate used.

Placements are counted by rung, with a hit share from the trace and the response's cache-read tokens: a
placement by the conversation's own messages is a hit when the response read the prefix the entry promised; one
by the static prefix, by a subagent's fallback to its parent agent's pin, or, with every account at its cap, by
any request's landing on another agent's pin of its session, is a hit when it read at all.

A budgeted pool member's spend sums the priced hourly rollups over each period it carries a budget for, the current UTC
day and the current month, an API-key member's rows whole, a subscription member's metered rows alone (`overage`
and `fast`); a keepalive probe's cost counts toward it the same as any other request's, since a probe on an
API-key member still draws real money. The table in force is fixed at startup, so a member held at its budget
waits for the period to end or its budget to be raised, rather than for a price correction. The rollups file each hour's
traffic under who paid for it: the plan, overage, or a fast answer's usage credits, as the response reported it.

A model an upstream rejected leaves two events in the log: `model_rejected` names the refusing account and the
spelling the request carried, `model_permitted` the account that served the same request afterwards. The
[routing log](#routing-log) lists both.

Percentiles for every millisecond latency and for output tokens per second come from a fixed, log-spaced,
64-bucket histogram. A range spanning several hours merges its hourly histograms bucket by bucket before reading
the percentile, and the value carries the error of the bucket it falls in.

Every ledger read the admin routes, the usage page, and the metrics listener make runs on a blocking worker off
the request runtime, so a slow analytics query never delays proxy traffic.

## Quota samples

Quota samples arrive from two sources. Every response head from an OAuth account carries the unified rate-limit
headers, which cost nothing to read and cover whichever accounts are in use at the moment; a poll of the usage
endpoint, every five minutes per OAuth account with each account on its own phase across the interval, fills the gaps
an idle account leaves, is the only source of extra-usage credit totals, and hands every clean reading to the router.

A sample's window is one of the endpoint's keys: `five_hour`, `seven_day`, `seven_day_overage_included` (the head's
overage-included week), `seven_day_fable` (the `limits` window) and any other `seven_day_*` key the poll reports; the
metric's `window` label uses the same spelling, and the console draws the windows it knows under its own labels and any
other key as it is. A head and a poll of the same five-minute bucket write one row per window, the head's where both
read it.

The usage endpoint is also read once at startup for every OAuth account with a credential, before the proxy listener
accepts requests and bounded by `routing.startup_fetch_seconds`, so selection holds a sample per account from the first
request after a restart; an account whose first answer is a 429 serves from the windows kept in the registry from its
last run, predicted forward, and its backoff starts at once, and one whose first answer is a transport failure or
another error serves from them too and keeps the cadence. The poll counts that read as its own: it next reads each
account five minutes after the startup step, on the account's phase, or when its hold ends after a 429, and its profile
an hour after.

An account the endpoint answers 429 backs off alone. Its hold grows through two, four and eight minutes to fifteen, a
`Retry-After` honored up to the same fifteen, and the retry comes when the hold ends, sooner than the account's next
five-minute slot when the hold is shorter. Each 429 is logged as a warning naming the account and the endpoint that
answered it, and the next clean answer returns the account to the five minutes. A transport failure or another error
keeps the cadence. The other accounts keep theirs throughout.

## Pricing

Every attempt is stamped with a cost estimate at list prices per million tokens: an estimate rather than a bill,
since tier and batch modifiers are not known here and subscription quota is the rate-limit windows, never this
number. The shipped price table (`crates/ledger/api_prices.yaml`) is embedded in the binary;
`stats.api_prices_path` overrides it with a file of the operator's own, and Settings names each table's path and date
on its About card, and the cost reference shows the table. An attempt on a model the table does not price carries no
cost estimate and is counted in `phoebis_pricing_unknown_model_total`.

A model that serves fast mode carries a second set of five cells at the fast rates under the same date; an attempt
whose answer reports `speed: fast` is priced at them, in dollars, as spend from usage credits, and one on a model
without them at the standard cells.

A price file whose cell is not a finite non-negative number is refused at start and by `check-config`, naming the
model and the cell (`fast.<cell>` for a fast-mode cell): a negative or `NaN` price would turn every estimate negative
or `NaN`, which no budget trips on.

## Plan meter

A subscription account's plan-side figures read as a share of its own 5-hour window and of its 7-day window, a percent
with the window named; an API-key account, and a subscription account in overage, read dollars.

A fast answer draws no plan window: it adds nothing to the share, the prediction, or the burn a shedding round ranks by.
It reads as a zero share in Plan per hour, and its dollars count toward the burn route's `usd`, which Spend per minute
charts under every grouping; a member's budget counts them at the fast cells, a subscription member's as an
API-key member's.

The subscription cost model (`crates/ledger/subscription_costs.yaml`) ships beside the price table and holds constants:
a token costs its model's weight times its type's multiplier in units on every window, a window holds a capacity in
units per tier, and a request's share of a window is the units it spent over that capacity, over the peak band's
capacity factor when the request completes inside the band. A unit is 1/150,000,000 of the Max 20x 5-hour window outside
the band, so one percent of that window, the step the usage header moves in, is 1,500,000 units. Nothing the console
shows is a unit count.

The capacities the shipped file carries, in units per window, keyed by the tier the profile reports (a Pro subscription
reports `default_claude_ai`):

| Tier | 5-hour | 7-day |
| --- | --- | --- |
| `default_claude_ai` (Pro) | 7,500,000 | 90,000,000 |
| `default_claude_max_5x` | 37,500,000 | 450,000,000 |
| `default_claude_max_20x` | 150,000,000 | 900,000,000 |

The weights, in units per plain input token, with the share of a Max 20x window one million input tokens spend outside
the band, derived from the weight over the capacity:

| Model | Weight | 5-hour input per million on Max 20x | 7-day input per million on Max 20x |
| --- | --- | --- | --- |
| `claude-fable-5-1` | 10 | 6.67% | 1.11% |
| `claude-opus-5-5` | 2.2 | 1.47% | 0.244% |
| `claude-sonnet-5-5` | 1.5 | 1.00% | 0.167% |
| `claude-haiku-4-5` | 1 | 0.667% | 0.111% |

The models the plan meter has not measured (`claude-fable-5`, `claude-opus-5`, `claude-opus-4-8`, `claude-sonnet-5`)
have no row and price no share.

Every model shares one multiplier per token type: input 1, output 5, a cache write 1.25 whatever its TTL, and a cache
read 0.025. Thinking tokens count as output. A model that breaks a shared constant takes an `overrides` row, which may
name its own multiplier for a type or its own weight on a window. The shipped file has none.

**Peak band** On weekdays from 5 to 11 AM Pacific time, 12:00 to 18:00 UTC while daylight time holds and 13:00 to 19:00
UTC from 2026-11-01, a 5-hour window holds 0.7 of its capacity, so a request completing inside the band spends 1/0.7 of
the share it would outside. The 7-day window's factor is 1, so the week has no band. The edges are steps, and the band
is weekdays only. The meter charges a request when it completes, so the prediction weighs a request's 5-hour share by
the band in force at its completion, and a session's burn keeps a copy weighed by the band at each renewal, which the
shedding round prices on plan quota so the order reads what the meter charges.

The Fable-scoped week is a row of the file, `model_windows.seven_day_fable`, half the 7-day window on every tier, so a
Fable request's share of it is twice its 7-day share.

A model without a weight prices no share: the prediction adds no charge, the shedding round ranks the pin after every
priced one, the analytics list the share under model unlisted and the cost reference shows a dash. A tier without a
capacity on a window prices no share there either, and the prediction adds no charge on that window. The shedding round
ranks that tier's pins like any other, on the model's anchor rates, and in the analytics an account on a tier the file
lacks reads tier unknown, while a window the file leaves `null` on a listed tier reads model unlisted. The file carries
one date, `effective`. A dated model id reads the weight of its undated name.

A new model is one weight line: its plain input rate in percent of the Max 20x 5-hour window per million tokens,
measured outside the band, times 1.5. An operator who cannot wait for a release copies the shipped file, adds the line
and points `stats.subscription_costs_path` at the copy.

## Body capture

Body capture is off until a key or a pool asks for it. Once asked for, a capture holds the request and response
bodies of that key or pool in the ledger for the hours it names, each body cut at 8 MiB if the stream ran longer,
the row keeping the length it ran to so the drawer can say how much was dropped, and each captured body is dropped
once its hours run out. `stats.body_capture_max_hours` is the ceiling a key or pool may ask for, because the
bodies are prompts and how long they are kept is the operator's call rather than the caller's. The console shows a
captured body in the request drawer, with a control to copy it. A captured request body is the body as the
client sent it, so a session in auto mode shows its `safeguards` member whole, the machine's permission rules and paths
included, whether or not the router dropped it on the way upstream.

## The ledger writer

A batch the ledger cannot write is retried in place after 1, 2, 4 and 8 seconds, then one record at a time; a record the
file as a whole refuses (a corrupt, full, read-only or unopenable file, an I/O error) is held and retried every 10
seconds until it lands; a record that fails on its own is dropped with a `ledger_gap` when it carries no money and
parked in the ledger file when it does, and every parked record is tried again once at the next start or on the
dashboard's Replay, its parked row deleted in the transaction that lands the record, so its charge counts once. A parked
record keeps its request and attempt rows and not its captured bodies, so the capture of a parked request is gone while
its cost is kept. While a batch is under retry the ledger is degraded: the system page shows it,
`phoebis_ledger_degraded` reads 1, and routing treats every budgeted member's spend as unknown until a batch lands and
the spend is read again.

The routing state writer is bounded the same way: a stalled registry file leaves its writes in memory, coalesced per
row, and the system page's `router.writer` reads `degraded` until they drain. A batch the registry file as a whole
refuses is tried once more at the writer's next wake and counted under `router.writer.failed_batches`; a second
refusal drops it.

## Retention

| Data | Kept for |
| --- | --- |
| Requests, attempts, events, decisions, kept-warm marks | `stats.retention_days` (90 days by default; 0 forever) |
| Quota and extra-usage samples | `stats.retention_days` (90 days by default; 0 forever) |
| Resolved credential refresh attempts and registry changes | `stats.retention_days` (90 days by default; 0 forever) |
| Captured bodies | The hours the capture named |
| Hourly rollups | `stats.rollup_retention_days` (400 days; 0 forever, otherwise at least `stats.retention_days`) |

A routing decision leaves in the purge that takes its request; one whose request never landed leaves at the same age.

The purge that enforces these runs once an hour on its own connection, in slices of 1,000 rows per transaction, so the
writer never waits behind a whole pass and the day's cut costs the WAL one slice at a time. Every five-minute poll's
reading is kept for `stats.retention_days`, so an idle account adds 288 quota rows per window per day. The same pass
cuts the registry's history at the requests' cutoff, in slices of 1,000 rows: its resolved credential refresh attempts
and its change log. It keeps what is read whatever its age: an attempt whose outcome is still uncertain, each account's
newest two resolved attempts, which the attention line reads, and its newest attempt, which the account view reads, and
each key id's newest create and delete, which tell a re-created key from its predecessor.

The analytics, the dashboard, the pool cards and the burn chart read the hourly rollups in pages of a day, each page
its own read, so a 400-day range holds one day of rows in memory and the reader connection is free between pages.

## Metrics

`stats.metrics_listen` serves `/metrics` in Prometheus text format and nothing else; it speaks plain HTTP and
asks for no credential, so it belongs behind a network only the scraper can reach. Twenty-three families are
rendered:

| Metric | Type | Labels |
| --- | --- | --- |
| `phoebis_account_cooldown_seconds` | gauge | `account`, `scope` |
| `phoebis_account_overage_in_use` | gauge | `account` |
| `phoebis_account_ratelimit_utilization` | gauge | `account`, `window` |
| `phoebis_attempts_total` | counter | `account`, `model`, `outcome` |
| `phoebis_attention` | gauge | `kind`, `subject` |
| `phoebis_bus_dropped_total` | counter | `subscriber` |
| `phoebis_bus_subscriber_panics_total` | counter | `subscriber` |
| `phoebis_keepalive_agents` | gauge | `state` |
| `phoebis_keepalive_probes_total` | counter | `outcome`, `pool` |
| `phoebis_keepalive_skipped_total` | counter | `pool`, `state` |
| `phoebis_ledger_behind_refusals_total` | counter | none |
| `phoebis_ledger_degraded` | gauge | none |
| `phoebis_ledger_dropped_total` | counter | `table` |
| `phoebis_ledger_parked_priced_total` | counter | none |
| `phoebis_ledger_queued` | gauge | none |
| `phoebis_pins_active` | gauge | `pool` |
| `phoebis_pricing_unknown_model_total` | counter | none |
| `phoebis_requests_total` | counter | `key`, `kind`, `outcome` |
| `phoebis_router_dropped_reports_total` | counter | `kind` |
| `phoebis_router_writer_degraded` | gauge | none |
| `phoebis_router_writer_depth` | gauge | none |
| `phoebis_tokens_total` | counter | `account`, `kind`, `model` |
| `phoebis_upstream_ttfb_seconds` | histogram | `model` |

The `model` label is bounded by the price table: a model reads as the table entry that prices it, its own id or, for a
dated id priced under its undated base, that base; every other model is reported as `other`, so a downstream caller
cannot grow the label set by naming models it likes.
`phoebis_keepalive_probes_total` counts each probe under its pool and one of four outcomes: `landed` when it read the
whole prefix back, or wrote it on the account a migration moved it to, `cold` when the upstream served it without that
read, `failed` when the upstream did not serve it, and `held` when the router refused it with `503 ledger_behind` while
its ledger caught up, which the engine sends again.
`phoebis_keepalive_skipped_total` counts a due probe the engine withheld, under its pool and the recipe's state, one of
[the five states](keepalive.md#the-five-states): `held` or `paused` for a gate that held it, `ended` for an entry that
lapsed before it.
`phoebis_account_cooldown_seconds`, `phoebis_account_overage_in_use`, `phoebis_keepalive_agents`, and
`phoebis_pins_active` are the four routing-state gauges; each is computed fresh from the router's own snapshot on every
scrape rather than accumulated between them. `phoebis_pins_active` is counted per pool by the routing loop itself, so
a scrape copies one count per pool rather than the pin table. Beside the four, `phoebis_router_writer_depth` and
`phoebis_router_writer_degraded` are read at the scrape from the router's writer status: the batches waiting in the
routing state writer's channel, and 1 while writes past its bound wait in memory
([the ledger writer](#the-ledger-writer)).
`phoebis_ledger_queued` reads the records waiting in the ledger's writer channel at the scrape, and
`phoebis_ledger_behind_refusals_total` counts the requests answered `ledger_behind` while it was at least half full
([run the container](installation.md#run-the-container)).
`phoebis_ledger_degraded` reads 1 from a batch the ledger could not write until a batch lands and the spend is read
again, and `phoebis_ledger_parked_priced_total` counts the priced records parked since start
([the ledger writer](#the-ledger-writer)).

## The live feed

`GET /admin/live` upgrades to a WebSocket that reports requests while they run; `GET /admin/live/snapshot?at=`
(or `?from=` and `?to=`) answers the same shape for a past window, 30 minutes by default. Both need an admin
session, like every other admin route. After the upgrade the socket sends one `{"kind": "snapshot", ...}`
message and then a `{"kind": "delta", ...}` message for every change. The feed keeps 30 minutes or 2,048
finished requests, whichever bound is reached first; a request or response preview inside a message is at most
4 KiB; and a client that falls 1,024 messages behind the broadcast is disconnected rather than left to grow an
unbounded backlog. A past snapshot carries no preview. The socket closes `1008 session ended` once its session ends,
and at most 64 sockets are served at once, a further one refused `503 live feed at capacity` until one closes.

## The console's views

### Dashboard

A line above the tiles names what needs the operator's attention, worst first, each item a badge that opens the account,
pool or key where it is fixed, or nothing for an item with no such place (a drift refusal that named no key, a parked
record without a pool, a pool deleted since, one of the router's own writers); it reads the same conditions the
[`phoebis_attention` gauge](#conditions-and-gauges) exports. The pool exhausted item opens the
[routing log](#routing-log) instead, on the pool's exhausted decisions from the moment the condition began. Four tiles
report requests, tokens, estimated cost, and the error rate over the last 24 hours, and a line under them says when the
figures were read, or that the last refresh failed and why. Two cards below show how much of the pooled 5-hour and 7-day
capacity is left: each subscription account is weighed by its tier's capacity on the window from the subscription cost
model, in Max 20x windows, and listed with its own remaining share, and the line under the ring reads the windows left
against the pooled capacity, such as 0.39 of 1.30 Max 20x windows left. A window whose reset has passed with no reading
since counts as a full allowance. An account whose tier the model does not size, or whose tier has no capacity on the
window, is named under the ring instead, with the reason. The 5-hour ring's subtitle opens the cost reference on its
Tiers and peak band tab. The page ends with the most recent requests and a sessions card that leads with the router's
live pins and the accounts they sit on.

### Observe

The metric strip reports eight measures: requests per second, active streams, the error rate, TTFB at p50, p95,
and p99, output tokens per second, and the estimated cost per minute. Below it, a live table lists requests as
they run, with filters, a pause control, and a range control for switching to a past window; selecting a request
opens a detail pane with its error code and phase waterfall, and a Streams grid shows every stream still open. A stream
that fails after its 200 head keeps that status, so a finished request with an error code never reads as a success.

![The Observe view: the metric strip and the live table](assets/observe-light.png#only-light)
![The Observe view: the metric strip and the live table](assets/observe-dark.png#only-dark)

### Quota

The Quota view covers the subscription accounts alone, since an API-key account has no quota window. The header counts
them and those needing attention, and a section bar under it shows one of two sections at a time: Accounts, where the
view opens, and Quota windows. A plain `/quota` shows the accounts, and a link or a reload with `#quota-windows` lands
on the timeline.

Accounts draws a card per account, in the default order or by soonest recovery. Every card has the same height and the
same slots in the same order, so a card with more to say moves no rule against its neighbors: the header, one row per
rate-limit window (5-hour, 7-day and Fable), the Extra usage line, the chips and the poll reading. Every window share on
the view reads as the share left. A card draws the windows the ledger sampled in the last day, or the ones the router
holds when the ledger has none, whatever the last read said, and shows an error in their place only when it holds none.
A window whose reset has passed with no reading since reads as expired, with no share and an empty meter, until the next
reading. The Extra usage line reads the month's spend against its limit, in the serious tone while requests bill as
extra usage and in the muted ink while they do not, and reads off or not reported in the muted ink when extra usage is
disabled or the account has not reported it. The reason the provider gave for it sits inside the line, cut to the line's
width, with the whole message on hover. The chips start with the plan, named by its tier (Max 20x, Max 5x or Pro), then
say when the plan of an account marked cancelled ends, in the serious tone inside a week, and when the trial of an
account on the Claude Code trial ends. The poll reading takes one or two lines: when the poll last asked the endpoint
and how it answered, a failed poll naming its reason cut to the line's width, and, while the account backs off, the next
attempt in the serious tone. The line's title carries the rest: the attempt's instant, a failed poll's whole message,
when the snapshot shown was read, since when the account backs off, and how many rate-limited answers it has had in a
row. Refresh reads the endpoint now unless the account is backing off or a fresh snapshot is cached. The page counts a
card that shows an error, whose last poll failed, or that is backing off as needing attention.

Quota windows lays the same windows across a calendar, zoomable between a two-week weekly view and a three-day 5-hour
view. An account marked cancelled with a saved end shows the end on its lane where it falls inside the drawn period: no
window opens after it, and the window that spans it stops there.

The fast-mode reading is on the account page's Usage card: a Fast mode line with the share of the fast lane's input and
output buckets used, as the last answer reported them, with the reset, a bucket past its reset reading as refilled, and
a chip while the account's fast lane cools, naming the reason (rate limited, or credits unavailable) and when it lifts.

![The Quota view's Accounts section: the cards per account](assets/quota-light.png#only-light)
![The Quota view's Accounts section: the cards per account](assets/quota-dark.png#only-dark)
![The Quota view's Quota windows section: the reset timeline](assets/quota-windows-light.png#only-light)
![The Quota view's Quota windows section: the reset timeline](assets/quota-windows-dark.png#only-dark)

### Models

The Models view lists a key's processed rules with their level and the accounts cooling for a matching model, every
model-scoped cooldown grouped by model with a clear button per row, and a link to the routing log's model rejected and
model permitted rows ([routing](routing.md#model-rules)).

### Requests

The table offers thirteen columns, ten shown by default (pool, kind and transport start hidden, since the drawer
names all three, and the Columns control turns any on): time, key, pool, account, model, client, kind,
transport, status, attempts, TTFB, tokens, and cost. A request whose 200 answer ended short reads `incomplete` with the
code that says how: `stream_cut` when the connection or the decoder failed mid-body, `stream_incomplete` when the stream
closed without its `message_stop`, `stream_stalled` when the upstream went silent past the idle bound. The model column
reads the family and version, with the whole id on hover. The client column shows the class the router derived from the
user agent (`claude-code/<entrypoint>` or `other`), cut at 12 characters, whole on hover, and the agent
type beneath it on a subagent's turn.
One row is one client request however many attempts it took, and the attempt trail reads as
"secondary 429, primary 200" when a 429 on the first account failed over to a 200 on the second. A request that did not
succeed names how it ended beside its status: `canceled`, `exhausted`, `failed`, `incomplete`, or `rejected`. A request
that waited for a slot while every account was at its cap carries a held marker beside its attempts with the time it
waited. The search matches the request id, the session, the key, the model, the error code, and an attempt's account,
upstream request id or upstream message id, literally: `_` and `%` are characters, not wildcards. Filters narrow the
table: the user agent filter takes a pattern (`claude-cli/2.1.*`, `*sdk-cli*`; `*` for any run of characters, `?` for
one, a backslash quoting either, and a value with no wildcard matching in full), the client filter takes a pattern over
the class (`claude-code/*`), the agent type and turn origin filters pick from the values the client's hint headers and
attribution line carry, the speed filter picks `fast` or `standard` by the speed the answer reported, else by whether
the request asked for `fast`, the status filter offers the codes the router and Anthropic answer and any other code the
page holds, the outcome filter picks how the request ended (`canceled`, `exhausted`, `failed`, `incomplete`, `rejected`,
or `succeeded`), and the held toggle keeps the requests that waited for a slot. The search and the two pattern fields
apply 400 milliseconds after the last keystroke, on Enter, or when the field loses focus. Every filter, the range and
the page size live in the page's address, so a reload or a shared link keeps them. A filter whose list did not load is
disabled and says why. Selecting a row opens a drawer with the request's detail, which shows the outcome beneath the
status, the user agent, the model as the client spelled it and, when
it differs, the canonical id the router recorded, its auto-mode standing (asked for a server-side check under an on
setting; carried the safeguards member under an off setting, which the dispatch drops; refused for its body shape; or
not requested), and the client hints: the agent type, the compaction and context-compacted markers, the turn origin, and
the tool time before the turn. The drawer's Request section names the upstream's request id for each attempt and, on the
answer that served, the message id a Claude Code transcript's assistant entry carries. The drawer's effort is the level
the upstream applies: when the client sends a per-turn control entry, the last entry's level, else the request's
top-level `output_config.effort`; a turn that ends with such an entry still counts as the user's turn, so it starts its
own prompt. The table holds the requests up to the moment you opened it, so paging does not shift as new ones arrive;
Refresh brings it up to now. The drawer opens on the answer's facts: the status, the outcome, the serving account and
model, the answer's stop reason and region, and the cache miss reason. The usage follows, then the request's identity
with the upstream request and message ids under it, its shape, the client hints and the captured bodies. Its head
carries a "Routing log" link and each row a "Routing" link beside "Details", both opening the
[routing log](#routing-log) on that request with a quarter hour either side. While the drawer is open the table shows a
compact set of columns (time, model, status and attempts), and the chosen set returns when it closes. The drawer shows
the captured request and response bodies, each with Copy, which copies the text as shown; a body past 256 KiB shows
compact until Pretty-print is pressed.

![The Requests view: the filterable table with its client column](assets/requests-light.png#only-light)
![The Requests view: the filterable table with its client column](assets/requests-dark.png#only-dark)

### Routing log

The Routing log sits under the Diagnostics heading at the foot of the navigation, above the Expert view switch. The
heading shows while the expert view is on or while the log is open, and every link into the log opens it whatever the
mode, without turning the expert view on.

The log is one stream of routing decisions and the state changes that explain them, newest first, 25 rows a page with
First, Previous and Next. A row reads the time, with a decision's request beneath it as a link to the request's drawer,
the kind (a decision in a filled badge with `attempt 2/3` beneath a failover attempt, an event in an outlined one), the
session's first eight characters with the agent beneath, the account (a move names the account it left and the one it
landed on, a pool or key event names its pool or key), the model, By and Fact, and Details, which opens the request's
drawer. By names what decided the row in one chip: the rule that fired (`spent rule`), the rung that placed it (`static
rung`), the strategy and the tier's priority (`Pace P0`), where a hold waited (`cohort`, `session pin`), what exhausted
the pool (`budget`, `cooldown`, `fast lane`), the refusal (`client drift`, `key revoked`), `no rule fired` for a
renewal, or an event's reason (`window spent`, `429 on 5h`). Fact gives the value that made it so, one labeled value or
two stacked: a muted label such as `gap`, `idle`, `prefix` or `until` and the value after it, never a time the row's
own time already gives. Every account, key, pool, model, session and request a row names is a link, and a column with
nothing to say shows a dash. Below a window width of 1420 pixels the model moves beneath the account.

The filter bar reads the range (the last hour by default, 3, 6 or 24 hours, or a custom range), the kinds (decisions,
then events), keys, pools, accounts, models, a session id or its prefix, and a request id. Every filter lives in the
page's address, so a filtered view is a link, a reload keeps it and Back returns to the view before it: `from` and `to`
as ISO instants (`2026-09-16T14:15:00.000Z`), `accounts`, `keys`, `kinds`, `models` and `pools` as comma lists, and
`session` and `request`. A `kinds` value replaces the default set, so `kinds=probe` lists probe decisions alone. A
kind this build does not know stays in the address, and the route's refusal shows where the rows would be. A filter
change returns to the first page, and Refresh reads up to now.

A request filter opens a line above the table that counts the request's decisions and events and offers three
widenings: this session (an hour either side of the request's time), this account (a quarter hour either side) and this
minute (the clock minute that holds it). Any other narrowing draws the same line with what the rows are narrowed to,
the range and the page's count. Clear returns to the unfiltered view.

Follow, off by default, reads the list every 5 seconds with the range ending now. Each read takes every row that
arrived since the read before, up to eight pages of them. When more arrived than that, a line above the list reads
"More rows arrived than Follow could fetch. Reload to read them.", and Next reads on from the last row Follow fetched.
A new row lands at the top in the accent tint until the next read, a filter change keeps Follow on, and paging to an
older page turns it off.

Probe decisions and warm prefix expiries are hidden by default. The Kinds filter lists them last in their groups, and
its button reads "Kinds (2 hidden)" until another set is chosen.

Every row opens. A click anywhere on a row but a link opens it beneath itself on a tint and a second click shuts it,
and another page or another filter starts with every row shut. A decision row loads its record and draws its card, a
probe decision's included. A card opened on a request that is still running reads its record again when Follow brings
the answer. The verdict line names the decision with its accounts and its pool as links ("Moved
secondary to primary", "Attempt 2 of 2: placed on primary", "Exhausted in jobs") and chips for the tier, the rule or
the rung, the pin's warmth and the cache verdict once the attempt reports usage (`cache pending` before). "How the
router decided" numbers the walk in the engine's order: the pin, every member's eligibility, the move rules, the cool
reading and the cap filter after a fired rule, the renewal, the three rungs, the partitions, the pick, and a hold or an
exhaustion. Each line reads the step, its verdict in a badge and the facts the engine read as chips, one value or one
key-value pair each, a member's fact keyed by the member. The rules after a fired one never run, so they fold into one
line, "Rules after spent". A rung line adds the entry the rung took, or the deepest one a gate refused: the boundary's
hash as its first eight characters with the whole hash and any refusing gate on hover, and `warm until 14:58` from the
entry's write and its TTL, which an entry the cache never confirmed leaves out. A refusal's walk is two lines, the
admission with its reason and the rest of the walk not run.

The candidate table lists every pool member in the walk's order with the chosen or held-on member tinted: the account
with its plan beneath, the tier (`plan P0`, `overage P0`, `keys P1`), the 5-hour, 7-day and Fable windows as three lines
each (the utilization, the reset, the reading's source and age), or an API key's one merged reading, the requests
in flight against the cap, the Pace score, the prefix entry, and the verdict with the one chip that decided it (`429 on
5h until 15:42`, `budget $1.10 of $5.00`), whose hover says why the member was passed over. Three folds, closed until
opened, hold the request's facts, the pin and its warmth (a record that pins nothing reads why: not looked
up or pinning off) and the twenty settings in force.
A refusal under a key or pool the registry no longer holds says so in its place. The outcome
lists the pin written, the cache-map entries the answer wrote (`pending` until it ends), the cache read with the TTL
tier of its deepest entry, the hold, the answer an exhaustion or a refusal gave, and the spend against a budget. The
foot repeats the three widenings for the row, names the record's version (Record v1), and Copy as JSON copies the
record route's answer whole, every field the card does not draw included. A record the retention purged reads "The
record left with the retention" in the card's place.

An event row opens to a bordered panel of pairs: When to the second in UTC, the account and the pool as links, then the
kind's own pairs. An event that names a request ends with Source request, which opens the log on that request a quarter
hour either side of the event, and Details, which opens the request's drawer. The kinds and what each row records:

| Event | What it records |
| --- | --- |
| account state changed | An account's state moved, with the old state, the new one and who moved it. |
| budget period ended | A member's budget period closed, with the spend it closed on. |
| budget reached | A member's spend crossed its budget, so the pool answers exhausted for it until the period ends. |
| catalog changed | An account's model catalog gained or lost models. |
| cooldown cleared | A cooldown ended by its deadline, by hand or by a recovery. |
| cooldown set | An answer cooled an account, its fast lane or one model on it until a time. |
| fast credits changed | An account's usage credits for fast mode became available or unavailable. |
| ledger gap | The ledger could not record a request whole, and the reason names what it is missing. |
| model permitted | An account served a model another account had rejected. |
| model rejected | An account answered 404 for a model, which cools that model on the account. |
| overage ended | An account left overage at its 7-day reset. |
| overage started | An account went into overage, so its requests draw on extra usage. |
| pin created | A placement pinned a session to an account. |
| pin expired | A pin was released, idle past the pin window or for the reasons it lists. |
| pin moved | A rule or a failover moved a session's pin to another account. |
| pin released | An operator released a session's pin by hand. |
| poll backoff ended | The usage poll answered cleanly again after backing off. |
| poll backoff started | The usage poll was rate limited, so decisions read the account's windows from response heads. |
| re-auth required | An account's refresh was refused, and routing to it stays off until it is paired again. |
| refresh failed | An access-token refresh failed. |
| refresh succeeded | An access-token refresh succeeded. |
| refresh uncertain | An access-token refresh ended without a clear answer. |
| registry changed | An operator edited a pool, a key or an account, field by field where the change names them. |
| router started | The router started and loaded its pins, shedding marks, window readings and affinity entries. |
| shedding round | A round marked pins on an account ahead of pace and kept the marks across a restart. |
| warm prefix expired | A warm prefix's cache-map entry lapsed past its TTL, listed only when the kind is chosen. |
| wind-down started | An account's subscription end entered the pool's wind-down, so no fresh session starts on it. |
| window reset | A quota window reset, with the reading before it. |
| window spent | A response head or the poll read a quota window at its limit. |

An account state change names who moved the state in the console's words: `credential refused` when the upstream refused
the account's credential, `credit balance` when it named the credit balance, `refresh failed` when the token refresh
failed for good, `by hand` for an operator's change, and `paired` for a pairing's confirm. A deleted account's last row
reads `deleted`, as its new state and as who moved it.

Other views open the log on what they show. A Requests row and its drawer open it on the request a quarter hour either
side. The Sessions row menu's "View in routing log" opens the session's decisions an hour either side of its last
request, ending no later than now, or over the last hour for a session with no request. On a pool card, the "Moves in
the last hour" tile's figure opens the pool's moved decisions over the last hour. The figure counts the rule moves on
the pool's member accounts in the last hour (`drain`, `overage`, `return_to_plan`, `shed` and `spent`), a keepalive
probe's move included, which the log lists only with probe decisions chosen. Its title lists the moves by reason with
the failovers apart, since the log draws a failover as a later attempt's placement. The card's pool exhausted row and
the dashboard's pool exhausted item open the pool's exhausted decisions from the moment the condition began. The Routing
log action on an account's Requests card shows the account's rows over the last hour, and the Models view's link opens
the model rejected and model permitted rows of the last 24 hours.

![The Routing log view: the merged stream with the renewal's decision card
open](assets/routing-log-light.png#only-light)
![The Routing log view: the merged stream with the renewal's decision card
open](assets/routing-log-dark.png#only-dark)

### Analytics

A range control offers 1 hour, 24 hours, 7 days, 30 days, or a custom span, and a group-by control regroups every chart
and table by account, client, effort, key, kind, model, pool, request class, speed, or user agent; the client grouping
reads the class and the user agent grouping the agent's first product token (`claude-cli/2.1.286` for
`claude-cli/2.1.286 (external, cli)`). The hourly rollups key these on a bounded vocabulary: the effort is one of `low`,
`medium`, `high`, `xhigh`, `max` or `none`, else `other`; the request class and the client fold to `other` past 64
bytes; the user agent is the header's first product token (`name/version`), cut to 64 bytes, else `other`. The request
row keeps the raw values, so the drawer shows what the client sent. The request class shown by this grouping is the
value the client sent on its class header, so a class a client adds later shows up under its own name; a classless
request folds under the reserved value alongside a keepalive credit, and a request that sent no user agent under the
reserved id. The speed grouping reads the speed the answer reported, else `fast` when the request asked for it,
`standard` otherwise; the Requests by speed chart therefore counts a fast request the router refused before any attempt
as fast. A key deleted and created again under the same id appears twice under the key grouping, since its figures stay
with the key that spent them. The earlier holder is labeled `<id> (deleted, created 2026-09-14 09:31 UTC)`, the date
and time it was created at minute precision, or `<id> (deleted)` when the ledger holds no creation time for it. The
client and user agent filters take the request log's pattern grammar; the analytics agent filter matches the folded
token while the request log's matches the raw header, so a pattern over the part after the token (`*sdk-cli*`) selects
nothing here. Under the user agent grouping a pattern list names the groups: `claude-cli/2.1.*` and `claude-cli/2.0.*`
as two groups, every other agent under `other`.

Traffic reports request volume by outcome (the chart's OK, Rate limited, Failed, Incomplete and Exhausted, and the
per-group table's Rejected beside them) and the error rate, with a Failovers chart and a Migration rewrites card beside
it: the cache-write tokens the first request after a move paid to warm its new account, and the moves the range counted,
by reason; then a Requests by speed chart, standard and fast per bucket, and an Effort levels card with a bar per level
the requests asked for. Burn reports a subscription account's spend as the share of its own 5-hour and 7-day window and
an API-key account's in dollars, side by side and each split into cached and uncached tokens, grouped by account,
key, request class, or session; the 5-hour share carries the peak band's weight, the 7-day share the week's own band
factor, 1 in the shipped model. The shares read under the account grouping alone, since shares of different accounts'
windows do not add, and the Spend per minute chart draws dollars alone, a subscription account's among them only for its
fast-mode answers; past the seven lines a chart draws, the subscription accounts folded into the rest draw their
fast-mode dollars as Other (fast mode), or Other (fast mode, tier unknown) for those whose tier the cost model does not
size. The account, key and request class groupings read the hourly rollups, which fold a request under the hour it
arrived in and weigh it by that hour's band rather than at its completion, where the meter charges it. Grouped by
account, it adds a Plan per hour chart: the share of each subscription account's 5-hour window, solid, and of its 7-day
window, dashed, that its traffic used per hour, a bucket the cost model cannot price (a model it has no weight for, or a
tier it has no capacity for on that window) listed under one model unlisted legend entry and drawn as a gap, a priced
hour with a gap or the range's edge on both sides marked with a dot, an hour with no traffic counted as zero on a window
the account priced in the range, a window with no priced bucket shown as a dash, and an account whose tier the cost
model does not size listed as tier unknown. The chart shades the hours inside the peak band behind the 5-hour lines,
none when the browser cannot read the band's zone, and its note opens the cost reference on the same Tiers and peak band
tab. Latency reports percentiles read from the merged per-hour histograms; Cache reports the hit ratio, tokens that
expired between turns, the share of input served from cache, and the output split into thinking and plain output; Cost
reports the estimated spend for the range and a Fast mode spend tile, the dollars fast-mode answers drew from usage
credits; Cache warming reports what probes cost, the requests they kept warm and the probes wasted, a fast-mode
session's under speed `fast`; and Accounts reports peak utilization, ineligible time, and pin churn per account.

The Cache section closes with Placements by rung: the attempts in the range the router placed by a warm prefix or beside
the main conversation rather than by its strategy, counted by rung (by the conversation's own messages, by its static
prefix, or with the main conversation), each with its hits, hit share, the tokens the responses read from cache, and the
prefix sizes the placements promised. A placement by the conversation's own messages is a hit when the response read the
whole prefix it was promised; one by the static prefix or beside the main conversation is a hit when the response read
from cache at all, since the latter promises no size. The table reads the range and the key and pool filters, and a
session view leaves it out. The Burn section closes with Member budgets: one row per budget a pool member carries, a
subscription member's spend being its metered spend, its spend in that day or month against the budget with a bar, and
when the period ends. A member at its budget is marked, since routing skips it until the period ends; one the ledger has
not reported on yet reads "not reported". The range and the filters do not apply to it.

Cooldown time books to the hour it elapses in as the interval runs, and a cooldown cleared early gives back only the
hours it never held. A cooldown is booked up to seven days ahead of the announcement that set it, and a later
announcement books the next stretch. Filtered to one session, the view sums the request log instead of the rollups, and
the cost split, the cache write and read costs, and the Cache warming figures read as unavailable because the request
log does not carry them; the Accounts section keeps reading the rollups for the range, since they carry no session, and
its subtitle says so.

![The Analytics view: range and group-by controls over seven tabs, Traffic shown](assets/analytics-light.png#only-light)
![The Analytics view: range and group-by controls over seven tabs, Traffic shown](assets/analytics-dark.png#only-dark)

### Settings

Settings opens on the downstream client configuration, the six-line environment block that points Claude Code at the
router with a reason under each line. Its base URL is a placeholder for the URL that reaches the proxy listener, and the
address the listener binds shows beside the block. Then come the appearance and time-format choices, and an About card
with the version, the profile, the metrics listener, the four storage paths (the price table's and the cost model's each
with its effective date) and the admin secret's standing. About closes on a pointer: the values the configuration file
sets are on the [installation page](installation.md), and `phoebis check-config` prints them.

## Conditions and gauges

The router exports what needs an operator as Prometheus gauges and shows the same conditions on the dashboard's
attention line and the pool cards, which read `GET /admin/attention` ([API](api.md#get-adminattention)), worst first
and the oldest first within a kind. `phoebis_attention` carries the labels `kind` and `subject`, reads 1 while the cause
holds and 0 once it clears, and has a series for every subject the kind applies to, so a cleared condition reads 0
rather than vanishing; a deleted account or pool loses its series at the next read, except that a pool a parked record
names keeps a holding `ledger_record_parked` series after it is deleted, and the `-` subject's and a deleted pool's
`ledger_record_parked` series leave when their rows go. A restart starts the conditions fed by the request bus over,
and the usage poll keeps its backoff in memory as well, so `poll_backoff` reads 0 until a new backoff run is thirty
minutes old; the conditions read from the registry, and `ledger_record_parked`, read from the ledger file, hold again
at the first read.

| Kind | Subject | Clears when |
| --- | --- | --- |
| `pool_exhausted` | the pool | a request of the pool succeeds |
| `client_drift` | the client version | an inference request from that version is relayed, or a re-pinned build starts |
| `writer_degraded` | the ledger | a batch lands and the spend refresh after it succeeds |
| `writer_degraded` | the routing state writer | the backlog drains |
| `certificate_reload_failed` | the listener key | a reload lands or the router restarts |
| `reauth_required` | the account | the account is paired again, or an API-key account gets a new secret |
| `funding_required` | the API-key account | the operator clears funding |
| `refresh_failed` | the OAuth account | a refresh commits, or a newer credential supersedes a pending one |
| `certificate_expiring` | the listener key | a pair with more than 30 days left is loaded |
| `ledger_record_parked` | the pool, or `-` for a record without one | the next start or the Replay button lands them |
| `poll_backoff` | the polled account | a poll answers clean |

**`pool_exhausted`** holds while a request of the pool has ended exhausted and none of the pool's requests has
succeeded since; it is fed by the request bus. An `incomplete` answer neither raises nor clears it. The detail names the
pool and the key of the request that raised it.

**`client_drift`** holds while a request from that client version has been refused for drift since the process started
and no inference request from it has been relayed since ([re-pinning](operations.md#re-pinning-the-client-build)); it
is fed by the request bus. A token count or a keepalive request clears nothing: a count carries no stamp for the check
to read, and a keepalive request replays a body the router kept rather than the client's next one. The subject is the
version the client's user agent names when it reads as a release, one to four runs of digits joined by dots in at most
32 bytes; any other value is filed under `other`, and a request that named none under `unknown`. At most sixteen
versions are kept: a new one takes the place of the cleared version whose run started first, or of the one that has
held longest when every version holds. The detail names the version, what drifted (`hash` or `version_suffix`) and the
key of the newest refusal, which the item opens; a refusal that named no key opens nothing.

**`writer_degraded`** holds for the ledger from a batch it could not write, through the batch's retries and the hold of
a record the file as a whole refuses, until a batch lands and the spend refresh after it succeeds; it holds for routing
while the routing loop keeps state writes in memory because the registry file is not taking them
([the ledger writer](#the-ledger-writer)). The subject is the component, `ledger` or `routing`, which the detail names;
neither writer records when it degraded, so the item dates from the first read of this process that saw it, and it opens
nothing. While the ledger's holds, routing treats every budgeted member's spend as unknown
([routing](routing.md#pools)).

**`certificate_reload_failed`** holds from a `phoebis tls reload` whose pair the loader refused until a later reload
lands or the router restarts, and the listener keeps serving the pair it had
([TLS on the listeners](installation.md#tls-on-the-listeners)). The subject is the listener key, `proxy.tls` or
`admin.tls`, and the item opens nothing. It dates from the refused reload, and the detail names the two files, the
loader's line and when the pair still served was loaded.

**`reauth_required`** holds while the account's state is `reauth_required`, read from the registry
([account states](pairing.md#account-states)). The registry records no time for a state change, so the item dates from
the first read of this process that saw the state. The detail names the account and its label.

**`funding_required`** holds while the account's state is `funding_required`, set when the upstream refuses a request
over the account's credit balance; it is read from the registry and dated and detailed as `reauth_required` is.

**`refresh_failed`** holds while the account's newest two resolved credential refreshes both failed or went uncertain,
a run read from the registry. A refresh that commits ends the run, and so does one a newer credential superseded, as a
re-pairing settles a refresh still pending; a pairing records its own committed attempt, so it ends the run even when
nothing was pending. The detail counts the run's failures and gives the newest attempt's outcome and error.

**`certificate_expiring`** holds while the leaf a listener serves is within 30 days of its end, past the end included.
It reads the pair the router loaded rather than the files, so a pair replaced on disk counts once `phoebis tls reload`
loads it. The subject is the listener key, and the item opens nothing. It dates from 30 days before the leaf's end, so a
restart keeps the date, and the detail names the certificate file, the end and the whole days left, negative once past.

**`ledger_record_parked`** holds while the ledger file keeps priced records that failed to land on their own; each is
tried again once at the next start or on the dashboard's Replay, and the condition clears when the table empties. The
subject is the pool the record was served under, or `-` for a record without one; a pool deleted since shows under its
id with nothing to open. The item dates from the oldest row, and the detail counts the rows and names the accounts.

**`poll_backoff`** holds once the usage poll has been backing off from the endpoint's 429 answers on the account for
thirty minutes, counted from the run's first 429. The detail gives when the run started, when the next attempt comes,
and how many 429s the endpoint answered in a row.

A threshold is a Prometheus rule over the gauge:

```yaml title="prometheus rules"
groups:
  - name: phoebis
    rules:
      - alert: PhoebisNeedsAttention
        expr: phoebis_attention == 1
        for: 10m
        labels: { severity: page }
        annotations: { summary: "{{ $labels.kind }} on {{ $labels.subject }}" }
```
