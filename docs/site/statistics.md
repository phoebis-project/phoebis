# Statistics and alerts

Every relayed request lands in the ledger, a second SQLite file beside the registry, and the console, the
analytics routes, the metrics listener, and the alert engine read from it or from the same events. This page
describes what is recorded, how long it is kept, what each view shows, and how an alert rule is written and
delivered.

## The ledger

Every relayed request lands in the ledger: one row for the client request and one row for each upstream attempt
it took. Once a request finishes, it is folded into the hour's rollup, which is what the analytics routes report,
so a long range costs one row per hour rather than one per request.

The request row names the key, the pool, the session, the agent, and the model; the kind and transport of the
call; the status and outcome; the error code; the time to first byte and to first token; the duration; the bytes
sent and received; whether the response was served kept warm; whether its body was captured; and the client's
user agent. The attempt row names the account and its kind and host; the upstream status and request id; the
outcome; the cooldown the attempt set; the five token counts; the cache miss reason the diagnostics beta reports;
the unified rate-limit window readings; overage; the cost split across input, cache read, cache write, and
output; and the pricing version the estimate used.

Percentiles for every millisecond latency and for output tokens per second come from a fixed, log-spaced,
64-bucket histogram. A range spanning several hours merges its hourly histograms bucket by bucket before reading
the percentile, and the value carries the error of the bucket it falls in.

## Quota samples

Quota samples arrive from two sources. Every response head from an OAuth account carries the unified rate-limit
headers, which cost nothing to read and cover whichever accounts are in use at the moment; a poll of the usage
endpoint, every `quota.poll_interval_seconds`, fills the gaps an idle account leaves and is the only source of
extra-usage credit totals.

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

## Body capture

Body capture is off until a key or a pool asks for it. Once asked for, a capture holds the request and response
bodies of that key or pool in the ledger for the hours it names, each body cut at 8 MiB if the stream ran longer,
and each captured body is dropped once its hours run out. `stats.body_capture_max_hours` is the ceiling a key or
pool may ask for, because the bodies are prompts and how long they are kept is the operator's call rather than
the caller's. Only the admin role reads bodies: a viewer's request for them is answered `403 captured bodies are
for administrators`. The console shows a captured body in the request drawer, with a control to copy it.

## Retention

| Data | Kept for |
| --- | --- |
| Requests, attempts, events, kept-warm marks | `stats.retention_days` (90 days by default) |
| Quota and extra-usage samples | `quota.sample_retention_days` (30 days by default) |
| Captured bodies | The hours the capture named |
| Hourly rollups | 400 days, fixed |

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

The `model` label is bounded by the price table: a model the table prices keeps its own id, and every other
model is reported as `other`, so a downstream caller cannot grow the label set by naming models it likes.
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
snapshot carries no preview whatever the role.

## The console's views

### Dashboard

Four tiles report requests, tokens, estimated cost, and the error rate over the last 24 hours; two cards below
them show the 5-hour and 7-day quota remaining for each subscription account; and the page ends with the most
recent requests and the active sessions.

### Observe

The metric strip reports eight measures: requests per second, active streams, the error rate, TTFB at p50, p95,
and p99, output tokens per second, and the estimated cost per minute. Below it, a live table lists requests as
they run, with filters, a pause control, and a range control for switching to a past window; selecting a request
opens a detail pane with its phase waterfall, and a Streams grid shows every stream still open.

![The Observe view: the metric strip, the live table, and the phase waterfall](assets/observe-light.png#only-light)
![The Observe view: the metric strip, the live table, and the phase waterfall](assets/observe-dark.png#only-dark)

### Quota

A card per upstream shows one row per rate-limit window, filterable by tabs for all accounts, API-key accounts,
and OAuth accounts. Below the cards, a reset timeline lays the same windows across a calendar, zoomable between a
two-week weekly view and a three-day 5-hour view.

![The Quota view: the cards per upstream and the reset timeline](assets/quota-light.png#only-light)
![The Quota view: the cards per upstream and the reset timeline](assets/quota-dark.png#only-dark)

### Request logs

The table lists thirteen columns: time, key, pool, upstream, model, client, kind, transport, status, attempts,
TTFB, tokens, and cost. One row is one client request however many attempts it took, and the attempt trail reads
as "mail 429, business 200" when a 429 on the first account failed over to a 200 on the second. Filters narrow
the table, and selecting a row opens a drawer with the request's detail.

![The Request logs view: the filterable table and the request drawer](assets/requests-light.png#only-light)
![The Request logs view: the filterable table and the request drawer](assets/requests-dark.png#only-dark)

### Analytics

A range control offers 1 hour, 24 hours, 7 days, 30 days, or a custom span, and a group-by control regroups
every chart and table by client, key, kind, model, pool, or upstream. Traffic reports request volume by outcome
and the error rate; Latency reports percentiles read from the merged per-hour histograms; Cache reports the hit
ratio, tokens that expired between turns, and the share of input served from cache; Cost reports the estimated
spend for the range; Cache warming reports what probes cost against what they saved; and Upstreams reports peak
utilization, ineligible time, and pin churn per upstream.

![The Analytics view: the range and group-by controls over six report sections](assets/analytics-light.png#only-light)
![The Analytics view: the range and group-by controls over six report sections](assets/analytics-dark.png#only-dark)

### Settings

Settings covers appearance, access (the admin and viewer secrets and what each may do), pricing, sessions, retention,
keepalive, listeners, the downstream client block, and an about panel.

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
when the session expires.

**Client drift** fires when the billing-hash self-check fails for a client request, because the client stamped a
hash this build cannot reproduce; re-pinning the router to that client build and resolving the event by hand
clears it.

**Funding required** fires when an API-key upstream is refused for billing reasons and resolves when an operator
clears the funding flag.

**Probe cost cap reached** fires when a pool's daily probe cost cap is reached; probes pause until the next UTC
day, which resolves it.

**Pool exhausted** fires when a request finds no eligible upstream in the pool and resolves on the next request
the pool serves.

**Re-auth required** fires when an upstream enters the reauth-required state and resolves once the upstream is
paired or verified again.

**Credential refresh failed** fires when a credential refresh fails twice in a row and resolves on the next
refresh that succeeds.

**Session active** opens on a session's first request through the router, or on the keepalive companion's
`session_start` when that comes first, and resolves on the companion's `session_end` or when the session has been
silent for `sessions.expire_after_seconds`, checked every second. A heartbeat from the companion counts as a sign
of life there, so a session whose user has stepped away stays open while its client is still attended. The opened
and resolved deliveries are the start and the end of the session. After a restart every open lifecycle event is
resolved at once with `reason: restart` in its detail, since the router no longer knows which sessions live; a
live session reopens on its next request.

**Session idle** opens once no request has arrived on the session for the rule's threshold in minutes, measured
from the session's last request on any of its agents, and resolves on the next request.

**Session parked** opens when the companion reports the main agent parked and resolves on the session's next
request, or when the companion reports the agent busy or idle again; without a companion for the session neither
happens. Resolving one by hand ends the event and leaves the router's record of the park as it was, so nothing
reopens while the session stays parked; the session's next request, or the companion's next report of the agent
busy or idle, arms the condition again.

**Subagent active** opens on a subagent's first request, or on the companion's `agent_start`, and resolves when
the companion releases the agent, when the session ends, or at the session's expiry. The detail names the parent
agent when the client sent `x-claude-code-parent-agent-id`.

**Utilization above threshold** fires when a usage sample for the chosen window reaches the threshold and
resolves once a sample reads five points below it.

At most one open event exists per rule and subject; an event resolves either on the condition its description
names or by hand, through Resolve. A webhook may not point at `localhost` or a `.localhost` name, a loopback,
link-local, or unspecified address; the refusal reads `webhook_url must not point at {reason}`, and the same
check runs again on every delivery against whatever address DNS actually answers with, since a name can resolve
differently over time.

A delivery posts a JSON body with the keys `detail`, `kind`, `opened_at`, `resolved_at`, `router`, `rule`,
`state` (`opened` or `resolved`), and `subject`, in that order. When the rule has a secret, the body carries an
`x-router-signature: sha256=<hex>` header, an HMAC-SHA256 of the raw bytes keyed by the secret; a rule with a
secret never posts unsigned. Delivery makes three attempts, the first immediate and the next two after waits of
1 and 4 seconds; each request has a 10-second timeout and follows no redirect, since a redirect would carry the
signature to a URL it was never computed for. A 2xx response counts as delivered, and Test fires one synthetic
delivery through the same path.

A viewer sees a rule's webhook host and never its full URL, since the URL can carry the receiver's credential in
its path or query.

![The Alerts view: the rules table and the open and resolved events with deliveries](assets/alerts-light.png#only-light)
![The Alerts view: the rules table and the open and resolved events with deliveries](assets/alerts-dark.png#only-dark)
