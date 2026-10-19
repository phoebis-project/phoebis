# Statistics

This page is for the operator reading a figure's meaning or alerting on a gauge: it describes what the ledger records
and how long it keeps it, how a request is priced in dollars and as a share of a subscription window, what the metrics
listener exports, and which conditions the router raises. The console's views are on [The console](console.md), and
the admin routes that serve their figures are in the [API reference](api-reference.md).

## The ledger

Every request that finished lands in the ledger, a second SQLite file beside the registry (`stats.ledger_path`): one row
for the client request, stamped with the instant the ledger wrote it, and one row for each upstream attempt it took.
`GET /admin/requests` filters on the stamp as `landed_after` and answers the newest one among the rows its filters admit
as `landed_through`, at most the read's `to` where it names one, and the Requests view's auto reload polls a preset
range by it with no `to`, so the route reads to its own clock. The relay waits for the ledger's queue when it must, so a
row is never lost to a busy moment. A request whose client leaves before the head lands as a canceled request with the
code `client_gone`, keeping the attempts it reached. Captured bodies are the one best-effort part, since a stream may
outrun its recorder, and a capture whose chunks were lost is stored as truncated with a `ledger_gap` event naming the
request. Once a request finishes, it is folded into the hour's rollup in the same transaction as its rows, which is what
the analytics routes report, so a long range costs one row per hour rather than one per request.

Every routing decision lands as its own row in a decisions table beside the attempt rows: one zstd-compressed record per
decision holding the request's facts, the settings in force, every pool member's standing at decision time, the rule
walk and the outcome, keyed by the request and the decision's place in it, so a hold and the placement after it are two
rows. The small trace on the attempt row stays for the analytics. The router emits the record as it emits its events,
and the ledger stores it.

A `ledger_gap` event names one of these reasons:

| Reason | Meaning |
| --- | --- |
| `attempt_missing` | The bus dropped an account's selection, so the trail is short an attempt row. |
| `capture_budget` | Past the 64 MiB capture budget, the largest capture, the oldest among equals, gave up its bodies. |
| `chunk_dropped` | The completion counted more published chunks than arrived, so the tail was dropped at the bus. |
| `chunk_skipped` | An interior chunk of a capture was dropped at the bus, and the capture is truncated from there on. |
| `expired` | The completion never reached the ledger within the 15-minute in-flight window. |
| `in_flight_cap` | The 16,384 requests in flight were reached, and the oldest entry was evicted to make room. |
| `poison` | A record without money failed on its own row after its batch's retries and was dropped. A priced one is parked instead and counted in `phoebis_ledger_parked_priced_total`. |

The request row names the key, the pool, the session, the agent and the model, the kind and transport of the call, the
status and outcome (`canceled`, `exhausted`, `failed`, `incomplete` for a 200 answer whose body ended before its end,
`rejected`, `succeeded`), and the error code (`stream_cut`, `stream_incomplete`, `stream_stalled` or
`stream_undecodable` under `incomplete`, and `stream_cut`, `stream_stalled` or `stream_undecodable` under `failed` for
a forwarded answer of another status whose body was cut, went silent or did not decode). It carries the time to first
byte and to first token, the duration, the bytes sent and received, whether the response was served kept warm, and
whether its body was captured. It names the client's user agent and class (`claude-code/<entrypoint>`
or `other`), the gateway hints it sent (the agent type, the compaction and context-compacted markers, and the summed
tool durations of the turn before), the turn origin off its attribution line, and its auto-mode standing as admission
read it: a server-side check asked for under an on setting, the `safeguards` member carried under an off setting,
which the dispatch drops, or a refusal for the body's shape before any upstream saw it. It names its classifier
standing, `client` for the client's own auto-mode classifier call (the class `auxiliary` with the beta), and the item a
`client_contract` refusal named. The request's shape is recorded as the effort, the thinking mode and budget, the output
limit, the tool, system-block and message counts, the context edits, the temperature, and the requested speed and
service tier.
A session that branched from another conversation
carries that conversation's root and the matched depth as well. The session the row names is the client's own,
and the Sessions view groups a row by it. The request row records who put the cache
diagnostics member on the body the upstream compared: the client, the router, or none.

The attempt row names the account and its kind, the upstream status, request id and message id, the outcome, the
cooldown the attempt set, the stop reason, the model the answer names when it differs from the one the router resolved,
the speed, the region inference ran in, the tokens a context edit cleared, and a refusal's category and recommended
model. It carries the five token counts, with the thinking share of the output and the web search and fetch calls the
answer made, the cache miss reason the diagnostics beta reports, the unified rate-limit window readings, overage, and
the serving account's rate-limit tier as the router held it when the request landed. No dollar is stored: every price is
computed when it is read, from these counts, at the price revision in force when the request arrived.

Placements are counted by rung, with a hit share from the trace and the response's cache-read tokens. A `content`
placement, by the conversation's own messages, is a hit when the response read the prefix the entry promised. A
`static` or `with_main` placement (by the static prefix, by a subagent's fallback to its parent agent's pin, or, with
every account at its cap, by any request's landing on another agent's pin of its session) is a hit when it read at all.

A budgeted pool member's spend sums the priced hourly rollups over each period it carries a budget for, the current UTC
day and the current month: an API-key member's rows whole, a subscription member's metered rows alone (`overage` and
`fast`). A keepalive probe's cost counts toward it the same as any other request's, since a probe on an API-key member
still draws real money. A member's spend is the period's hourly sums priced at the revision in force at each hour, so a
revision that lands moves the spend at the next refresh, and a model priced by prompt length sums its hours by the band
each attempt fell in. The rollups file each hour's traffic under who paid for it: the plan, overage, or a fast answer's
usage credits, as the response reported it. They also count, per account and pool, the time the slow-down held the
hour's requests before their heads and how many requests it held, which the Analytics view's held chart reads
([Slow-down](routing.md#slow-down)). An account's rollups also book a cooldown's time when its announcement
lands, each part to the hour it falls in, up to seven days past the announcement, a later announcement booking on from
the end already booked, and a cooldown cleared early gives back the time after the clear.

A model an upstream rejected leaves two events in the log: `model_rejected` names the refusing account and the
spelling the request carried, `model_permitted` the account that served the same request afterwards. The
[Routing log](console.md#routing-log) lists both.

Percentiles for every millisecond latency and for output tokens per second come from a fixed, log-spaced,
64-bucket histogram. A range spanning several hours merges its hourly histograms bucket by bucket before reading
the percentile, and the value carries the error of the bucket it falls in.

Every ledger read the admin routes, the usage page, and the metrics listener make runs on a blocking worker off
the request runtime, so a slow analytics query never delays proxy traffic.

## Quota samples

Quota samples arrive from two sources. Every response head from an OAuth account carries the unified rate-limit
headers, which cost nothing to read and cover whichever accounts are in use at the moment. A poll of the usage
endpoint, every five minutes (300 seconds) per OAuth account with each account on its own phase across the interval,
fills the gaps an idle account leaves, is the only source of extra-usage credit totals, and hands every clean reading
to the router. The poll's lines on each account card, Refresh and Refresh all are on [Quota](console.md#quota).

A sample's window is one of the endpoint's keys: `five_hour`, `seven_day`, `seven_day_overage_included` (the head's
overage-included week), `seven_day_fable` (the `limits` window) and any other `seven_day_*` key the poll reports. The
metric's `window` label uses the same spelling, and the console draws the windows it knows under its own labels and any
other key as it is. A head and a poll of the same five-minute bucket write one row per window, the head's where both
read it.

The usage endpoint is also read once at startup for every OAuth account with a credential, before the proxy listener
accepts requests and bounded by `routing.startup_fetch_seconds`, so selection holds a sample per account from the first
request after a restart. An account whose first answer is a 429 serves from the windows kept in the registry from its
last run, predicted forward, and its backoff starts at once. One whose first answer is a transport failure or another
error serves from them too and keeps the cadence. The poll counts that read as its own: it next reads each account five
minutes after the startup step, on the account's phase, or when its hold ends after a 429, and its profile an hour
after, on every twelfth poll. A pairing's confirm and the Quota view's Refresh read the endpoint at once, outside the
cadence, and hand the reading to the router and the ledger as a scheduled read does, under the same hold and coalescing,
and the card draws the reading the refresh took the moment it answers, its time the reading's own instant, and the
Accounts view shows the first reading once the poll lands, a few seconds after the confirm.

An account the endpoint answers 429 backs off alone. Its hold grows through two, four and eight minutes to fifteen, a
`Retry-After` honored up to the same fifteen, and the retry comes when the hold ends, sooner than the account's next
five-minute slot when the hold is shorter. Each 429 is logged as a warning naming the account and the endpoint that
answered it, `the {endpoint} endpoint rate-limited the account`, and the next clean answer returns the account to the
five minutes. A transport failure or another error keeps the cadence. The other accounts keep theirs throughout.

## Pricing

A request's cost is an estimate at list prices per million tokens, computed from its token counts when a page or a route
reads it: an estimate rather than a bill, since tier and batch modifiers are not known here and subscription quota is
the rate-limit windows, never this number. The shipped price table (`crates/ledger/api_prices.yaml`) is embedded in the
binary. `stats.api_prices_path` replaces its whole list with a file of the operator's own, Settings names each table's
path and date on its About card, and the cost reference dialog the Sessions view opens draws the table ([The cost
reference](console.md#the-cost-reference)). An attempt on a model the entry in force does not price reads no cost
estimate and is counted in `phoebis_pricing_unknown_model_total`.

The file is a list of revisions under its one key, `revisions`: each entry carries an `effective` date in `YYYY-MM-DD`
form and the whole table of that date, `models`. An entry is in force from 00:00 UTC of its date to the next entry's,
and an instant before the first entry reads the first. A request is priced at the entry in force when it arrived, so all
its attempts price under one entry. An entry added late re-prices the ledger's rows from its date on at their next read.
An entry dated after the one in force is held: every read takes it from its date on, and the burn rates the router
builds at start for the shedding round take it at the first start after that date. A correction of a figure the file had
wrong edits every entry that carries the row and adds no entry, since the correction says what was always true, while a
provider's price change, a new model or a moved threshold adds one.

The nine rows of the shipped revision in force, in USD per million tokens, in the file's order (the three Opus models'
fast cells are twice each figure):

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

`GET /admin/sessions/costs` adds a sixth figure per model, `reads_per_write_5m`, the 5-minute cache write price over
the cache read price of the standard cells: 50 for `claude-fable-5-1`, 25 for `claude-opus-5-5` and
`claude-sonnet-5-5`, and 12.5 for the other six.

A model that serves fast mode carries a second set of five cells at the fast rates under the same date. An attempt
whose answer reports `speed: fast` is priced at them, in dollars, as spend from usage credits, and one on a model
without them at the cells its prompt selects.

A model priced by prompt length carries a `long_context` block, a threshold `over` in tokens and a second set of the
five cells. An attempt whose prompt is longer than the threshold is priced at that set for all five counts, the output
included. The prompt is the attempt's uncached input, cache reads and cache writes together, as the answer's usage
reports them. A fast answer prices at the fast cells whatever its prompt. The shipped table lists Claude Haiku 5.5 with
both sets. The band an attempt's prompt selects is a dimension of the hourly rollups, so a range read from them prices
each band at its own cells.

A keepalive probe's own row is priced from the table in dollars, whatever the account's funding, at the cells the
answer's reported speed selects. A probe on a model the table does not list prices at the dearest standard cell of each
type the table carries, never at zero, so a probe that consumed quota never reads as free. On a model priced by prompt
length, a probe prices at the set its own prompt selects.

A price file whose cell is not a finite non-negative number is refused at start and by `check-config`, naming the
entry's date, the model and the cell (`fast.<cell>` for a fast-mode cell, `long_context.<cell>` for a cell over the
threshold): a negative or `NaN` price would turn every estimate negative or `NaN`, which no budget trips on. A
`long_context.over` that is not a whole number of tokens above zero is refused the same way, since a zero would price
every prompt at the long-context cells. The list itself is refused when it is empty, when an entry's date is not in
`YYYY-MM-DD` form, naming the entry's place in the list, and when two entries carry one date, naming the date
([stats](installation.md#stats) gives each message).

## Plan meter

A subscription account's plan-side figures read as a share of its own 5-hour and 7-day windows, a percent with the
window named. An API-key account, and a subscription account in overage, read dollars, and an overage request adds to
no window share.

A fast answer draws no plan window: it adds nothing to the share, the prediction, the projection, or the burn a shedding
round ranks by. It reads as a zero share in Plan per hour, and its dollars count toward the burn route's `usd`, which
Spend per minute charts under every grouping. An answer in overage reads the same in the burn figures, a zero share in
Plan per hour and its dollars in `usd`. A member's budget counts a fast answer at the fast cells, a subscription
member's as an API-key member's.

A client classifier call reads a zero share and a zero equivalent on a subscription account, with the reason
`client_classifier` on the request, and its dollars in the overage and on an API key.

The subscription cost model (`crates/ledger/subscription_costs.yaml`) ships beside the price table and holds constants:
a token costs its model's weight times its type's multiplier in units on every window, a window holds a capacity in
units per account tier, and a request's share of a window is the units it spent over that capacity, over the peak band's
capacity factor when the request completes inside the band. A unit is 1/150,000,000 of the Max 20x 5-hour window outside
the band, so one percent of that window, the step the usage header moves in, is 1,500,000 units. A request prices under
the regime that lists its client's entrypoint (the interactive regime, below), so one percent of the Max 20x 5-hour
window is 3,000,000 units for an interactive request and 1,500,000 for any other. The admin routes answer the unit as
the 20x window equivalent beside the dollars: the share of a Max 20x window the same tokens would spend, so figures of
different account tiers compare and add, with an `estimated` flag beside it. It comes in up to three members, the 5-hour
window with the peak band's weight, the 7-day window with the week's factor, and a model week over its capacity factor,
and the dashboard, key and pool routes answer the first two. One million plain input tokens of Opus 5.5 are 2,200,000
units, 1.47% of a 20x 5-hour window outside the band and 0.244% of a 20x 7-day window, and the same request on a Pro
account reads 29.3% of its own 5-hour window. Under the interactive regime the same million tokens are 0.73% of the 20x
5-hour window and 0.196% of the 20x week, and 14.7% of a Pro 5-hour window against 29.3%. The console reads the 20x
equivalent under the top bar's Unit switch, whose default Native position shows a subscription request in it and a
billed request in dollars ([The shell](console.md#the-shell)).

The base capacities the shipped file carries, in units per window, keyed by the tier the profile reports (a Pro
subscription reports `default_claude_ai`):

| Tier | 5-hour | 7-day |
| --- | --- | --- |
| `default_claude_ai` (Pro) | 7,500,000 | 90,000,000 |
| `default_claude_max_5x` | 37,500,000 | 450,000,000 |
| `default_claude_max_20x` | 150,000,000 | 900,000,000 |

The interactive regime's capacities, in the same units, twice the base 5-hour window and 1.25 times the base 7-day
window on every tier, the Pro row assumed from the Max 20x factor:

| Tier | 5-hour | 7-day |
| --- | --- | --- |
| `default_claude_ai` (Pro, assumed) | 15,000,000 | 112,500,000 |
| `default_claude_max_5x` | 75,000,000 | 562,500,000 |
| `default_claude_max_20x` | 300,000,000 | 1,125,000,000 |

A request prices under the interactive regime when its client's entrypoint is `claude-desktop`, `claude-desktop-3p`,
`claude-vscode` or `cli` (the desktop app, the third-party desktop, the IDE extension and the terminal, which the plan
meter measured at one rate). The relay reads the entrypoint in its position inside the user agent's parenthesis, the
second comma-separated token, and reads `cli` where the parenthesis names no second token or one outside the entrypoint
alphabet (ASCII lowercase letters, digits, hyphens and underscores). `sdk-cli` and every entrypoint no regime lists
price at the base rows, as does a request from a client other than Claude Code, which carries no entrypoint. An
unmeasured entrypoint prices at the base rows because they charge more, so the router sheds early rather than late.

The account's tier is recorded on each attempt when it lands, as the router held it, and carried into the hourly
rollups, so a share reads under the tier the account had when it served the request, and a tier change lands on new rows
only.

The weights of the same file, in units per plain input token, each with the date the plan meter measured it, and the
share of a Max 20x window one million input tokens spend outside the band, derived from the weight over the capacity:

| Model | Weight | Measured | 5-hour input per million on Max 20x | 7-day input per million on Max 20x |
| --- | --- | --- | --- | --- |
| `claude-fable-5-1` | 10 | 2026-09-30 | 6.67% | 1.11% |
| `claude-opus-5-5` | 2.2 | 2026-10-04 | 1.47% | 0.244% |
| `claude-sonnet-5-5` | 1.5 | 2026-09-30 | 1.00% | 0.167% |
| `claude-sonnet-5` | 1.5 | not measured | 1.00% | 0.167% |
| `claude-haiku-5-5` | 0.1 | 2026-10-08 | 0.0667% | 0.0111% |
| `claude-haiku-4-5` | 1 | 2026-10-05 | 0.667% | 0.111% |

`claude-sonnet-5` is not measured: its row takes Sonnet 5.5's weight, the model at the same list price, and stays
unmeasured since the client sends Sonnet 5 for its classifier alone, so the ledger holds no Sonnet 5 main turn to
measure against the meter. The priced models the plan meter has not measured and the file leaves out (`claude-fable-5`,
`claude-opus-5`, `claude-opus-4-8`) have no weight row and read the weight of the latest weighed model of their model
tier as an estimate, Fable 5 that of Fable 5.1 and the two Opus models that of Opus 5.5.

Under the interactive regime a request spends half the table's 5-hour share and 0.8 of its 7-day share, since the regime
doubles the 5-hour capacity and raises the 7-day by a quarter while the weights stay the same.

Every model shares one multiplier per token type: input 1, output 5, a cache write 1.25 whatever its TTL, and a cache
read 0.025. Thinking tokens count as output. A model that breaks a shared constant takes an `overrides` row, which may
name its own multiplier for a type or its own weight on a window. The shipped file has none.

The peak band covers weekdays from 5 to 11 AM Pacific time, read in the `America/Los_Angeles` zone the file names with
the end hour excluded, so its UTC hours move by an hour when daylight time starts and ends. A 5-hour window holds 0.7 of
its capacity inside the band, so a request completing inside it spends 1/0.7 of the share it would outside. The 7-day
window's factor is 1, so the week has no band. The interactive regime holds its whole capacity inside the band, so an
interactive request's 5-hour share does not move with it. The edges are steps, and Saturday and Sunday are outside the
band. The meter charges a request when it completes, so the prediction weighs a request's 5-hour share by the band in
force at its completion, and a session's burn, an average whose time constant is the pool's `burn_average_seconds`,
keeps a copy weighed by the band at each renewal, which the sessions route's burn reads while the account runs on its
plan, as the share the meter charged. The shedding round orders its pins by the plain copy: while the account runs on
its plan by its average share of the window that fired over the span the round covers, on the account's tier, and in
overage by its dollars. The projection reads the plain copy at the band in force when it reads, on every window the
session's model draws, and carries it forward weighing the band's hours inside the horizon as the band weighs them. A
projection, a runway and a stretch across a band edge weigh the in-band part of the horizon at one over the band factor
and the rest at the plain rate. The analytics routes read the hourly rollups instead, which fold a request under the
hour it arrived in, so their shares and 20x equivalents weigh it by that hour's band rather than by the band at its
completion. The burn route groups by the analytics' ten dimensions, read from the rollups the same way, and by session,
which reads the attempts, each weighed by the band at its completion. The shares answer under the account grouping, or
under every grouping when the filter names exactly one account, since shares of different accounts' windows do not add,
and the 20x equivalents answer under every grouping. The Tiers and peak band tab of the cost reference names this week's
hours in your zone ([The cost reference](console.md#the-cost-reference)).

The Fable-scoped week is a row of the file, `model_windows.seven_day_fable`, half the 7-day window of the request's
regime on every tier, so a Fable request's share of it is twice its 7-day share.

A model without a weight of its own reads the weight of the latest weighed model of its model tier as an estimate: the
prediction charges it, the projection and the shedding round price its pin's burn by that estimate, and every route that
answers its figure answers `estimated` true beside it, which the console draws as a `~` before the figure. Once the
model has served a
plan-funded request since the start, the ledger raises the `model_unweighted` attention item for the `phoebis_attention`
gauge, `GET /admin/attention` answers it, and the console lists it on the dashboard's attention line with a link to the
[Models](console.md#models) view ([Conditions and gauges](#conditions-and-gauges)). A model of no model tier, or of a
model tier with no weighed model, prices no share: the prediction adds no charge, the projection adds no burn, the
shedding round ranks the pin after every priced one and never marks it, the analytics list the share under model
unlisted and the cost reference shows a dash. An account tier without a capacity on a window prices no share there
either, and the prediction adds no charge and the projection no burn on that window. While the account runs on its plan,
a shedding round on that window reads that tier's pins as unpriced, after every priced pin, and marks none of them, and
in the analytics an account on a tier the file lacks reads tier unknown, while a window the file leaves `null` on a
listed tier reads model unlisted. A dated model id reads the weight of its undated name.

The file is a revision list in the price table's shape: `revisions`, each entry an `effective` date and the whole model
of that date, in force from 00:00 UTC of its date to the next entry's, an instant before the first entry reading the
first. The shipped file carries one entry, and `stats.subscription_costs_path` replaces the whole list. A measured
weight that replaces an assumed one edits every entry, since it says what the weight always was.

Two members of each entry name the model tiers: `tier_order`, the tiers in display order (`fable`, `opus`, `sonnet` and
`haiku` on the shipped file), and `model_tier`, each model's tier by undated id. They name the tier whose latest weighed
model stands in for an unweighted one, and `GET /admin/pricing` answers both with the rest of the cost model, so a new
model or a new family is an edit of the file. A model the map does not name takes the tier its id names after `claude-`
when `tier_order` lists it.

A new model is one weight line and its `model_tier` entry: the weight is its plain input rate in percent of the Max 20x
5-hour window per million tokens, measured outside the band through the headless client (`sdk-cli`), times 1.5, or times
3 when the measurement ran through an interactive one. An operator who cannot wait for a release copies the shipped
file, adds both lines and points `stats.subscription_costs_path` at the copy.

## Body capture

Body capture is off until a key or a pool asks for it. Once asked for, a capture holds the request and response
bodies of that key or pool in the ledger for the hours it names, each body cut at 8 MiB if the stream ran longer,
the row keeping the length it ran to so the drawer can say how much was dropped, and each captured body is dropped
once its hours run out. `stats.body_capture_max_hours` is the ceiling a key or pool may ask for, because the
bodies are prompts and how long they are kept is the operator's call rather than the caller's. The console lists a
captured body in the request drawer, with a control to copy it, and draws it in the wide view. A captured request body
is the body as the client sent it, so a session in auto mode shows its `safeguards` member whole whether or not the
router dropped it on the way upstream, which is the warning the capture control carries ([Keys](routing.md#keys)).

## The ledger writer

A batch the ledger cannot write is retried in place after 1, 2, 4 and 8 seconds, then one record at a time. A record
the file as a whole refuses (a corrupt, full, read-only or unopenable file, an I/O error) is held and retried every 10
seconds until it lands. A record that fails on its own is dropped with a `ledger_gap` when it carries no money and
parked in the ledger file when it does, and every parked record is tried again once at the next start or on the
dashboard's Replay, its parked row deleted in the transaction that lands the record, so its charge counts once. A parked
record keeps its request and attempt rows and not its captured bodies, so a parked request loses its capture and keeps
its cost. While a batch is under retry the ledger is degraded: the system page shows it,
`phoebis_ledger_degraded` reads 1, and routing treats every budgeted member's spend as unknown until a batch lands and
the spend is read again.

The routing state writer is bounded the same way: a stalled registry file leaves its writes in memory, coalesced per
row, and the system page's `router.writer` reads `degraded` until they drain. A batch the registry file as a whole
refuses is tried once more at the writer's next wake and counted under `router.writer.failed_batches`. A second
refusal drops it.

## Retention

| Data | Kept for |
| --- | --- |
| Requests, attempts, events, decisions, kept-warm marks | `stats.retention_days` (365 days by default, 0 keeps them forever) |
| Quota and extra-usage samples | `stats.retention_days` (365 days by default, 0 keeps them forever) |
| Resolved credential refresh attempts and registry changes | `stats.retention_days` (365 days by default, 0 keeps them forever) |
| Captured bodies | The hours the capture named |
| Hourly rollups and account rollups | `stats.rollup_retention_days` (365 days by default, 0 keeps them forever, otherwise at least `stats.retention_days`) |

A routing decision leaves in the purge that takes its request. One whose request never landed leaves at the same age.

The purge that enforces these runs once an hour on its own connection, in slices of 1,000 rows per transaction, so the
writer never waits behind a whole pass and the day's cut costs the WAL one slice at a time. Every five-minute poll's
reading is kept for `stats.retention_days`, so an idle account adds 288 quota rows per window per day. The same pass
cuts the registry's history at the requests' cutoff, in slices of 1,000 rows: its resolved credential refresh attempts
and its change log. It keeps what is read whatever its age: an attempt whose outcome is still uncertain, each account's
newest two resolved attempts, which the attention line reads, and its newest attempt, which the account view reads, and
each key id's newest create and delete, which tell a re-created key from its predecessor. A budgeted member's hourly
rows stay from its period's start on, so a retention shorter than the period never reopens an exhausted member.

The analytics, the dashboard, the pool cards and the burn chart read the hourly rollups in pages of a day, each page
its own read, so a 400-day range holds one day of rows in memory and the reader connection is free between pages.

A prompt-length threshold that moves re-folds the hours whose attempts the ledger still holds at the next start, and the
older hours keep their split. The re-fold moves the request rows alone. A wasted probe's counts and a probe the
keepalive still holds keep the band they settled or were recorded in, so the waste of a probe whose prompt falls
between the old and the new threshold inside the retained window stays priced at that band.

## Metrics

`stats.metrics_listen` (`127.0.0.1:9464` by default) serves `/metrics` in Prometheus text format and nothing else. It
speaks plain HTTP and asks for no credential, so it belongs behind a network only the scraper can reach. Twenty-three
families are rendered, each with a `# TYPE` line and no `# HELP` line:

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

The `model` label is bounded by the price entry in force at the request: a model reads as the row that prices it, its
own id or, for a dated id priced under its undated base, that base, and every other model is reported as `other`, so a
downstream caller cannot grow the label set by naming models it likes. `phoebis_tokens_total` counts under five `kind`
values, `cache_read`, `cache_write_1h`, `cache_write_5m`, `input` and `output`, and `phoebis_upstream_ttfb_seconds` uses
the same 64 log-spaced buckets the rollups use, with an overflow bucket.
`phoebis_keepalive_probes_total` counts each probe under its pool and one of four outcomes: `landed` when it read the
whole prefix back, or wrote it on the account a migration moved it to, `cold` when the upstream served it without that
read, `failed` when the upstream did not serve it, and `held` when the router refused it with `503 ledger_behind` while
its ledger caught up, which the engine sends again.
`phoebis_keepalive_skipped_total` counts a due probe the engine withheld, under its pool and the recipe's state, one of
[the five states](keepalive.md#the-five-states): `held` or `paused` for a gate that held it, `ended` for an entry that
lapsed before it.
`phoebis_account_cooldown_seconds`, `phoebis_account_overage_in_use`, `phoebis_keepalive_agents` (its `state` one of
`busy`, `idle` and `parked`), and `phoebis_pins_active` are the four routing-state gauges. Each is computed fresh from
the router's own snapshot on every scrape rather than accumulated between them. `phoebis_pins_active` is counted per
pool by the routing loop itself, so a scrape copies one count per pool rather than the pin table. Beside the four,
`phoebis_router_writer_depth` and `phoebis_router_writer_degraded` are read at the scrape from the router's writer
status: the batches waiting in the routing state writer's channel, and 1 while writes past its bound wait in memory
([the ledger writer](#the-ledger-writer)).
`phoebis_ledger_queued` reads the records waiting in the ledger's writer channel at the scrape, and
`phoebis_ledger_behind_refusals_total` counts the requests answered `ledger_behind` while the channel held 2,048
records or more, half its 4,096 ([Refusal codes](client.md#refusal-codes)).
`phoebis_ledger_degraded` reads 1 from a batch the ledger could not write until a batch lands and the spend is read
again, and `phoebis_ledger_parked_priced_total` counts the priced records parked since start
([the ledger writer](#the-ledger-writer)).

## The console's views

The views that show the figures above are described on [The console](console.md), each with its figures, filters and
controls: the [Dashboard](console.md#dashboard) with its attention line, four 24-hour tiles and two capacity rings,
[Models](console.md#models) with the rules a key's requests are judged by, and [Settings](console.md#settings) with
the About card that names each price table's path and date.

### Quota

The subscription accounts' windows, their resets, extra usage and poll standing, read from the samples above, are on
[Quota](console.md#quota).

### Requests

The request table, its filters and the drawer that opens a row, with the captured bodies and the phases a request went
through, are on [Requests](console.md#requests).

### Routing log

The decisions and events of the log, the decision card and the query the view keeps in the page's address are on
[Routing log](console.md#routing-log).

### Analytics

The Traffic, Burn, Latency, Cache, Cost, Cache warming and Accounts sections, their ranges and groupings, are on
[Analytics](console.md#analytics), the held time per bucket under Traffic's moves chart among them, which reads the
slow-down's two rollup counts above.

## Conditions and gauges

The router exports what needs an operator as Prometheus gauges and shows the same conditions on the dashboard's
attention line and the pool cards, which read `GET /admin/attention` (see [API reference](api-reference.md)), worst
first and the oldest first within a kind. `phoebis_attention` carries the labels `kind` and `subject`, reads 1 while the
cause holds and 0 once it clears, and has a series for every subject the kind applies to, so a cleared condition reads 0
rather than vanishing. A deleted account or pool loses its series at the next read, except that a pool a parked record
names keeps a holding `ledger_record_parked` series after it is deleted, and the `-` subject's and a deleted pool's
`ledger_record_parked` series leave when their rows go. A deleted account or pool a routing condition holds on keeps
that series the same way, until the fold's trailing day lets its row go. A restart starts the conditions fed by the
request bus over, and the usage poll keeps its backoff in memory as well, so `poll_backoff` reads 0 until a new backoff
run is thirty minutes old. The conditions read from the registry, and `ledger_record_parked`, read from the ledger file,
hold again at the first read. The four routing conditions, `wall_reached`, `destination_spent`, `shed_no_runway` and
`slowed_long`, read a table the ledger folds from its events of the trailing 24 hours once an hour and at start, so a
restart holds them again at the first fold. `meter_drift` lands in the same table by the same fold, from the reconciled
windows rather than the events, so a restart holds it again at the first fold too.

| Kind | Tone | Subject | Clears when |
| --- | --- | --- | --- |
| `pool_exhausted` | critical | the pool | a request of the pool succeeds |
| `client_drift` | serious | the client version | an inference request from that version is relayed, or a re-pinned build starts |
| `writer_degraded` | serious | the ledger | a batch lands and the spend refresh after it succeeds |
| `writer_degraded` | serious | the routing state writer | the backlog drains |
| `certificate_reload_failed` | serious | the listener key | a reload lands or the router restarts |
| `reauth_required` | critical | the account | the account is paired again, or an API-key account gets a new secret |
| `funding_required` | serious | the API-key account | a retry succeeds after five minutes (or the [model rejection interval](routing.md#cooldowns-and-failover) when that is shorter), or the operator clears funding |
| `refresh_failed` | serious | the OAuth account | a refresh commits, or a newer credential supersedes a pending one |
| `certificate_expiring` | warning | the listener key | a pair with more than 30 days left is loaded |
| `ledger_record_parked` | serious | the pool, or `-` for a record without one | the next start or the Replay button lands them |
| `poll_backoff` | warning | the polled account | a poll answers clean |
| `model_unweighted` | warning | the model | a later subscription request on the model is priced at a cost model entry that weighs it, or the router restarts |
| `meter_drift` | warning | the OAuth account | the newest reconciled window of each kind with a rise of at least 5% reads a ratio inside 0.8 to 1.25, or no window has such a rise |
| `wall_reached` | warning | the account | no window of the account read spent in the last 24 hours |
| `destination_spent` | warning | the destination account | no move onto the account was followed by a spent window within an hour in the last 24 hours |
| `shed_no_runway` | warning | the pool | no round of the pool marked nothing for want of a destination in the last 24 hours |
| `slowed_long` | warning | the account | the slow-down spans of the last 24 hours sum at or under twice the pool's `slow_down_stretch_seconds`, at least an hour |

**`pool_exhausted`** holds while a request of the pool has ended exhausted and none of the pool's requests has
succeeded since. It is fed by the request bus. An `incomplete` answer neither raises nor clears it. The detail names the
pool and the key of the request that raised it.

**`client_drift`** holds while a request from that client version has been refused for drift since the process started
and no inference request from it has been relayed since. What the relay judges is on
[Client drift](relay.md#client-drift), and the cure, a build re-pinned to the client, on
[Re-pinning the client build](operations.md#re-pinning-the-client-build). The condition is fed by the request bus. A
token count or a keepalive request clears nothing: a count carries no stamp for the check to read, and a keepalive
request replays a body the router kept rather than the client's next one. The subject is the version the client's user
agent names when it reads as a release, one to four runs of digits joined by dots in at most 32 bytes. Any other value
is filed under `other`, and a request that named none under `unknown`. At most sixteen versions are kept: a new one
takes the place of the cleared version whose run started first, or of the one that has held longest when every version
holds. The detail names the version, what drifted (`hash` or `version_suffix`) and the key of the newest refusal, which
the item opens. A refusal that named no key opens nothing.

**`writer_degraded`** holds for the ledger from a batch it could not write, through the batch's retries and the hold of
a record the file as a whole refuses, until a batch lands and the spend refresh after it succeeds. It holds for routing
while the routing loop keeps state writes in memory because the registry file is not taking them
([the ledger writer](#the-ledger-writer)). The subject is the component, `ledger` or `routing`, which the detail names.
Neither writer records when it degraded, so the item dates from the first read of this process that saw it, and it
opens nothing. While the ledger's holds, routing treats every budgeted member's spend as unknown
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
over the account's credit balance. It clears when the operator clears funding or when the router's retry succeeds after
five minutes, or the model rejection interval when that is shorter ([cooldowns and
failover](routing.md#cooldowns-and-failover)). It is read from the registry and dated and detailed as `reauth_required`
is.

**`refresh_failed`** holds while the account's newest two resolved credential refreshes both failed or went uncertain,
a run read from the registry. A refresh that commits ends the run, and so does one a newer credential superseded, as a
re-pairing settles a refresh still pending. A pairing records its own committed attempt, so it ends the run even when
nothing was pending. The detail counts the run's failures and gives the newest attempt's outcome and error.

**`certificate_expiring`** holds while the leaf a listener serves is within 30 days of its end, past the end included.
It reads the pair the router loaded rather than the files, so a pair replaced on disk counts once `phoebis tls reload`
loads it. The subject is the listener key, and the item opens nothing. It dates from 30 days before the leaf's end, so a
restart keeps the date, and the detail names the certificate file, the end and the whole days left, negative once past,
which the dashboard's line words as `expires in 12 days` or `expired 3 days ago`, and as `expires today` and
`expired today` in the day before the end and the day after it.

**`ledger_record_parked`** holds while the ledger file keeps priced records that failed to land on their own. Each is
tried again once at the next start or on the dashboard's Replay, and the condition clears when the table empties. The
subject is the pool the record was served under, or `-` for a record without one. A pool deleted since shows under its
id with nothing to open. The item dates from the oldest row, and the detail counts the rows and names the accounts.

**`poll_backoff`** holds once the usage poll has been backing off from the endpoint's 429 answers on the account for
thirty minutes, counted from the run's first 429. The detail gives when the run started, when the next attempt comes,
and how many 429s the endpoint answered in a row.

**`model_unweighted`** holds while a model the cost model does not weigh has served plan-funded requests since the
start, each priced at the weight of the latest weighed model of its model tier and marked estimated wherever its figure
shows ([Plan meter](#plan-meter)). The ledger keeps the run in memory, so it starts over at a restart, and the next
plan-funded request on the model raises it again. A later plan-funded request priced at a cost model entry that weighs
the model clears it. The subject is the model, the dashboard's line reads the item as "Model weight estimated", and it
opens the [Models](console.md#models) view. It dates from the run's first request, and the detail names the model, its
model tier, the model whose weight stands in and the requests served since.

**`meter_drift`** holds while, for any window kind, the account's newest completed window with a rise of at least 5%
reads a ratio outside 0.8 to 1.25 between the rise the upstream's readings showed and the share the router priced for
its own requests in it. The ledger reconciles each account's completed 5-hour, 7-day and Fable windows every hour,
prices the plan-funded attempts inside them under each attempt's regime, and keeps the rows for 30 days, which the Meter
reconciliation card of the [Analytics](console.md#analytics) view lists. The subject is the account, and the item dates
from the oldest off window's end. The detail names the off windows with their ratios, the cost model's source and the
revision in force, since the fix is the cost model file. The dashboard's line reads it under the label "Meter drift
against the table" as `acct-a: 5h charged 2.0x the table, 7d 1.3x, since 14:20`, and it opens the Analytics view on the
account with the Burn section in view.

**`wall_reached`** holds while a window of the account read spent in the last 24 hours: a `window_spent` event of any
source, the prediction's among them, so a wall the router predicted counts before a head or the poll reads it. A wall is
announced once per reset cycle, since a newer head of the same cycle within two points of 1.0 after it neither clears
nor repeats it, and two points cover a head, which reads the share up to a point under the poll's whole percent, with
one point of room. A head that names no reset, taken before the announced reset, is of the announced cycle and neither
clears nor repeats the wall, and the cycle that holds a head within the two points runs from the instant the wall was
announced, or from the reset's passing for a wall that goes on past its reset. The ledger folds the four routing
conditions from its events once an hour and at start into one table, which the gauge and `GET /admin/attention` read, so
a condition clears at the first fold whose trailing day holds none of its events. The subject is the account, and the
item dates from the oldest wall of the day. The detail counts the walls, names their windows and the settings `shed_cap`
and `window_ceiling`, and names one figure by the trigger of the newest shedding round on the wall's window within that
window's length before the wall: after a projection round the projection error, one less the round's projection, after a
slow-down round the time the wall arrived before the reset (0 at or after it), and neither with no round. The ledger
also keeps each account's longest early wall of the hour under a slow-down round, so the figure outlives the trailing
day. The item opens the [routing log](console.md#routing-log) on the account's spent windows since the condition began.

**`destination_spent`** holds while a move landed on the account in the last 24 hours and a `window_spent` event of any
source followed on it within the hour. It is folded as `wall_reached` is. The subject is the destination account, and
the item dates from the oldest such move. The detail counts the moves, gives their destination fits with a count each
and names the spent windows and the setting `window_ceiling`. The item opens the routing log on the account's moves and
spent windows since the condition began.

**`shed_no_runway`** holds while a shedding round of the pool marked nothing for want of a destination with runway (the
outcome `nothing_to_mark` with the reason `no_runway`) in the last 24 hours, one round being enough. It is folded as
`wall_reached` is. The subject is the pool, and the item dates from the oldest such round. The detail counts the rounds
and names the accounts they ran on and the setting `window_ceiling` with the pool's value. The item opens the routing
log on the pool's shedding rounds since the condition began.

**`slowed_long`** holds while the account's slow-down spans of the last 24 hours, each from its `slow_down_started`
event to its `slow_down_ended` event, to the previous run's last row where a `router_started` row closed it (or to that
row where it names none), or to now while it stands open, sum past twice the pool's `slow_down_stretch_seconds`, at
least an hour (five hours at the default of 9000, the hour on a pool set to move early at 1800), every span clamped to
the day. It is folded as `wall_reached` is. The slow-down holds an account for as long as its demand runs ahead of its
window, and the threshold is how long the operator lets it hold before a round moves sessions, so two thresholds' worth
of slow-down in a day says the pool's load exceeds that account's windows, which is the pool's membership, its ceiling
or its threshold and not one turn, while an account slowed for ten minutes at the end of a window is working as intended
and raises nothing. The subject is the account, and the item dates from the start of the oldest span counted. The detail
counts the spans, gives their sum, the bar, whether a span is open and the lowest factor of the spans, and names the
setting `slow_down_stretch_seconds` with the pool's value, which the dashboard's line words under the label "Slowed past
twice the stretch" as
`studio: slowed 2 h 8 min of the last 24 h, over 1 h, slowest 0.35x, slow_down_stretch_seconds 1800, since 09:12`. The
item opens the routing log on the account's slow-down starts and ends since the condition began
([Slow-down](routing.md#slow-down)).

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
