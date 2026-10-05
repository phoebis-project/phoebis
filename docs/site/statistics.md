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
day and the current month, an Anthropic-key member's rows whole, a subscription member's metered rows alone (`overage`
and `fast`); a keepalive probe's cost counts toward it the same as any other request's, since a probe on an
Anthropic-key member still draws real money. The table in force is fixed at startup, so a member held at its budget
waits for the period to end or its budget to be raised, rather than for a price correction. The rollups file each hour's
traffic under who paid for it: the plan, overage, or a fast answer's usage credits, as the response reported it.

A model an upstream rejected leaves two events in the log: `model_rejected` names the refusing account and the
spelling the request carried, `model_permitted` the account that served the same request afterwards; the request
drawer lists both.

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
metric's `window` label and the console use the same spelling. A head and a poll of the same five-minute bucket write
one row per window, the head's where both read it.

The usage endpoint is also read once at startup for every OAuth account with a credential, before the proxy listener
accepts requests and bounded by `routing.startup_fetch_seconds`, so selection holds a sample per account from the first
request after a restart; an account whose first answer is a 429 serves from the windows kept in the registry from its
last run, predicted forward, and its backoff starts at once, and one whose first answer is a transport failure or
another error serves from them too and keeps the cadence. The poll counts that read as its own: it next reads each
account five minutes after the startup step, on the account's phase, and its profile an hour after.

An account the endpoint answers 429 backs off alone. Its hold grows through two, four and eight minutes to fifteen, a
`Retry-After` honoured up to the same fifteen, and the retry comes when the hold ends, sooner than the account's next
five-minute slot when the hold is shorter. Each 429 is logged as a warning naming the account and the endpoint that
answered it, and the next clean answer returns the account to the five minutes. A transport failure or another error
keeps the cadence. The other accounts keep theirs throughout.

## Pricing

Every attempt is stamped with a cost estimate at list prices per million tokens: an estimate rather than a bill,
since tier and batch modifiers are not known here and subscription quota is the rate-limit windows, never this
number. The shipped price table is embedded in the binary and dated 2026-09-30; `stats.pricing_path` overrides it
with a file of the operator's own, and Settings shows the table in force with its version and its source. An
attempt on a model the table does not price carries no cost estimate and is counted in
`phoebis_pricing_unknown_model_total`.

A model that serves fast mode carries a second set of five cells at the fast rates under the same date; an attempt
whose answer reports `speed: fast` is priced at them, in dollars, as spend from usage credits, and one on a model
without them at the standard cells.

A price file whose cell is not a finite non-negative number is refused at start and by `check-config`, naming the
model and the cell (`fast.<cell>` for a fast-mode cell): a negative or `NaN` price would turn every estimate negative
or `NaN`, which no budget trips on.

## Plan meter

A subscription account's plan-side figures read as a share of its own 5-hour window and of its 7-day window, a percent
with the window named; an Anthropic-key account, and a subscription account in overage, read dollars.

A fast answer draws no plan window: it adds nothing to the share, the prediction, the burn a shedding round ranks by, or
the reconciliation. It reads as a zero share in Plan per hour, and its dollars count toward the burn route's `usd`,
which Spend per minute charts under every grouping; a member's budget counts them at the fast cells, a subscription
member's as an Anthropic-key member's.

The cost table (`crates/ledger/cost_table.yaml`) ships beside the price table: per window, model and token type, the
share of a Max 20x subscription's window that one million tokens consume, a number where the plan meter measured the
cell and `null` where it did not. No reader prices a share through an unmeasured cell: the prediction adds no charge
for it, the shedding round ranks the pin after every priced one, a plan-funded probe settles in dollars, the analytics
list the share under model unlisted and the cost reference shows a dash. The table carries one date, `effective`; a
dated model id reads the cells of its undated name. The 5-hour input and output cells, as a percent of the window per
million tokens:

| Model | Input | Output |
| --- | --- | --- |
| `claude-fable-5` | - | - |
| `claude-fable-5-1` | 6.65% | 32.9% |
| `claude-haiku-4-5` | 0.659% | 3.29% |
| `claude-opus-5` | - | - |
| `claude-opus-5-5` | 1.46% | 7.28% |
| `claude-sonnet-5` | - | - |
| `claude-sonnet-5-5` | 0.999% | 4.98% |

Every 7-day cell is unmeasured until the 7-day window is read per model. The peak band is one measured factor on the
5-hour cells, applied at a request's completion; the 7-day cells are unbanded.

The same file holds each subscription tier's capacity of each window over Max 20x's under `tiers`, so a share of a
Max 20x window over the tier's factor is that share of the account's own window:

| Tier | 5-hour | 7-day |
| --- | --- | --- |
| `default_claude_max_20x` | 1, the reference | 1 |
| `default_claude_max_5x` | 0.25 | - |
| `default_claude_pro` | 0.05 | - |

The Fable-scoped week is half the 7-day window on every tier, so a Fable request's share of it is twice its 7-day share.
An account whose tier the table does not list, or whose factor on a window is unmeasured, is priced at no share on that
window.

**Peak band** On weekdays from 5 to 11 AM Pacific time, 12:00 to 18:00 UTC while daylight time holds and 13:00 to 19:00
UTC from 2026-11-01, the 5-hour meter charges a request 1.435 times as much, measured on Max 20x on 2026-09-30 and the
same for every token type; whether the 7-day meter has a band is unmeasured, so the row weighs the 5-hour window alone.
The edges are steps, and the band is weekdays only. The meter charges a request when it completes, so the prediction
weighs a request's 5-hour share by the band in force at its completion, and a session's burn keeps a copy weighed by the
band at each renewal, which the shedding round prices on plan quota so the order reads what the meter charges.

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
parked in the ledger file when it does, and every parked record is tried again once at the next start. A parked record
keeps its request and attempt rows and not its captured bodies, so the capture of a parked request is gone while its
cost is kept. While a batch is under retry the ledger is degraded: the system page shows it, `phoebis_ledger_degraded`
reads 1, and routing treats every budgeted member's spend as unknown until a batch lands and the spend is read again.

The routing state writer is bounded the same way: a stalled registry file leaves its writes in memory, coalesced per
row, and the system page's `router.writer` reads `degraded` until they drain. A batch the registry file as a whole
refuses is tried once more at the writer's next wake and counted under `router.writer.failed_batches`; a second
refusal drops it.

## Retention

| Data | Kept for |
| --- | --- |
| Requests, attempts, events, kept-warm marks | `stats.retention_days` (90 days by default; 0 forever) |
| Quota and extra-usage samples | `stats.retention_days` (90 days by default; 0 forever) |
| Resolved credential refresh attempts and registry changes | `stats.retention_days` (90 days by default; 0 forever) |
| Captured bodies | The hours the capture named |
| Hourly rollups | `stats.rollup_retention_days` (400 days; 0 forever, otherwise at least `stats.retention_days`) |

The purge that enforces these runs once an hour on its own connection, in slices of 1,000 rows per transaction, so
the writer never waits behind a whole pass and the day's cut costs the WAL one slice at a time. Every five-minute
poll's reading is kept for `stats.retention_days`, so an idle account adds 288 quota rows per window per day. A parked
record keeps its request and attempt rows and not its captured bodies, so the capture of a parked request is gone while
its cost is kept. The same pass cuts the registry's history at the requests' cutoff, in slices of 1,000 rows: its
resolved credential refresh attempts and its change log. It keeps what is read whatever its age: an attempt whose
outcome is still uncertain, each account's newest two resolved attempts, which the attention line reads, and its
newest attempt, which the account view reads, and each key id's newest create and delete, which tell a re-created key
from its predecessor.

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
| `phoebis_keepalive_probes_total` | counter | `account`, `outcome` |
| `phoebis_keepalive_saved_usd_total` | counter | `pool` |
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
`phoebis_keepalive_probes_total`'s `account` label reads `-` for a probe cancelled before any account was
attributed, the same reserved id a request row with no account reads under.
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
pool or key where it is fixed, or nothing for an item with no such place (one of the router's own writers, a parked
record without a pool, a pool deleted since, a drift refusal that named no key); it reads the same conditions the
[`phoebis_attention` gauge](#conditions-and-gauges) exports. Four tiles report requests, tokens, estimated cost, and the
error rate over the last 24 hours, and a line under them says when the figures were read, or that the last refresh
failed and why. Two cards below show how much of the pooled 5-hour and 7-day capacity is left: each subscription account
is weighed by its tier's capacity in Max 20x windows from the cost table and listed with its own remaining share, and
the line under the ring reads the windows left against the pooled capacity, such as 0.39 of 1.30 Max 20x windows left. A
window whose reset has passed with no reading since counts as a full allowance. An account whose tier the table does not
size, or whose window the table has not measured, is named under the ring instead, with the reason. The page ends with
the most recent requests and a sessions card that leads with the router's live pins and the accounts they sit on.

### Observe

The metric strip reports eight measures: requests per second, active streams, the error rate, TTFB at p50, p95,
and p99, output tokens per second, and the estimated cost per minute. Below it, a live table lists requests as
they run, with filters, a pause control, and a range control for switching to a past window; selecting a request
opens a detail pane with its error code and phase waterfall, and a Streams grid shows every stream still open. A stream
that fails after its 200 head keeps that status, so a finished request with an error code never reads as a success.

![The Observe view: the metric strip and the live table](assets/observe-light.png#only-light)
![The Observe view: the metric strip and the live table](assets/observe-dark.png#only-dark)

### Quota

The header counts the accounts and those needing attention. A card per account shows one row per rate-limit window,
filterable by tabs for all accounts, OAuth accounts, and Anthropic-key accounts, and every window share on the view
reads as the share left. Each OAuth card's footer says when the poll last asked the endpoint and how it answered, a
failed poll naming its reason (cut to the line's width, the whole message in the line's title), when the snapshot it
shows was read, and, while the account backs off, since when and when the next attempt comes; Refresh reads the endpoint
now unless the account is backing off or a fresh snapshot is cached. A card draws the windows the ledger sampled in the
last day, or the ones the router holds when the ledger has none, whatever the last read said, and shows an error in
their place only when it holds none; the page counts a card that shows an error, whose last poll failed, or that is
backing off as needing attention. A window whose reset has passed with no reading since reads as expired, with no share
and an empty meter, until the next reading. Each card names the account's plan with the date its subscription started
and, for an account on the Claude Code trial, when the trial ends; an account marked cancelled says when its plan ends,
in the serious tone inside a week. A card whose account served a fast-mode request shows a Fast mode line: the share of
the fast lane's input and output buckets used, as the last answer reported them, with the reset, a bucket past its reset
reading as refilled, and a chip while the account's fast lane cools, naming the reason (rate limited, or credits
unavailable) and when it lifts; the account page's Usage card shows the same line and chip. Below the cards, a reset
timeline lays the same windows across a calendar, zoomable between a two-week weekly view and a three-day 5-hour view.

![The Quota view: the cards per account and the reset timeline](assets/quota-light.png#only-light)
![The Quota view: the cards per account and the reset timeline](assets/quota-dark.png#only-dark)

### Models

The Models view lists a key's processed rules with their level and the accounts cooling for a matching model, every
model-scoped cooldown grouped by model with a clear button per row, and the newest rejections and permits
([routing](routing.md#model-rules)).

### Requests

The table offers thirteen columns, ten shown by default (pool, kind and transport start hidden, since the drawer
names all three, and the Columns control turns any on): time, key, pool, account, model, client, kind,
transport, status, attempts, TTFB, tokens, and cost. A request whose 200 answer ended short reads `incomplete` with the
code that says how: `stream_cut` when the connection or the decoder failed mid-body, `stream_incomplete` when the stream
closed without its `message_stop`, `stream_stalled` when the upstream went silent past the idle bound. The model column
reads the family and version, with the whole id on hover. The client column shows the class the router derived from the
user agent (`claude-code/<entrypoint>` or `other`), cut at 12 characters, whole on hover, and the agent
type beneath it on a subagent's turn.
One row is one
client request however many attempts it took, and the attempt trail reads as "mail 429, business 200" when a 429 on the
first account failed over to a 200 on the second. A request that did not succeed names how it ended beside its status:
`canceled`, `exhausted`, `failed`, `incomplete`, or `rejected`. A request that waited for a slot while every account was
at its cap carries a held marker beside its attempts with the time it waited. The search matches the request id, the
session, the key, the model, the error code, and an attempt's account, upstream request id or upstream message id,
literally: `_` and `%` are characters, not wildcards.
Filters narrow the table: the user agent filter takes a pattern (`claude-cli/2.1.*`, `*sdk-cli*`; `*` for any run of
characters, `?` for one, a backslash quoting either, and a value with no wildcard matching in full), the client filter
takes a pattern over the class (`claude-code/*`), the agent type and turn origin filters pick from the values the
client's hint headers and attribution line carry, the speed filter picks `fast` or `standard` by the speed the answer
reported, else by whether the request asked for `fast`, the status filter offers the codes the router and Anthropic
answer and any other code the page holds, the outcome filter picks how the request ended (`canceled`, `exhausted`,
`failed`, `incomplete`, `rejected`, or `succeeded`), and the held toggle keeps the requests that waited for a slot. The
search and the two pattern fields apply 400 milliseconds after the last keystroke, on Enter, or when the field loses
focus. Every filter, the range and the page size live in the page's address, so a reload or a shared link keeps them. A
filter whose list did not load is disabled and says why. Selecting a row opens a drawer with the request's detail, which
shows the outcome beneath the status, the user agent, the model as
the client spelled it and, when it differs, the canonical id the router recorded, its auto-mode standing (asked for a
server-side check under an on setting; carried the safeguards member under an off setting, which the dispatch drops;
refused for its body shape; or not requested), and the client hints: the agent type, the compaction and
context-compacted markers, the turn origin, and the tool time before the turn; the trail names an account the model was
rejected on and the one that served it, and each attempt names the upstream's request id under its head and, on the
answer that served, the message id a Claude Code transcript's assistant entry carries. The drawer's effort is the level
the upstream applies: when the client sends a per-turn control entry, the last entry's level, else the request's
top-level `output_config.effort`; a turn that ends with such an entry still counts as the user's turn, so it starts its
own prompt. The table holds the requests up to the moment you opened it, so paging does not shift as new ones arrive;
Refresh brings it up to now. When the session moved during the request, the drawer says from which account to which and
why: rate limited, wound down, account marked, over its plan, back to plan, load shed, or window spent. When its pin was
released without a move, the drawer says from which account and every reason the router found, among: already tried this
request, the account at its spend budget, the creating request's own client error, an account cooldown, the account
deleted, disconnected, idle past the affinity window, funding required, in overage, paused, needing reauthorization,
released outright, window spent, the subscription ended, never paired, or no longer serving the model. The drawer also
shows the strategy the selection used, the tier it drew from, whether the pin continued and was renewed, moved (naming
the rule), or was placed afresh, whether the cache answered the request and by how many of its input tokens, and every
candidate the selection passed over with the obstacle in its way: already tried, above the ceiling, at its in-flight
cap, at its spend budget, an account cooldown, at the pace floor, an operator hold, in overage, ranked behind another
candidate, its subscription ended, its window spent, or winding down. A placement names where it landed: by its messages
or by its static prefix at the depth that matched, or with the main conversation. A request held while every account was
at its cap names the account it was held on: its cohort's, or else the last one it waited on, and how long it waited.
The cache line reads a placement as a hit or a miss against the prefix it was promised: a placement by its messages is a
hit when the response read at least that prefix, one by its static prefix when it read anything, and one with the main
conversation keeps the plain cache line. The drawer shows the captured request and response bodies, each with Copy,
which copies the text as shown; a body past 256 KiB shows compact until Pretty-print is pressed.

![The Requests view: the filterable table with its client column](assets/requests-light.png#only-light)
![The Requests view: the filterable table with its client column](assets/requests-dark.png#only-dark)

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
with the key that spent them. The earlier holder is labelled `<id> (deleted, created 2026-09-14 09:31 UTC)`, the date
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
an Anthropic-key account's in dollars, side by side and each split into cached and uncached tokens, grouped by account,
key, request class, or session; the 5-hour share carries the peak band's weight, the 7-day share none. The shares read
under the account grouping alone, since shares of different accounts' windows do not add, and the Spend per minute chart
draws dollars alone, a subscription account's among them only for its fast-mode answers; past the seven lines a chart
draws, the subscription accounts folded into the rest draw their fast-mode dollars as Other (fast mode), or Other (fast
mode, tier unknown) for those whose tier the cost table does not size. The account, key and request class groupings read
the hourly rollups, which fold a request under the hour it arrived in and weigh it by that hour's band rather than at
its completion, where the meter charges it. Grouped by account, it adds a Plan per hour chart: the share of each
subscription account's 5-hour window, solid, and of its 7-day window, dashed, that its traffic used per hour, a bucket
the table cannot price (a model it has no row for, or a cell it leaves unmeasured) listed under one model unlisted
legend entry and drawn as a gap, a priced hour with a gap or the range's edge on both sides marked with a dot, an hour
with no traffic counted as zero on a window the account priced in the range, a window with no priced bucket shown as a
dash, and an account whose tier the table does not size listed as tier unknown. Latency reports percentiles read from
the merged per-hour histograms; Cache reports the hit ratio, tokens that expired between turns, the share of input
served from cache, and the output split into thinking and plain output; Cost reports the estimated spend for the range
and a Fast mode spend tile, the dollars fast-mode answers drew from usage credits; Cache warming reports what probes
cost against what they saved, where a fast-mode session's probes and credits read as dollars and file under speed
`fast`; and Accounts reports peak utilization, ineligible time, and pin churn per account.

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
router with a reason under each line. Its base URL is a placeholder for the URL that reaches the proxy listener, and
the address the listener binds shows beside the block. Then come the price table, the appearance and time-format
choices, and an About card with the version, the profile, the metrics listener, the three storage paths, the three
keepalive limits the file sets and the admin secret's standing. About closes on a pointer: the values the configuration
file sets are on the [installation page](installation.md), and `phoebis check-config` prints them.

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
| `reauth_required` | the account | the account is paired again, or an Anthropic-key account gets a new secret |
| `funding_required` | the Anthropic-key account | the operator clears funding |
| `refresh_failed` | the OAuth account | a refresh commits, or a newer credential supersedes a pending one |
| `ledger_record_parked` | the pool, or `-` for a record without one | the start after the fault lands the parked rows |
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

**`reauth_required`** holds while the account's state is `reauth_required`, read from the registry
([account states](pairing.md#account-states)). The registry records no time for a state change, so the item dates from
the first read of this process that saw the state. The detail names the account and its label.

**`funding_required`** holds while the account's state is `funding_required`, set when the upstream refuses a request
over the account's credit balance; it is read from the registry and dated and detailed as `reauth_required` is.

**`refresh_failed`** holds while the account's newest two resolved credential refreshes both failed or went uncertain,
a run read from the registry. A refresh that commits ends the run, and so does one a newer credential superseded, as a
re-pairing settles a refresh still pending; a pairing records its own committed attempt, so it ends the run even when
nothing was pending. The detail counts the run's failures and gives the newest attempt's outcome and error.

**`ledger_record_parked`** holds while the ledger file keeps priced records that failed to land on their own; each is
tried again once at the next start, and the condition clears when the table empties. The subject is the pool the record
was served under, or `-` for a record without one; a pool deleted since shows under its id with nothing to open. The
item dates from the oldest row, and the detail counts the rows and names the accounts.

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
