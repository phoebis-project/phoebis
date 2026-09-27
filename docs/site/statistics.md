# Statistics and alerts

Every request that finished lands in the ledger, a second SQLite file beside the registry, and the console, the
analytics routes, the metrics listener, and the alert engine read from it or from the same events. This page
describes what is recorded, how long it is kept, what each view shows, and how an alert rule is written and
delivered.

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
| `in_flight_cap` | The 16,384 requests in flight was reached; the oldest entry was evicted to make room. |

The request row names the key, the pool, the session, the agent, and the model; the kind and transport of the
call; the status and outcome; the error code; the time to first byte and to first token; the duration; the bytes
sent and received; whether the response was served kept warm; whether its body was captured; and the client's
user agent and class (`claude-code/<entrypoint>` or `other`), the gateway hints it sent (the agent
type, the compaction and context-compacted markers, and the summed tool durations of the turn before), the turn
origin off its attribution line, and the request's shape: the effort, the thinking mode and budget, the output
limit, the tool, system-block, and message counts, the context edits, the temperature, and the requested speed
and service tier.
A session that branched from another
conversation carries that conversation's root and the matched depth as well.
The attempt row names the account and its kind; the upstream status and request id; the outcome; the
cooldown the attempt set; the stop reason, the model the answer names when it differs from the one the router
resolved, the speed, the region inference ran in, the tokens a context edit cleared, and a refusal's category
and recommended model; the five token counts, with the thinking share of the output and the web search and
fetch calls the answer made;
the cache miss reason the diagnostics beta reports; the unified rate-limit window readings; overage; the cost
split across input, cache read, cache write, and output; and the pricing version the estimate used.

Placements are counted by rung, with a hit share from the trace and the response's cache-read tokens: a
placement by the conversation's own messages is a hit when the response read the prefix the entry promised;
one by the static prefix is a hit when it read at all.

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
endpoint, every `quota.poll_interval_seconds`, fills the gaps an idle account leaves and is the only source of
extra-usage credit totals.

The usage endpoint is also read once at startup for every OAuth account with a credential, before the proxy listener
accepts requests and bounded by `routing.startup_fetch_seconds`, so selection holds a sample per account from the
first request after a restart; the poll then keeps its interval.

## Pricing

Every attempt is stamped with a cost estimate at list prices per million tokens: an estimate rather than a bill,
since tier and batch modifiers are not known here and subscription quota is the rate-limit windows, never this
number. The shipped price table is embedded in the binary and dated 2026-09-08; `stats.pricing_path` overrides it
with a file of the operator's own; an import confirmed through Settings wins over both and is kept as its own
version. An import takes a URL, shows a diff against the table in force, and asks for confirmation before it
writes anything; the preview it shows expires after ten minutes. Settings tells the operator the import "accepts
the LiteLLM `model_prices_and_context_window.json` or a models.dev catalog"; a source that prices only the
5-minute cache write has its 1-hour price derived as 1.6 times it. An attempt on a model the table does not price
carries no cost estimate and is counted in `phoebis_pricing_unknown_model_total`.

## Plan meter

The plan side of every figure is counted in meter tokens: one plain input token of Claude Opus 5 on the 5-hour meter of
a subscription. Two dated tables ship beside the price table. `plan_factors` says what one token of each type costs in
meter tokens, the same ratios for every model: an input token 1, a cache read 0.026, a 5-minute or 1-hour cache write
1.23 and an output token 4.67, each times the model's factor.

| Model | Factor | Measured |
| --- | --- | --- |
| `claude-fable-5` | 2.0 | Not measured: the list-price ratio |
| `claude-fable-5-1` | 3.61 | 2026-09-19 |
| `claude-haiku-4-5` | 0.36 | 2026-09-24 |
| `claude-opus-5` | 1 | 2026-09-19 |
| `claude-sonnet-5` | 0.54 | 2026-09-24 |

A row flagged `unmeasured` carries the list-price ratio until a meter run fills it. A dated model id reads the row of
its undated base, and a model the table lacks weighs as Claude Opus 5. `plan_windows` holds each subscription tier's
window sizes as measured on Max 5x, Max 20x and Pro: the 5-hour and 7-day windows in meter tokens, the Fable-scoped week
in Fable tokens, since the weekly meter may weigh Fable differently from the 5-hour one. The live fit replaces them per
account once it folds.

**Live fit** The ledger fits each subscription account's window sizes from its own traffic: the meter tokens the router
sent to the account, booked when each request completes, against the rise of the account's utilization readings. A run
of readings folds once it has risen by three hundredths of the window, reading the booked tokens over the summed rise,
so no fold rests on a single rounding step. A reading folds nothing while any of the router's requests other than the
one whose response head carried it is in flight on the account, since its rise may carry that request's charge before
its tokens are booked; the run carries on to the next reading with no other request in flight. The carrying request's
own charge is not in its head's reading, since the meter charges a request on the reading that follows it, so an account
whose requests run one after another can fold at each head, and one that keeps two or more in flight folds only at
a reading with at most its own in flight; a run that outgrows its window that way is dropped. The fit drops a run when a
reset falls inside it (the reset time moved or the utilization fell by a hundredth or more; a smaller fall is rounding
and reads as flat), when it outgrows its window, when the utilization rose while the router had nothing booked or in
flight on the account, and when a request that went out before a reset finished after it. The ratios between token types
stay the table's; a per-model correction moves only once the fit is confident and the model mix changed between folds.
Spend another client sends while one of the router's requests is in flight counts as the router's, since the usage
endpoint reports no per-surface breakdown. Each fit keeps its carried run and its band sums in the registry beside the
size, so a restart continues them.

**Peak band** On weekdays from 5 to 11 AM Pacific time, 12:00 to 18:00 UTC while daylight time holds and 13:00 to 19:00
UTC from 2026-11-01, the 5-hour meter charges a request about 1.45 times as much. Output read 1.44 on 2026-09-24 and
input 1.33 on 2026-09-25, so the factor lies between 1.3 and 1.45 until one window reads both; the edges are steps, and
a weekend probe read the off-peak rate on 2026-09-26, so the band is weekdays only; the row stays provisional for its
factor until one in-band window reads both input and output. The meter charges a request when it completes, so the band
in force then sets its weight: the fit books its tokens that way, and a session's burn in meter tokens folds a renewal
inside the band at the band's factor, so the shedding order reads what the meter charges. A band row with a later date
takes over from that date and never re-weights an earlier request.

The analytics set each fit beside its tier's row, which shows the drift between the table and the meter, and the band
factor the fit observes beside the row's.

## Body capture

Body capture is off until a key or a pool asks for it. Once asked for, a capture holds the request and response
bodies of that key or pool in the ledger for the hours it names, each body cut at 8 MiB if the stream ran longer,
the row keeping the length it ran to so the drawer can say how much was dropped, and each captured body is dropped
once its hours run out. `stats.body_capture_max_hours` is the ceiling a key or pool may ask for, because the
bodies are prompts and how long they are kept is the operator's call rather than the caller's. Only the admin role
reads bodies: a viewer's request for them is answered `403 captured bodies are for administrators`. The console
shows a captured body in the request drawer, with a control to copy it.

## Retention

| Data | Kept for |
| --- | --- |
| Requests, attempts, events, kept-warm marks | `stats.retention_days` (90 days by default; 0 forever) |
| Quota and extra-usage samples | `quota.sample_retention_days` (30 days by default) |
| Captured bodies | The hours the capture named |
| Hourly rollups | `stats.rollup_retention_days` (400 days; 0 forever, otherwise at least `stats.retention_days`) |

The purge that enforces these runs once an hour.

## Metrics

`stats.metrics_listen` serves `/metrics` in Prometheus text format and nothing else; it speaks plain HTTP and
asks for no credential, so it belongs behind a network only the scraper can reach. Sixteen families are
rendered:

| Metric | Type | Labels |
| --- | --- | --- |
| `phoebis_account_cooldown_seconds` | gauge | `account`, `scope` |
| `phoebis_account_overage_in_use` | gauge | `account` |
| `phoebis_account_ratelimit_utilization` | gauge | `account`, `window` |
| `phoebis_alert_deliveries_total` | counter | `outcome`, `rule` |
| `phoebis_attempts_total` | counter | `account`, `model`, `outcome` |
| `phoebis_bus_dropped_total` | counter | `subscriber` |
| `phoebis_bus_subscriber_panics_total` | counter | `subscriber` |
| `phoebis_keepalive_agents` | gauge | `state` |
| `phoebis_keepalive_probes_total` | counter | `account`, `outcome` |
| `phoebis_keepalive_saved_usd_total` | counter | `pool` |
| `phoebis_ledger_dropped_total` | counter | `table` |
| `phoebis_pins_active` | gauge | `pool` |
| `phoebis_pricing_unknown_model_total` | counter | none |
| `phoebis_requests_total` | counter | `key`, `kind`, `outcome` |
| `phoebis_tokens_total` | counter | `account`, `kind`, `model` |
| `phoebis_upstream_ttfb_seconds` | histogram | `model` |

The `model` label is bounded by the price table: a model reads as the table entry that prices it, its own id or, for a
dated id priced under its undated base, that base; every other model is reported as `other`, so a downstream caller
cannot grow the label set by naming models it likes.
`phoebis_keepalive_probes_total`'s `account` label reads `-` for a probe cancelled before any account was
attributed, the same reserved id a request row with no account reads under.
`phoebis_account_cooldown_seconds`, `phoebis_account_overage_in_use`, `phoebis_keepalive_agents`, and
`phoebis_pins_active` are the four routing gauges; each is computed fresh from the router's own snapshot on every
scrape rather than accumulated between them.

## The live feed

`GET /admin/live` upgrades to a WebSocket that reports requests while they run; `GET /admin/live/snapshot?at=`
(or `?from=` and `?to=`) answers the same shape for a past window, 30 minutes by default. Both need an admin
session, like every other admin route. After the upgrade the socket sends one `{"kind": "snapshot", ...}`
message and then a `{"kind": "delta", ...}` message for every change. The feed keeps 30 minutes or 2,048
finished requests, whichever bound is reached first; a request or response preview inside a message is at most
4 KiB; and a client that falls 1,024 messages behind the broadcast is disconnected rather than left to grow an
unbounded backlog. A session logged in with the viewer secret sees no preview on the socket, and a past
snapshot carries no preview whatever the role. The socket closes `1008 session ended` once its session ends,
and at most 64 sockets are served at once, a further one refused `503 live feed at capacity` until one closes.

## The console's views

### Dashboard

Four tiles report requests, tokens, estimated cost, and the error rate over the last 24 hours; two cards below
them show the 5-hour and 7-day quota remaining for each subscription account; and the page ends with the most
recent requests and a sessions card that leads with the router's live pins and the upstreams they sit on.

### Observe

The metric strip reports eight measures: requests per second, active streams, the error rate, TTFB at p50, p95,
and p99, output tokens per second, and the estimated cost per minute. Below it, a live table lists requests as
they run, with filters, a pause control, and a range control for switching to a past window; selecting a request
opens a detail pane with its error code and phase waterfall, and a Streams grid shows every stream still open. A stream
that fails after its 200 head keeps that status, so a finished request with an error code never reads as a success.

![The Observe view: the metric strip, the live table, and the phase waterfall](assets/observe-light.png#only-light)
![The Observe view: the metric strip, the live table, and the phase waterfall](assets/observe-dark.png#only-dark)

### Quota

A card per upstream shows one row per rate-limit window, filterable by tabs for all accounts, OAuth accounts,
and API-key accounts. Each card names the account's plan with the date its subscription started and, for an account
on the Claude Code trial, when the trial ends; an account marked cancelled says when its plan ends, in the serious
tone inside a week. Below the cards, a reset timeline lays the same windows across a calendar, zoomable between a
two-week weekly view and a three-day 5-hour view.

![The Quota view: the cards per upstream and the reset timeline](assets/quota-light.png#only-light)
![The Quota view: the cards per upstream and the reset timeline](assets/quota-dark.png#only-dark)

### Models

The Models view lists a key's processed rules with their level and the upstreams cooling for a matching model, every
model-scoped cooldown grouped by model with a clear button per row, and the newest rejections and permits
([routing](routing.md#model-rules)).

### Requests

The table lists thirteen columns: time, key, pool, upstream, model, client, kind, transport, status, attempts,
TTFB, tokens, and cost. The client column shows the class the router derived from the user agent
(`claude-code/<entrypoint>` or `other`), with the agent type beneath it on a subagent's turn. One row
is one client request however many attempts it took, and the attempt trail reads as "mail 429, business 200" when a
429 on the first account failed over to a 200 on the second. Filters narrow the table: the user agent filter takes a
pattern (`claude-cli/2.1.*`, `*sdk-cli*`; `*` for any run of characters, `?` for one, a backslash quoting either, and
a value with no wildcard matching in full), the client filter takes a pattern over the class (`claude-code/*`), and
the agent type and turn origin filters pick from the values the client's hint headers and attribution line carry.
Selecting a row opens a drawer with the request's detail, which shows the user agent, the model as the client spelled it
and, when it differs, the canonical id the router recorded, and the client hints: the agent type, the compaction and
context-compacted markers, the turn origin, and the tool time before the turn; the trail names an upstream that rejected
the model and the one that served it. The table holds the requests up to the moment you opened it, so paging does not
shift as new ones arrive; Refresh brings it up to now. When the session moved during the request, the drawer says from
which upstream to which and why: rate limited, wound down, upstream marked, over its plan, back to plan, load shed, or
window spent. When its pin was released without a move, the drawer says from which upstream and every reason the
router found, among: already tried this request, the creating request's own client error, an account cooldown, the
account deleted, disconnected, idle past the affinity window, funding required, in overage, paused, needing
reauthorization, released outright, window spent, the subscription ended, never paired, or no longer serving the
model. The drawer also shows the strategy the selection used, the tier it drew from, whether the pin continued and
was renewed, moved (naming the rule), or was placed afresh, whether the cache answered the request and by how many
of its input tokens, and every candidate the selection passed over with the obstacle in its way: already tried,
above the ceiling, at its in-flight cap, an account cooldown, at the pace floor, an operator hold, in overage,
ranked behind another candidate, its subscription ended, its window spent, or winding down.

![The Requests view: the filterable table with its client column](assets/requests-light.png#only-light)
![The Requests view: the filterable table with its client column](assets/requests-dark.png#only-dark)

### Analytics

A range control offers 1 hour, 24 hours, 7 days, 30 days, or a custom span, and a group-by control regroups every
chart and table by client, effort, key, kind, model, pool, request class, upstream, or user agent; the client
grouping reads the class and the user agent grouping the raw agent. The request class shown by this grouping is the
value the client sent on its class header, verbatim, so a class a client adds later shows up under its own name; a
classless request folds under the reserved value alongside a keepalive credit. A key deleted and created again under
the same id appears twice under the key grouping, the earlier holder labelled `<id> (deleted)`, since its figures
stay with the key that spent them. The client and user agent filters take the same patterns as the request log's, and
under the user agent grouping a pattern list names the groups: `claude-cli/2.1.*` and `claude-cli/2.0.*` as two
groups, every other agent under `other`.

Traffic reports request volume by outcome and the error rate, with a Failovers chart and a Migration rewrites card
beside it: the cache-write tokens the first request after a move paid to warm its new upstream, and the moves the range
counted, by reason. Burn reports the plan's burn in meter tokens for a subscription account and in dollars for an
api-key account, side by side and each split into cached and uncached tokens, grouped by upstream, key, session, or
request class, beside a Plan meter fit table (see [Plan meter](#plan-meter)); a subscription account's meter tokens
carry the peak band's weight. The key, request class and upstream groupings read the hourly rollups, which fold a
request under the hour it arrived in and weigh it by that hour's band, while the live fit weighs a request at its
completion: a request that starts before the band and completes inside it reads at the off-peak weight in these charts
and at the band's in the fit, and one that starts inside the band and completes after it the other way round. The
session grouping weighs each attempt at its completion, as the fit does. Grouped by upstream, it adds a Plan per hour
chart, the plan-percent view: the share of each subscription upstream's 5-hour window, at its fitted size or else its
tier's, that its traffic used per hour, weighted by the peak band. Latency reports percentiles read from the merged
per-hour histograms; Cache reports the hit ratio, tokens that expired between turns, the share of input served from
cache, and the output split into thinking and plain output; Cost reports the estimated spend for the range; Cache
warming reports what probes cost against what they saved; and Upstreams reports peak utilization, ineligible time, and
pin churn per upstream.

Cooldown time books to the hour it elapses in as the interval runs, and a cooldown cleared early gives back only the
hours it never held. A cooldown is booked up to seven days ahead of the announcement that set it, and a later
announcement books the next stretch. Filtered to one session, the view sums the request log instead of the rollups, and
the cost split, the cache write and read costs, and the Cache warming figures read as unavailable because the request
log does not carry them; the Upstreams section keeps reading the rollups for the range, since they carry no session, and
its subtitle says so.

![The Analytics view: the range and group-by controls over seven report sections](assets/analytics-light.png#only-light)
![The Analytics view: the range and group-by controls over seven report sections](assets/analytics-dark.png#only-dark)

### Settings

Settings covers appearance, access (the admin and viewer secrets and what each may do), pricing, sessions, retention,
keepalive, listeners, the downstream client block, compatibility, and an about panel. Four more cards show the
bootstrap file's quota, routing, storage, and usage blocks: the quota poll interval and sample retention, the
catalog cache window, the cooldown fallback, the model rejection window, and the attempt limit, the ledger,
pricing, and storage paths, and the usage cache TTL and refresh concurrency.

## Alerts

A rule names a kind, a scope, a webhook URL, an optional secret, and an enabled flag; only `utilization_above`
also takes a threshold from 0 to 1 and a window of `5h`, `7d`, or `7d_oi`, and `session_idle` takes a threshold in
whole minutes from 1 to 10080. `all` and `any_account` take no scope id; `account`, `key`, and `pool` each require
one.

| Kind | Label | Scopes |
| --- | --- | --- |
| `ask_user_question` | Waiting for user input | `all`, `key`, `pool` |
| `client_drift` | Client drift | `all` |
| `funding_required` | Funding required | `account`, `any_account` |
| `keepalive_cost_cap` | Probe cost cap reached | `all`, `pool` |
| `pool_exhausted` | Pool exhausted | `all`, `pool` |
| `reauth_required` | Re-auth required | `account`, `any_account` |
| `refresh_failed` | Credential refresh failed | `account`, `any_account` |
| `session_active` | Session active | `all`, `key`, `pool` |
| `session_idle` | Session idle | `all`, `key`, `pool` |
| `session_parked` | Session parked | `all`, `key`, `pool` |
| `subagent_active` | Subagent active | `all`, `key`, `pool` |
| `utilization_above` | Utilization above threshold | `account`, `all`, `any_account`, `key`, `pool` |

**Waiting for user input** fires when a response relayed to a client carries an AskUserQuestion tool call, so the
agent is blocked on an answer; it resolves on the session's next request, the first sign the user replied, or
when the session expires. Once the captured body it was read from has passed its own retention, the question
text is withheld from the webhook the same way it is from the API and the console.

**Client drift** fires when a self-check fails for a client request: the client stamped a billing hash, or a
`cc_version` suffix, this build cannot reproduce; re-pinning the router to that client build and resolving the event by
hand clears it.

**Funding required** fires when an API-key upstream is refused for billing reasons and resolves when an operator
clears the funding flag.

**Probe cost cap reached** fires when a pool's daily probe cost cap is reached; probes pause until the next UTC
day, which resolves it, as removing the cap resolves it at the next poll.

**Pool exhausted** fires when a request finds no eligible upstream in the pool and resolves on the next request
the pool serves.

**Re-auth required** fires when an upstream enters the reauth-required state and resolves once the upstream is
paired or verified again.

**Credential refresh failed** fires when a credential refresh fails twice in a row, an attempt whose answer was
lost counting as a failure, and resolves on the next refresh that succeeds. The detail names the account, the
length of the run, the last error, and `outcome`: `failed` for a refresh the token endpoint refused, `uncertain`
for one whose answer never arrived, so the token may or may not have rotated upstream.

The four session kinds are events rather than conditions: a rule of one of them receives one delivery per event
as it happens, opens nothing, and has nothing to resolve. `session_active` receives `session_started` and
`session_ended`, `subagent_active` receives `subagent_started` and `subagent_ended`, `session_parked` receives
`session_parked` and `session_resumed`, and `session_idle` receives `session_idle`. A delivery goes to every
enabled rule of the kind whose scope matches the session at that moment, so a rule enabled mid-session receives
the session's later events and no start it missed; after a restart nothing is posted, since the router no longer
knows which sessions live, and a live session's next event is posted as it comes. A graceful stop posts the end of
every session it still tracks, so a session whose start was posted is never left open across the restart. Each
delivery lands in the event log as `lifecycle` with the event's name, and the Alerts view lists them as a log.

**Session active** posts `session_started` on a session's first request through the router, or on the keepalive
companion's `session_start` when that comes first, and `session_ended` on the companion's `session_end` or when
the session has been silent for `sessions.expire_after_seconds`, checked every second. A heartbeat from the
companion counts as a sign of life there, so a session whose user has stepped away is not ended while its client
is still attended. The detail's `source` says which side posted the event: `request`, `companion`, `expiry`, or
`shutdown`.

**Session idle** posts `session_idle` once no request has arrived on the session for the rule's threshold in
minutes, measured from the session's last request on any of its agents, once per crossing; the session's next
request arms the rule again.

**Session parked** posts `session_parked` when the companion reports the main agent parked and `session_resumed`
on the main agent's next request or when the companion reports the agent busy or idle again; without a companion
for the session neither happens.

**Subagent active** posts `subagent_started` on a subagent's first request, or on the companion's `agent_start`,
and `subagent_ended` when the companion releases the agent, when the session ends, or at the session's expiry.
The detail names the parent agent when the client sent `x-claude-code-parent-agent-id`.

**Utilization above threshold** fires when a usage sample for the chosen window reaches the threshold and
resolves once a sample reads five points below it.

For the other eight kinds at most one open event exists per rule and subject; an event resolves either on the
condition its description names or by hand, through Resolve. A webhook may not point at `localhost` or a
`.localhost` name, a loopback, link-local, or unspecified address; the refusal reads `webhook_url must not point
at {reason}`, and the same check runs again on every delivery against whatever address DNS actually answers with,
since a name can resolve differently over time.

A delivery posts a JSON body with the keys `detail`, `kind`, `opened_at`, `resolved_at`, `router`, `rule`,
`state` (`opened` or `resolved` for an event's two transitions, `event` for a session event, whose `opened_at`
is the event's time and whose `resolved_at` is null), and `subject`, in that order. When the rule has a secret,
the body carries an `x-router-signature: sha256=<hex>` header, an HMAC-SHA256 of the raw bytes keyed by the
secret; a rule with a secret never posts unsigned. Delivery makes three attempts, the first immediate and the
next two after waits of 1 and 4 seconds; each request has a 10-second timeout and follows no redirect, since a
redirect would carry the signature to a URL it was never computed for. A 2xx response counts as delivered, and
Test fires one synthetic delivery through the same path.

A viewer sees a rule's webhook host and never its full URL, since the URL can carry the receiver's credential in
its path or query.

![The Alerts view: the rules, the events with deliveries, and the lifecycle log](assets/alerts-light.png#only-light)
![The Alerts view: the rules, the events with deliveries, and the lifecycle log](assets/alerts-dark.png#only-dark)
