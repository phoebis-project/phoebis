# Changelog

This page is for the operator reading what a release holds before an upgrade, with one section per release starting at
the first release, 0.1.0.

## 0.1.0

The router relays Claude Code's requests to `api.anthropic.com` under the credential of the account it selected, with
the body and header edits [The relay](relay.md) lists. The billing fields a reduced-shape request lacks are restored,
each only when absent ([The restoration](relay.md#the-restoration)), two drift checks refuse a client build the router
is not pinned to with `403` `client_drift` before any account is chosen ([Client drift](relay.md#client-drift)), and
every upstream response head is classified before a byte reaches the client, so a rate limit fails over to a sibling
([How a head is classified](relay.md#how-a-head-is-classified)). A request whose client leaves before the head lands in
the ledger as a canceled request with the code `client_gone`, keeping the attempts it reached
([Statistics](statistics.md#the-ledger)). The router's own reading of a stream holds at most 512 KiB of one event while
that event is still arriving: an event that outgrows the bound before its end is skipped and counted on the request's
completion, the events after it still reach the ledger, and an event that arrives whole in one decoded piece is parsed
whatever its size ([How a head is classified](relay.md#how-a-head-is-classified)). A stream cut, stalled or undecodable
after the head, or ended by the account's cancel, logs one warning with the error's words and lands as a
`stream_interrupted` event on the request ([How a head is classified](relay.md#how-a-head-is-classified)). Every coded
answer is decoded and forwarded as identity with `content-encoding` dropped, the four codings gzip, deflate, brotli and
zstd, and any other coding answers `502` `upstream_undecodable` before any byte reaches the client
([The edits toward the client](relay.md#the-edits-toward-the-client)). A resumed conversation's version suffix
reproduces over the empty prompt the client stamps, and a refusal on a resumed conversation names the resume
([Client drift](relay.md#client-drift)). A billing line the strict decoder refuses for a lone surrogate escape is
checked for its version suffix on the tolerant read and refused as `version_suffix` drift when the suffix does not
reproduce ([Client drift](relay.md#client-drift)). A session that switches model between its turns names no previous
message in its cache diagnostics on its first request to another account, like any move
([The body edits](relay.md#the-body-edits)). A billing line the strict decoder refuses for a lone surrogate escape is
refused for its shape as `400` `body_shape` on a request whose line the relay restores
([The body edits](relay.md#the-body-edits), [Refusal codes](client.md#refusal-codes)).
A body carrying a fallback credit token from a refusal is placed on the account that issued it, the relay keeping the
issuer under the token's digest in memory for an hour ([Admission](relay.md#admission)).
The router requires the session header, the class header, the attribution line, `metadata.user_id` and the agent and
compaction headers of a Claude Code request and refuses the rest `403` `client_contract` ([The required
set](client.md#the-required-set)), restores `cc_prompt_id` from the client's prompt-id header and `cc_prev_req` by
class, blanks the account uuid on an API-key leg, prices the client's auto-mode classifier calls that carry the
classifier marker at no share of a plan window and, where the plan pays for the call, no dollars, and sends each of
them, best effort and without a pin, to the account its judged turn ended on where it can, because the upstream charges
a classifier call that lands on another account than the turn it judges ([Auto mode behind the
router](relay.md#auto-mode-behind-the-router)). Every caller-credential refusal (`auth_ambiguous`, `auth_malformed`,
`auth_missing` and `auth_unknown`, the registry's key refusal among them) carries `x-should-retry: false` on every route
that answers it, so the client does not resend it ([Refusal codes](client.md#refusal-codes)). The router is measured
against Claude Code 2.1.310 on Linux x64, the build the profile and the fixtures come from ([The pinned client
build](development.md#the-pinned-client-build)). A version suffix refusal names the anchors the router tried and the
marks a subagent's request carries, and says that a re-pin does not help where the session's main turns pass ([Client
drift](relay.md#client-drift)). A request refused for an account's malformed stored uuid files as rejected, and every
answer of the side routes carries `Cache-Control: no-store`, a refusal of a malformed path among them ([When nothing can
serve](routing.md#when-nothing-can-serve), [What the proxy accepts](client.md#what-the-proxy-accepts)). A body holding
more than 1,000,000 elements is refused `400` `body_shape` before it is parsed, so a body of many tiny elements cannot
build a parsed tree past about 346 MiB, what nested single-entry objects (the dearest shape measured) build at the bound
([Admission](relay.md#admission)).

A key routes into one pool ([Pools, keys and routing](routing.md)), and every request it presents is placed on one
account by the session's pin, the pool's strategy and each account's quota. A live pin moves only by one of five rules,
spent, overage, return to plan, drain and shed, every move passing a runway gate on its destination (the pin's cold
rewrite and the burn of its sessions carried to the window's horizon under the pool's ceiling) and landing where the
pool's strategy ranks the accounts the session fits, the longest runway breaking the strategy's tie, `return_to_plan`
choosing when a session on an API-key account returns to a subscription account (`always`, `when_cold` or `never`, with
`return_to_plan_dwell_seconds` under `always`), the shed rule reading every plan window's projection over the ceiling at
its horizon after a wait, a spent window about to reset waited out in place, and the routing log recording a shedding
mark that clears as `mark cleared`, a reservation that leaves without its usage as `reservation expired` and a spent
window that drops back under its reset as `window cleared` ([Moves](routing.md#moves), [The
windows](routing.md#the-windows-the-router-works-from), [Routing log](console.md#routing-log)). With a pool's
`slow_down` on, the sessions of an account whose 5-hour window would pass the ceiling before its reset are held before
each request for the pin's cycle times one over the account's speed factor less one, up to `slow_down_hold_seconds`,
and, with `load_shedding` on, the round on that window fires once the time the slow-down adds has passed
`slow_down_stretch_seconds` for a safety wait, its horizon moved earlier by the threshold, so it removes only what the
slow-down cannot cover ([Slow-down](routing.md#slow-down)). The slow-down sizes a hold from the mean of a session's last
eight gaps and leaves a pause of one burn average or longer out of it, ramps its holds up over one burn average after a
stretch first reads over zero, and clears its entry at the 5-hour reset, a window's wall is announced once per reset
cycle, the first request after a move names no previous message in its cache diagnostics, and the moved pin's next
request waits for that request's head before it goes out, so a move pays its rewrite once. A quota wall's cycle covers a
head that names no reset while the announced reset lies ahead, and the cycle that holds a head within two points of the
wall runs from the announcement's instant, or from the reset's passing for a wall that goes on past its reset
([Statistics](statistics.md#conditions-and-gauges)). A request tries each eligible account at most once, the cooldowns
classified by account kind, and a failover lands the session as a move whose record and event carry its cause
([Cooldowns and failover](routing.md#cooldowns-and-failover)), and `overage_policy` chooses what happens once an account
bills extra usage ([Overage](routing.md#overage)). A pin whose model does not draw the fired window leaves the round, no
5-hour round fires in the window's first two burn averages, the cheapest pick compares a rewrite as the Pro share of the
pin's weekly window and records it as the Pro-week figure with the window it compared on, which the routing log reads in
the chosen week, and the projection, the runway and the stretch weigh the band's hours inside the horizon. A moved pin's
stamp names how it moved, a shedding round or the failover that landed it, which the status page's pins block and the
routing log's record card read as `shed from` or `failed over from` the account it left ([Is it
up?](operations.md#is-it-up), [Routing log](console.md#routing-log)). After a slow-down round a `wall_reached` condition
gives how long before the window's reset the wall arrived in place of the projection error, and the ledger keeps each
account's longest of the hour ([Conditions and gauges](statistics.md#conditions-and-gauges)). The shed rule's wait
belongs to the window whose reset its first reading over was read under, so a reading whose reset moved an hour or more
opens a new window whose wait starts again ([Moves](routing.md#moves)). A pin only auxiliary requests use stays warm
while its last use is younger than its cache TTL ([Moves](routing.md#moves)). A shedding round that walks no pin and
leaves one out off the window that fired reads `off_window`, and while a subscription account runs on its plan the round
orders its pins by the span's average it prices them by ([Moves](routing.md#moves)). A placement's own fit and Pace
score leave the session's own standing row on the account it left out of the burn ([Pools, keys and
routing](routing.md#the-windows-the-router-works-from)). A usage-based Enterprise seat, classed from its stored profile,
is placed as a paid source beside the API-key accounts and never reads as in overage ([Two kinds of
account](pairing.md#two-kinds-of-account), [Selection](routing.md#selection)). A usage-based seat whose spend share
reads one or more is held out for every model until the spend window's reset, the share expires at the reset it was read
under, and the fast lane's credits read follows the same expiry ([Overage](routing.md#overage)). A usage-based seat's
poll share and a share-only head's share lapse at the head reset the router holds from a head observed at or before the
reading while that reset is still ahead of it, and the account view's `spend` reads the share's lapse from the router
wherever the router holds the reading ([Overage](routing.md#overage)). A session's runway that reaches past its binding
window's next reset reads unbounded on `GET /admin/sessions` and in the decision record's fit table, while the fit's
verdict reads the computed figure against the horizon ([The windows the router works
from](routing.md#the-windows-the-router-works-from)). A request whose connection could not be opened is retried once
after about a second, a reused connection that died is retried at once, and a request unsent twice answers 502
`upstream_unreachable` ([Cooldowns and failover](routing.md#cooldowns-and-failover)). A window read under its limit
after a predicted spent edge emits `window_cleared` as a stored reading's fall does, and the clear ends the attention
condition the spent event built ([The windows the router works from](routing.md#the-windows-the-router-works-from)). A
restored wall keeps its anchor across a restart, so within-margin readings after the start do not hold the account spent
past the window's length ([The windows the router works from](routing.md#the-windows-the-router-works-from)). A
session's creation time is its main agent's first placement, kept across a failover, a move and a second pin, so the
Sessions view's order by creation is stable ([Routing](routing.md#selection)). A usage-based seat's old plan readings
start no shedding round ([Routing](routing.md#moves)). An OAuth account's shared windows are the ones its newest clean
poll carried, both before the first, so a poll that lacks the 5-hour window drops the account's reading of it and its
charges, ends its slow-down entries and leaves the window out of the prediction, the projection, the reservation and the
burn, while a head drops nothing and a head's reading of a window the account lacks does not land ([The windows the
router works from](routing.md#the-windows-the-router-works-from)).
A decision record names a seat its spend cap holds out `spent` and an account the `never` policy holds out until its
overage ends `overage`, in its standing facts, its trace, its candidates' obstacles and its probe line, as its
exhausted line does ([Overage](routing.md#overage)). A client classifier call that runs off the account its judged turn
ended on, or in a session with no turn to judge, is charged as any call and recorded `client_off`, and a classifier call
directed to the account its judged turn ended on moves no pin ([Clients](routing.md#clients)). A guardrail create that
loses the race for its id to another create answers the route's 409 for a taken id, as the key and pool creates do, and
a guardrail, key or pool edit whose row is deleted while it is being saved answers 404 ahead of a stale stamp's 409
([API reference](api-reference.md)). A pool member's `eligible` word on `GET /admin/pools` reads `unknown` for a member
the router has not loaded yet and `subscription_ended` for one past its subscription's end, apart from a deleted, paused
or unready member's `unavailable` ([API reference](api-reference.md)). The slow-down's ramp grows from the span's own
start, so a return to full speed shorter than the span's wait continues it, the account view names the ramp,
`GET /admin/sessions` and `GET /admin/pools` name the factor and the realized speed, and a rate-limit claim on a window
the account's plan dropped recovers on the windows it has ([Slow-down](routing.md#slow-down), [Cooldowns and
failover](routing.md#cooldowns-and-failover)). A slow-down hold never runs past the 5-hour window's reset, and a request
after the reset is read afresh ([Slow-down](routing.md#slow-down)). A reading that moves a window's reset later while
the stored one still lies ahead replaces it with no `window_reset`, and a clean poll that drops the 5-hour window
records the end of the account's slow-down in the routing log at the drop ([The windows the router works
from](routing.md#the-windows-the-router-works-from)). A placement that replaces a live pin on another account is
recorded as a move with the reason the pin left for, and a placement after an idle expiry names no account it left
([Moves](routing.md#moves)). A shedding round leaves out a pin whose agent ended and a pin with no burn on the fired
window, and marks a pin in place of a covering one only where its burn covers a tenth of what is left to shed, so a
round never rewrites a session for a sliver ([Moves](routing.md#moves)). A move fits a destination only where its room
covers half the moved pin's burn beyond the pin's own, a fresh placement keeping the ceiling, and an ended pin's burn
leaves the projection until its next request ([Moves](routing.md#moves)). A window's round waits a quarter of the time
from its first reading over the ceiling to the wall instant its burn projects, at most two burn averages, so a burst
toward a near wall instant fires early and a slow climb waits, and the routing log names the wall instant
([Moves](routing.md#moves)). A shedding round and the heal of a standing mark test a window against the wall where its
horizon is the account's end or the window's own reset, since the quota left there is lost, and against the ceiling
elsewhere ([Subscription end](routing.md#subscription-end)). A move's fit reads the windows the destination's window set
holds, so an account whose set lacks the 5-hour window can take a shed move ([Moves](routing.md#moves)). A pin's
entrypoint and effort read null where the turn named none ([Sessions](console.md#sessions)), and the routing log's
cooldown chip names the model a cooldown holds ([Routing log](console.md#routing-log)).

With `keepalive.enabled` on a pool the router keeps a replayable copy of every eligible request a session sends and,
shortly before the cache entry would expire, sends a probe that reads it ([Keepalive](keepalive.md)). The policy's
ceilings, `max_idle_seconds`, `max_parked_seconds` and `scheduled_horizon_seconds`, bound how long a recipe is probed,
the parked and horizon ceilings capped at their break-even and the idle ceiling taken as set ([The four
fields](keepalive.md#the-four-fields)). The router reads session liveness and agent state from the companion mod, a
Claude Code plugin inside the client's process that holds no policy ([The companion mod](companion-mod.md)), over [The
keepalive protocol](relay.md#the-keepalive-protocol). A probe kept past its recipe's end follows a model's resolution to
its dated id, the kept-warm credit goes to the newest probe that covered the entry over a later one that did not cover
it, and a failed turn decides no probe, so the retry after it earns the credit ([Which requests re-arm a
recipe](keepalive.md#which-requests-re-arm-a-recipe)). A chain of probes on one model, speed and account, where each
later probe read the entry back, settles against its last probe's entry, so a request on that lineage that reads the
cache inside that entry answers every probe of the chain, and a completed compaction stops the recipe until the next
main turn ([Which requests re-arm a recipe](keepalive.md#which-requests-re-arm-a-recipe)). A pool's idle ceiling above
the break-even is taken as set, and the resolved share can read above one for it ([The
ceilings](keepalive.md#the-ceilings)). A kept-warm credit's saving prices under the regime of its request's client
([What the ledger records](keepalive.md#what-the-ledger-records)). An arrival after a completion the keepalive missed
measures the kept-warm gap from the turn that answered, and a probe under a twin spelling of a dated model id is dated
([Which requests re-arm a recipe](keepalive.md#which-requests-re-arm-a-recipe)). The companion mod keeps its owner and
instance across a `/clear` and retries a start the router refused for a live holder once that holder's lease lapses, and
the router warns once per session at the first request that names another owner while the holder's lease is live and
lets such a request take the session over once the lease has lapsed ([The owner
header](companion-mod.md#the-owner-header)). A released agent's record leaves the engine one pool cache TTL after its
release, freeing its slot of the agent cap ([The five states](keepalive.md#the-five-states)). A failed or canceled
request leaves the last answered recipe armed, and a probe's transient failure retries on a ladder bounded by the
entry's expiry ([The five states](keepalive.md#the-five-states)). A message's text-only content array hashes as its
blocks up to the last one carrying the cache marker, a single block as its text, an idle window counts a return on
either of the last two message boundaries of the request before it, and a refreshing request whose chain holds neither
boundary while its answer read the cache is logged and counted per client version, every chain hash of such a body
moving once at the deploy, so the cache map and the anchors of running sessions start cold
([Keepalive](keepalive.md#the-return-rate-by-idle-age), [Relay](relay.md#the-boundary-chains)). The idle-return counters
are kept by pool and UTC day, so the keepalive overview answers the return rate over the last 7 or 30 days, the counters
starting afresh at the deploy ([Keepalive](keepalive.md#the-return-rate-by-idle-age)). The companion mod keeps a
subagent the router named released unannounced when a later answer omits it, until its next report, so a released
record's departure frees its slot ([The keepalive protocol](relay.md#the-keepalive-protocol)). The keepalive prices its
probes, the savings and the break-even in the 7-day window of the account's tier, the window every plan keeps, and the
console shows the probe cost and the saving in the chosen week ([The four fields](keepalive.md#the-four-fields), [What
the ledger records](keepalive.md#what-the-ledger-records)). The keepalive reads a request's cache markers at the five
positions the Messages API takes one from, a block inside a tool result's content among them, so a marker there that is
not ephemeral or names an unknown TTL leaves the request not retained under `invalid_cache_control` ([The five
states](keepalive.md#the-five-states)). A session the mod registered holds no slot under the session and agent caps
until its first request, and a first request over either cap is not kept and reads `session_limit` or `agent_limit`
([Turning it on](keepalive.md#turning-it-on)). No probe goes out past an account's subscription end, and the wind-down
holds a probe due within its interval of the end ([How a probe works](keepalive.md#how-a-probe-works)). A request on a
pool with the keepalive off or from a key without the `keepalive` permission is recorded at its arrival with no body
kept ([The five states](keepalive.md#the-five-states)), and a kept-warm credit files under the tier its request row
files under ([What the ledger records](keepalive.md#what-the-ledger-records)). An idle return also counts where the
request carries the deepest boundary the earlier request marked for the cache, so an interrupted turn's next request
reads as a return and the chain watch stays quiet ([The return rate by idle
age](keepalive.md#the-return-rate-by-idle-age)). A request whose billing line holds a `cch=` the replay cannot read or
reach is not retained ([The five states](keepalive.md#the-five-states)). The companion mod records a release the router
named for a subagent and does not send it again ([The keepalive protocol](relay.md#the-keepalive-protocol)).

The admin listener serves the web console behind a login with the admin secret, with the views in the sidebar's order,
Dashboard, Quota, Accounts, Pools, Keys, Guardrails, Requests, Analytics, Sessions and Settings, and under them the
Diagnostics group holding Routing log and Meter ([The console](console.md)). Every list view opens on one filter bar
holding the slots it has in one order: the range pills, the search, Add filter with its filters as chips under the bar,
the view's own controls, and at the right edge a split Refresh button, whose dropdown sets auto reload, and the count of
what the view lists. A count of a thousand or more groups its digits on every view, such as 1,204 requests
([The console](console.md)). Every list view's filter bar holds a Sort control whose key and direction travel in the
address ([The console](console.md)). `/models` opens the Keys view, where a saved key's dialog shows its Effective list,
the accounts cooling for a model show on the Accounts view and the model rejected and model permitted rows in the
Routing log, and a model a decision card, an event's detail or a log row names opens the Routing log on its rows over
the last day ([The shell](console.md#the-shell), [Keys](console.md#keys), [Routing log](console.md#routing-log)). The
Accounts, Analytics and Pools views tell a bundle that changed under the tab from an error while drawing, each with its
own words and a Reload button ([The shell](console.md#the-shell)). The console shows a reload banner when the router
answers from a newer build than the open tab's bundle, and an attention condition the console does not know reads as
Unknown condition with its kind on hover ([The shell](console.md#the-shell), [Dashboard](console.md#dashboard)). The
expert view, a switch in the sidebar's foot, shows every field of the pool and key dialogs, the advanced ones included,
and the sidebar's Diagnostics group ([The shell](console.md#the-shell)). The login screen offers "Stay logged in", which
keeps the browser logged in for 30 days from the last visit, across a router restart, until a secret rotation or a
logout of a kept login, which ends every kept login ([The shell](console.md#the-shell)). The top bar wraps a view title
it cannot hold beside its controls onto a second line at a phone's width, so the Routing log page fits the window's
width ([The shell](console.md#the-shell)). The unit legend hangs from its button as the console's other popovers do,
takes the focus when it opens, closes on Escape, a click outside it or the focus leaving it, and hands the focus back to
its button when it closes on Escape, and the Settings view's Appearance rows wrap their controls under their labels at a
phone's width ([The shell](console.md#the-shell), [Settings](console.md#settings)). A key's processed rules are read on
`GET /admin/keys/{id}/models` and in the key dialog, and the Model unpriced attention item opens no page
([API reference](api-reference.md)). Each `GET /admin/sessions` row reads from the main agent's conversation pin, or the
newest conversation pin of another agent where main holds none, and lists the row's conversation pins as agent lines
with their effort and their warmth, a session no companion reports leaving the list after the non-companion timeout of
the sessions block ([API reference](api-reference.md), [Installation](installation.md#sessions),
[Keepalive](keepalive.md#in-the-console)). A row also carries the newest client request's `user_agent`, the session's
spend over its life under its key as `spend`, its client requests of the last 24 hours by class and their spend as
`day`, whether its pin's account still holds the prompt cache as `warm`, one entry per agent as `agent_requests`, with
its requests and their spend over every model the agent ran, and main's lifecycle word as `lifecycle`, and each agent
line carries its pin's burn per hour as `burn`, `burn_unit`, `burn_window`, `burn_equivalent` and `burn_at` and its
agent's own `lifecycle` ([API reference](api-reference.md)). The admin API is documented by an OpenAPI document the
router serves, with an interactive reference page on the admin listener ([API reference](api-reference.md)). The
account's `has_extra_usage_enabled` reads the stored profile's `organization` member, as its rate-limit tier does
([API reference](api-reference.md)). The account create and edit refuse a body that is not a JSON object with
`body must be a JSON object`, and the pool members route answers `body must carry a members list` to an object without
one ([API reference](api-reference.md)). A key or pool create that loses the race for its id to another create answers
the route's 409 for a taken id, as the account create and the pairing confirm do ([API reference](api-reference.md)).
`GET /admin/dashboard` answers the last day's figures and the live pin count, and the account list carries each
account's quota windows ([API reference](api-reference.md), [Dashboard](console.md#dashboard)). The attention line's
slowed-long badge reads the held time of the last 24 hours and the requests held ([Dashboard](console.md#dashboard)).
The Dashboard, Guardrails, Pools and Settings views fit a 360 px window with no horizontal scroll
([Dashboard](console.md#dashboard), [Guardrails](console.md#guardrails), [Pools](console.md#pools),
[Settings](console.md#settings)). The Dashboard's account cards and the Accounts list read
`No window since the plan ended {ago}` on an account whose plan ended ([Dashboard](console.md#dashboard),
[Accounts](console.md#accounts)). The Dashboard's attention line keeps one row and folds the items past it behind a "+N
more" badge, and a pool card's attention rows end with their age ([Dashboard](console.md#dashboard),
[Pools](console.md#pools)). The Dashboard's attention line reads the keepalive engine's three conditions, each opening
the Sessions view, and `GET /admin/attention` and the `phoebis_attention` gauge carry them as kinds:
`keepalive_bus_gaps` while the engine missed bus events in the last 24 hours, `keepalive_cap` while one of its caps
stands at 90 percent of its max or more, and `expired_between_turns` while prompt caches expired between turns on a pool
whose keepalive policy is on, under a key the keepalive may probe for ([Dashboard](console.md#dashboard), [Conditions
and gauges](statistics.md#conditions-and-gauges)). The Dashboard's Estimated cost tile reads the day's subscription
traffic in the chosen week under Native ([Dashboard](console.md#dashboard)). The Dashboard's Sessions card counts the
live sessions' agents, busy, idle and parked by lifecycle word, as the Sessions view's summary line does at its bare
address, and its Keepalive engine block reads the engine's three caps, the day's cache probes by outcome and the
requests that expired between turns beside the bus gaps, the bus gaps' hover reading the last gap's instant from
`last_bus_gap_at` on `GET /admin/keepalive/overview` ([Dashboard](console.md#dashboard),
[API reference](api-reference.md)). The Dashboard's two rings, 7d remaining and Fable week remaining, weigh each
subscription account by its tier's 7-day capacity over the chosen week's tier's and read their lines in the chosen week,
such as 0.39 of 1.30 Ultra weeks left, and the rings and the Plan per hour card price a Team or Enterprise seat at its
tier where the cost model holds the row, the rings naming an account whose poll lacks the window, a free plan, a
usage-based seat or an unheld tier under the ring with the reason, and the card reading an account the router prices no
tier for whose range holds no priced bucket as tier unknown ([Dashboard](console.md#dashboard),
[Analytics](console.md#analytics)). The Dashboard's rings list each account with the most of its own window left first
([Dashboard](console.md#dashboard)). The Dashboard's rings read the week's unpriced sentence where the chosen week's
tier has no 7-day capacity ([Dashboard](console.md#dashboard)). A Quota card marks every window whose reset is its
soonest in the accent ink, the Fable row and a seat's Spend row among them ([Quota](console.md#quota)). A Quota card's
Refresh draws its reading the moment it answers, at the reading's instant ([Quota](console.md#quota)). A usage-based
seat draws one spend bar wherever the window bars are drawn, with its amounts, its reset marked estimated until a header
names it, and a lane on the Quota windows timeline ([Quota](console.md#quota)). An account in the `disconnected` or
`reauth_required` state reports no quota window, so neither a console view nor the status screen draws its last reading
([Quota](console.md#quota)). Each window the accounts route serves carries its lead over pace, which the console's pace
words, pace mark and pace orders read ([Quota](console.md#quota)). On a screen under 900 px wide the Quota cards' reset
label takes a line of its own under the meter ([Quota](console.md#quota)). The Quota cards sort by the bar's Sort
control, on Default order in the route's order, on a pace key per window with the account furthest ahead first, or on a
recovery key per window with the soonest reset first and a usage-based seat among the accounts without that window by
name, and the Quota windows timeline draws each bar in its account's color, each lane stacking its account and its chips
above its bars on a narrow screen ([Quota](console.md#quota)). The Quota cards' reset line and the window bars of the
Dashboard and the Accounts list read the share the projection leaves, such as `19% left at the reset`
([Quota](console.md#quota)). The Quota windows timeline draws its now marker and its day grid over the bars, so the
marker stands at the instant's place on them and no grid line crosses a lane's stacked account and chips on a narrow
screen ([Quota](console.md#quota)). The Quota view leaves out an account that is disconnected or needs
re-authentication, which keeps its row and its state on the Accounts view ([Quota](console.md#quota)). The Quota view
leaves out an account whose plan ended while its flag stands, as it leaves out a disconnected one, and the Accounts
detail's Usage card names the ended plan ([Quota](console.md#quota), [Accounts](console.md#accounts)). The speed chip
reads the speed the holds realized, and its hover names the factor the pool computed and how far the span has ramped
([Quota](console.md#quota)). Under the 12-hour time setting the Quota timeline's short cells read "6 AM" in a cell wide
enough for it, with fewer cells at a given width ([Quota](console.md#quota)). A pairing confirm on a slot sealed under
another storage key answers 409 and names the account, so the dialog can offer Disconnect
([Pairing](pairing.md#with-curl)). A pairing whose confirm meets a credential sealed under another storage key reads the
router's sentence and offers Disconnect, which clears the credential and starts the pairing again from its first step
([Accounts](console.md#accounts)). A usage poll's 401 moves the account to `reauth_required`, and any other failed poll,
except a 429 or the usage 403 an ended plan explains, logs one warning and writes a `poll_failed` event
([Pairing](pairing.md#account-states)). A usage-based seat, the rate-limit tier `default_claude_zero` or the seat tier
`enterprise_usage_based`, reads no overage word on the Accounts view, and its Identity list reads "usage-based" beside
its rate-limit tier ([Accounts](console.md#accounts)). An account whose plan ended reads Free alone beside its Plan
ended row, the plan the router resolved after the end ([Accounts](console.md#accounts)). An account whose plan ended
reads the instant in its Identity list until a profile read or a pairing finds a plan again
([Accounts](console.md#accounts)). An account whose profile reports no plan is flagged plan ended, which pauses its
usage poll and places nothing on it until a profile read or a re-pair finds a plan again
([Pairing](pairing.md#account-states)). While an account's plan-ended flag stands, `GET /admin/accounts` and `/{id}`
answer its `windows` empty, as they do for a disconnected account ([Pairing](pairing.md#account-states)). Flagging the
plan ended closes an open usage-poll backoff run, so `poll_backoff` clears with the pause
([Statistics](statistics.md#conditions-and-gauges)). An account's plan reads as one word resolved from its profile's
organization type first and its rate-limit tier second (`free`, `pro`, `max_5x`, `max_20x`, `max`, `team`, `enterprise`
or `usage_based`), and a free organization or a usage-based seat prices no capacity, so the router reads it as an
untiered account ([Pairing](pairing.md#two-kinds-of-account), [API reference](api-reference.md)). An account's badge,
its Plan row and the Accounts search read the display name claude.ai sends where the account carries one and the plan
the router resolved from the organization type and the tier otherwise, Ultra, Max or Pro for the tiers the cost model
prices, "Free" for a free organization and "Usage-based" for a seat, as do the Quota card's chip and the capacity rings,
while the routing log's candidates read the label of the plan word a record carries ([Accounts](console.md#accounts),
[Dashboard](console.md#dashboard), [Quota](console.md#quota), [Routing log](console.md#routing-log)). An account's
detail draws "Allowance left at reset", each weekly window's share left unused at its last four resets in the chosen
week under every position, a term's end counting as a reset marked "term ended" ([Accounts](console.md#accounts)). An
account's detail draws "Meter drift", the newest completed window of each kind against the table with the windows off
the band in 30 days, opening the Meter view on the account ([Accounts](console.md#accounts)). An account's end editor
names the browser's zone beside its field ([Accounts](console.md#accounts)). An account's end editor opens empty for a
running plan, and its hint names the date the plan's billing settings show as the end ([Accounts](console.md#accounts)).
An extra usage reading with no amounts draws its state alone, with no amounts and no meter, on the Accounts view's Usage
card and the Quota cards ([Accounts](console.md#accounts), [Quota](console.md#quota)). The account's cancellation flag
is spelled `canceled` on the account view, the account edit, the routing log's term facts and the `wind_down_started`
event, with the error code `account_canceled`, the probe outcome `canceled` and the pairing session's status `canceled`
([Accounts](console.md#accounts), [API reference](api-reference.md)). The Accounts view orders its list through the
filter bar's sort control, a key and a direction carried in the address, with Name ascending by default, one key per
window's reset and Most used filing an account with no plan window last ([Accounts](console.md#accounts)). The Accounts
view shows a paired account's first reading once the poll a confirm started lands, a few seconds after the confirm, with
"The first quota reading is in flight." in its Usage card until then and a read of the list every 2 s for up to 30 s
while auto reload is off ([Accounts](console.md#accounts)). The allowance block and the Analytics card read each weekly
reset in the chosen week and price it at the tier the account ran under at that reset ([Accounts](console.md#accounts),
[Analytics](console.md#analytics)). A new pool's dialog starts with cache warming off ([Pools](console.md#pools)). A
pool card's Active pins fact counts every pin under the pool's keys, as the Dashboard's Sessions card does, and reads
how many sit on a former member under the figure ([Dashboard](console.md#dashboard), [Pools](console.md#pools)). The
pool card's Edit and Delete buttons stay together at the right at every width ([Pools](console.md#pools)). The pool
dialog holds the save on a member budget field the browser cannot read as a number ([Pools](console.md#pools)). The pool
dialog's idle window set above the break-even reads a note that the ceiling lies above it and prices its cost from the
set value, while the parked and horizon ceilings stay capped at the break-even ([Pools](console.md#pools)). The Pools
dialog's return rate reads the last 7 or 30 days ([Pools](console.md#pools)). The Pools view names the third selection
tier `metered` in its serving table and its Selection hint, the tier holding the API-key accounts and the usage-based
seats ([Pools](console.md#pools)). The Pools view orders its cards through the filter bar's sort control by Name,
Status, Members, "Requests, 24h", "Cost, 24h" or Active pins, starting on Name, and a card's links into the pool dialog
carry the order, so the dialog closes onto it ([Pools](console.md#pools)). The Keys view orders its table through the
filter bar's sort control by Name, Created, Last used, "Requests, 7d", "Tokens, 7d", "Cost, 7d", Expiry or Status,
starting on Name, and while Created or "Requests, 7d" is picked each row reads its creation date or its week's requests
under its name ([Keys](console.md#keys)). The Keys view's Pool filter draws one chip per pool, and a pool the address
names that the router does not list shows its chip until the pools load ([Keys](console.md#keys)). The Guardrails view
orders its rows through the filter bar's sort control by Name, Mode, "Matches, 24h", Created or Last updated, starting
on Name, and while Created or Last updated is picked each row reads its date under its name
([Guardrails](console.md#guardrails)). A request row opens its drawer on a click, Enter or Space and closes it the same
way, and a request the drawer could not load draws its error with Try again ([Requests](console.md#requests)). A request
whose answer the upstream refused draws the refusal's category as a warning badge beside its status, and the Outcome
group of Add filter offers Stop reason and Refusal category ([Requests](console.md#requests)). A request whose stream
was cut after its head reads what cut it in its drawer's Error row ([Requests](console.md#requests)). A request's "(in
the band)" reads the band at the request's completion on the cost revision in force at its receipt
([Requests](console.md#requests)). An auto reload on a custom range reads once a request received inside the range and
written after an earlier read, as a preset range does ([Requests](console.md#requests)). Each captured body block in the
request drawer has a find field that counts the lines holding a term, opens the wide window on a hit, marks every
occurrence on the drawn rows and walks the hits with Enter and Shift+Enter ([Requests](console.md#requests)). A request
row of `GET /admin/requests` and a request's captured bodies carry no request truncation flag, since the router stores a
captured request whole, and the bodies' byte counts say what a cut dropped ([API reference](api-reference.md),
[Requests](console.md#requests)). A pool member's `eligible` word on `GET /admin/pools` reads `spent` for a usage-based
seat at its spend cap and `budget_unknown` for a budgeted member while the ledger is degraded, each a reason the router
skips the member ([API reference](api-reference.md)). The Pools view's Standing reads `spend cap until {time}` for a
seat at its spend cap, `budget unknown` for a budgeted member while the ledger is degraded, `subscription ended` for a
member whose subscription ended and `unknown` for a member the router answers no reading of, and an exhausted pool's
status sentence names when a spent member's spend cap resets ([Pools](console.md#pools)). On the Routing log an
exhaustion that waits on a spend cap reads `spend cap` in its By chip, and a candidate at its spend cap reads
`spend cap reached until {time}` in its Verdict chip ([Routing log](console.md#routing-log)).
`GET /admin/analytics/burn` and `GET /admin/sessions` read each account's kind from the routing snapshot and answer no
`storage failure` ([API reference](api-reference.md)). On the Requests view and the Routing log a late row that lands
below the rows shown draws on the next auto reload read on any range whose rows fit on the page
([Requests](console.md#requests), [Routing log](console.md#routing-log)). On the Requests view and the Routing log the
list's late answer replaces the page an auto reload poll stood in for it, and Next past an overrun under a preset reads
to the router's clock ([Requests](console.md#requests), [Routing log](console.md#routing-log)). The pressed range pill
names its range and its end on hover, and when the tab comes back to a read at once the rows it brought keep their tint
through the read after it ([Requests](console.md#requests), [Routing log](console.md#routing-log)). The Requests and
Routing log Time cells print the date before the time on every row, and the Routing log's columns fit their content
([Requests](console.md#requests), [Routing log](console.md#routing-log)). The Requests and Routing log views filter on
an agent id through the Agent filter of the Subject group, carried in the address as `agent_id`
([Requests](console.md#requests), [Routing log](console.md#routing-log)). The Requests rows mark a request that
presented a fallback token, the Fallback filter lists them, and the Classifier filter offers `client_off`
([Requests](console.md#requests)). The Requests, Sessions and Routing log tables lay their columns out by one rule, with
a minimum per column kind, one fill column and a set wider than the card scrolling inside it, so the Requests table fits
1280 px with its default columns ([Requests](console.md#requests), [Sessions](console.md#sessions),
[Routing log](console.md#routing-log)). The Requests table orders by Time, Attempts, Duration, TTFB, Uncached input or
Output tokens through the Sort control and offers a Duration column ([Requests](console.md#requests)). The Requests
view's empty state reads "Widen the range or clear a filter.", as the Routing log's does, and a request the router
refused before it resolved a model reads a dash for its model ([Requests](console.md#requests)). Unchecking Select all
in a filter's list clears every check, the inverse of checking it, and an empty list keeps its meaning
([Requests](console.md#requests), [Routing log](console.md#routing-log)). A deleted key's group reads `<id> (deleted,
created <time>)`, its creation instant in the viewer's zone ([Analytics](console.md#analytics)). The Accounts cards
offer the three windows the router reads, and the Accounts in range table reads the Fable week's peak as Peak Fable
([Analytics](console.md#analytics)). The Accounts section's card "Unused allowance by account" ranks the subscription
accounts by the figure of their weekly windows left unused over the last four weeks, in the chosen week, with the
fleet's figures ([Analytics](console.md#analytics)). The Analytics session view narrows to one agent under the Agent
filter of the Subject group, carried in the address as `agent_id` and written by the Sessions view's agent menu, its
links to the request log carrying the agent, the filter disabled until a session is picked and leaving with the session
([Analytics](console.md#analytics)). The Analytics session view reads the session's rows through the route's session
filter, and each auto reload re-reads the first page alone ([Analytics](console.md#analytics)). The Analytics view's
breakdown tables head a failed read with the table's name ([Analytics](console.md#analytics)). The Analytics view's Burn
tab draws three plan cards, Plan per hour, 5-hour per hour and Fable week per hour, the subscription traffic per hour in
the chosen week, the burn route's 5-hour figure scaled to the chosen tier's 5-hour window, and the traffic of the models
that draw the Fable week, and of those alone, scaled to its Fable week, where a group with no such traffic over the
whole range draws no line, a bucket the cost model could not price staying a gap under a "model unlisted" entry
([Analytics](console.md#analytics)). The Analytics view's Cache warming tab spans its last card across the row where
that card would sit alone, as the Burn tab does ([Analytics](console.md#analytics)). The Analytics view's group tables
order through the bar's Sort control by Name or the open tab's columns ([Analytics](console.md#analytics)). The
Analytics view's model grouping lists a request refused before it resolved a model under "no model"
([Analytics](console.md#analytics)). The Burn section closes on Member budgets, its reconciliation rows listed on the
Meter view ([Analytics](console.md#analytics), [Meter](console.md#meter)). The Cache warming tab shows what the probes
saved and the net saving after their cost, as tiles, a chart and a Saved column per pool
([Analytics](console.md#analytics)). The Client and User agent filters list the range's values and take a typed pattern,
each value a chip ([Analytics](console.md#analytics)). The Cost by chart's note names the unpriced groups it draws as
gaps ([Analytics](console.md#analytics)). The Cost split's subscription ring and the Plan per hour legend read their
figures in the chosen week ([Analytics](console.md#analytics)). The ledger records a null diagnostics member on a
request that named a message as `none` and a comparison the upstream was still running as `pending`, which Misses by
reason and its per-group table read as No divergence and Pending ([Analytics](console.md#analytics)). The Refusals by
category card narrows under the Client and User agent filters and its links carry the client pattern
([Analytics](console.md#analytics)). The Refusals by category card splits its bars by model and counts the requests that
presented a fallback token ([Analytics](console.md#analytics)). The Traffic section's Refusals by category card counts
the answers the upstream refused by category, each bar opening the Requests view on them
([Analytics](console.md#analytics)). The Traffic table's Error rate reads a dash for a group with no requests
([Analytics](console.md#analytics)). The unused allowance card and the Accounts detail's allowance block read their
figures in the chosen week under every position and wait on the cost model, so a failed settings read draws their error
([Analytics](console.md#analytics), [Accounts](console.md#accounts)). Under the Analytics classifier grouping the
requests with no classifier call are labeled `none`, as the Requests facet names them
([Analytics](console.md#analytics)). The Burn tab's three plan cards and Spend per minute sit two to a row where the
grid fits two columns or more ([Analytics](console.md#analytics)). A session that branched from another conversation at
depth 2 or deeper reads a `fork` badge in its Sessions row, its hover naming the base, the depth and, where the list
holds the base with both pins live, the base's account, and a click on the badge scrolls to the base's row where the
list holds it ([Sessions](console.md#sessions)). An open Sessions row ends on its Cache warming details, shut by default
with the recipes' states on their line, which read each agent's TTL, last request, last and next probe, the probes sent
and the requests they kept warm, and its recipe state, or `warm` or `cold` by the cache TTL on a pin no recipe stands on
([Sessions](console.md#sessions)). An open Sessions row lists the agents the ledger holds requests of that draw no line,
such as a subagent whose pin and recipe are gone, on one earlier agents line under its Agents table with their requests
and their cost, and the line opens to one row per agent ([Sessions](console.md#sessions)). At a phone's width the
Sessions table scrolls sideways inside its card while an open row's detail keeps to the card's visible width at its left
edge, the Session facts above the Routing facts and the Agents table scrolling on its own
([Sessions](console.md#sessions)). On the Sessions view a released subagent's record leaves the engine one cache TTL
after the release, and one that held a slot of the agent cap keeps it until then ([Sessions](console.md#sessions)).
Opening a Sessions row reads the session's facts in a Session grid and a Routing grid, then the Agents table, one line
per agent with its type, model, account, context, burn, requests, cost, status and cache, the agent linking to the
request log and the account to its page, and each line's menu opens the analytics session view, the request log and the
routing log on the session and the agent ([Sessions](console.md#sessions)). The Sessions card's summary line counts the
agents of the rows the filters keep by lifecycle word, with the client requests of the last 24 hours and their spend,
and the bar's count reads `{n} sessions · {k} agents` ([Sessions](console.md#sessions)). The Sessions row's "+N" badges
count every agent pinned on another model in the Model cell and on another account in the Account cell, a pin without a
recipe among them ([Sessions](console.md#sessions)). The Sessions row's Cache reads `warm` or `cold` by the pin's cache
TTL with the recipe state word under it, from main's recipe or, where main holds none, the newest other agent's, as the
row's `keepalive` summary on `GET /admin/sessions` carries it ([Sessions](console.md#sessions)). The Sessions row's Cost
reads the spend of the requests its Requests cell counts, under Native the session's share of the chosen week and under
the other positions its dollars, with a `+$` badge where they billed dollars beside a plan figure and the pin's burn per
hour under it ([Sessions](console.md#sessions)). The Sessions row's Last used reads the newest use across the session's
conversation pins, main's and its agents' ([Sessions](console.md#sessions)). The Sessions row's Requests counts the
session's client requests over its life under its key, the ones the router refused before any account was asked left
out, with its last request's age under it ([Sessions](console.md#sessions)). The Sessions row's Status reads main's
lifecycle word, `busy`, `idle`, `parked`, `registered`, `released` or `unknown`, `finished` on a finished session and
`unreported` where no mod reports it or the engine holds no record of main, with `seen {ago}` or `silent {span}` under
it on a companion session, and its Agents cell counts the agents by their words ([Sessions](console.md#sessions)). The
Sessions view lists the sessions the router lists, a session no companion reports only while the latest of its last use,
its last request and, where its companion ended the session, that end is within the non-companion timeout, one row each,
with Session, Status, Agents, Model, Account, Requests, Cost, Last used and Cache shown by default and Companion,
Context, Created, Effort, Key, Last request, Next probe and Runway through the Columns control, Context and Effort read
from main's conversation pin ([Sessions](console.md#sessions)). The Sessions view orders its rows through the filter
bar's sort control by Created, Last activity, Cost or Requests, newest or largest first by default
([Sessions](console.md#sessions)). The Sessions view's filters, carried in its address, stand in two groups under one
rule: a session is listed when it passes every Session filter (Active within, Client, Companion, Finished sessions,
Forked sessions, Key, Pool and Slowed) and one of its agent lines passes every Agents filter (Account, Agent type,
Cache, Effort, Model, Released agents and Status), a finished session staying hidden unless Finished sessions stands
([Sessions](console.md#sessions)). While an Agents filter stands on the Sessions view, its chip ends in `(any agent)`,
the count reads `{k} matching agents`, and an open row lists its matching agents with the others under
`{k} agents hidden by the filters`, which opens them ([Sessions](console.md#sessions)). The cost reference's Windows by
tier draws the Interactive regime's rows first under a headline naming its clients and the base rows under Headless, its
Peak band card says which traffic the band applies to, and Plan rates' Regime control reads Interactive and Headless
([The cost reference](console.md#the-cost-reference)). The Settings page's About card lists the session expiry and the
non-companion timeout, the two keys of the `sessions` block, each as a duration with its seconds beside it and its hover
naming the key ([Settings](console.md#settings)). The Settings page's About card names the bootstrap's body capture
limit beside its two retention lines ([Settings](console.md#settings)). The Settings page's Appearance card offers a
Palette control, Default, the console's own palette, or GitHub, stored in the browser ([Settings](console.md#settings)).
A decision card's cache-map row folds a record of four entries or more to one chip per tier with the count and the depth
range and the latest three depths, behind a control that lists every entry ([Routing log](console.md#routing-log)). An
event panel's When reads in the viewer's zone, and a shedding round's Rewrite cell names the window as the cost figures
do ([Routing log](console.md#routing-log)). A renewal after a hold carries the wait it followed on its card
([Routing log](console.md#routing-log)). A guardrail's update records each changed field with its value before and
after, as an account's, a key's or a pool's update does ([Routing log](console.md#routing-log)). A request refused
before it resolved a model reads a dash in the Routing log's Model cell, as it does on the Requests view
([Routing log](console.md#routing-log)). The candidate table of a routing decision, "Candidates, by rank", lists the
chosen member first and the ranked members by rank ([Routing log](console.md#routing-log)). The pick line of a decision
names the tied accounts, the runway that narrowed them and whether a draw chose among them
([Routing log](console.md#routing-log)). The registry's change log records an edit of a pool, a key or an account as
each changed field's value before and after, an account edit that replaced the secret says so without its value, and a
registry change's Account cell links the account or the key it edited ([Routing log](console.md#routing-log)). A pool
save that changes nothing writes no change row and no event, so the change log shows real changes alone
([Routing log](console.md#routing-log)). The Routing log draws a guardrail block's refusal record with the guardrail's
name as a second chip ([Routing log](console.md#routing-log)). The Routing log draws a pool save's keepalive policy as
dotted fields ([Routing log](console.md#routing-log)). The routing log lists a failed usage poll as `poll failed` and a
stream cut after its head as `stream interrupted`, and a shedding round that walks no pin and leaves one out off the
window that fired reads `off window` ([Routing log](console.md#routing-log)). The routing log lists an account whose
plan ended as `plan ended` and the plan's return as `plan restored`, each with the read's source and the profile's
organization type, rate-limit tier and subscription status ([Routing log](console.md#routing-log)). The Routing log
orders by Time in either direction ([Routing log](console.md#routing-log)). The Routing log reads a backoff's close by
its cause, Resumed under a clean poll and Ended by the plan's end under the plan's end
([Routing log](console.md#routing-log)). The Routing log's Pace cell and hover read a lapsed usage-based seat's
remaining share as a percent ([Routing log](console.md#routing-log)). The Routing log's window reset panel reads Read
from, the usage sample or the clock walk ([Routing log](console.md#routing-log)). The Meter view badges a ratio only
where the priced share reaches the rise's floor ([Meter](console.md#meter)). The Meter view in the Diagnostics group
lists every subscription account's completed windows of the last 30 days grouped by account and window kind, the weekly
windows apart, each group collapsed to its latest ratio, its off count and a 30-day ratio spark, and the Meter drift
condition opens it on the account ([Dashboard](console.md#dashboard), [Meter](console.md#meter)). A confirmation toast
shows ahead of the failures queued behind the one on screen. A whole-day or whole-hour duration drops its zero part, so
three whole days read `3d` and an hour `1h`. Every data table alternates its row tone, with a hovered or selected row
reading over it ([The console](console.md)). The page behind an open dialog holds still and keeps its scroll position.
The embedded console builds on Node 26 or later, which the build script checks before npm runs
([Installation](installation.md#build)). A pool's change rows name each keepalive policy member they moved as
`keepalive.<member>`, and a new pool's row lists the policy ([Console](console.md#routing-log)). A Team or Enterprise
seat prices under its tier word where the cost table holds it, and the log says when it does not
([Pairing](pairing.md#account-states)). A plan-ended account answers the plan and the tier its last profile named, its
earlier readings keeping the plan they were priced under ([Pairing](pairing.md#account-states)). `POST /admin/keys` and
the key edit refuse an unknown member with `400` as the pool routes do ([API reference](api-reference.md)). A pool or
key edit, or a guardrail create or edit, that would also take a list past 64 guardrails answers its stale stamp, its
missing id or its refused member before the bound ([Pools](routing.md#pools), [Keys](routing.md#keys)). The Dashboard's
account cards and the Accounts list keep a slot for a window the account's poll does not carry, "none" muted in the
share's place with the window's words on hover, "no 5-hour window" on an approved Ultra account, the Quota cards keep
the window's row with the same words muted, and an account whose poll has read no window reads "Not read yet" on the
Dashboard's cards and the Accounts list ([Dashboard](console.md#dashboard), [Accounts](console.md#accounts),
[Quota](console.md#quota)). The top bar's Week control reads a subscription figure as a share of the Pro, Max or Ultra
week, Ultra week until you pick one, stored in the browser beside the unit position, mirrored on the Settings view's
Unit row and disabled under the dollar positions ([The shell](console.md#the-shell), [Settings](console.md#settings)). A
cost model that sizes no 7-day capacity for the chosen week's plan draws the unpriced dash on every cost figure in that
week, titled "Unpriced: the cost model holds no 7-day capacity for the {plan} week", the tiles' note reading "unpriced"
([The shell](console.md#the-shell)). The Accounts view's lifetime figures and the Keys view's lifetime tiles and card
re-read their rows older than the week once an hour ([Accounts](console.md#accounts), [Keys](console.md#keys)). Auto
reload skips a tick while the view's previous read is still in flight, and a read with no answer after a minute fails
and frees the next tick ([The console](console.md)). The Dashboard's attention line skips a tick or a focus that finds
its previous read still out, so a slow read lands and the next tick reads again ([Dashboard](console.md#dashboard)). On
Requests and the Routing log auto reload also waits while the list for the view's filters is still loading, unless its
last read failed ([The console](console.md)). Auto reload's read at once, when a wait ends or the interval changes a
full interval after the last answer, also waits for a read still in flight and leaves the read to the next tick
([The console](console.md)). An account's Cooling down table draws a model scope's id and a scope the console does not
know in the mono face and Whole account, Fast mode and Fast mode credits in the text face
([Accounts](console.md#accounts)). The Routing log draws an id that links to its account, key, model or pool in the mono
face where the chip asks for it, as it draws a session's or a request's link, so a decision's Request facts read the Key
row's id in the face of the Session row's ([Routing log](console.md#routing-log)). A dialog draws its fields afresh at
each opening, so a picker's search text starts empty ([The shell](console.md#the-shell)). The login screen's session
read fails after a minute with no answer, as a view's read does, so a router that never answers it brings the card that
says the router did not answer ([The shell](console.md#the-shell)). The Accounts view reads the settings once per visit,
whichever account opens, and the allowance card's Try again reads them again ([Accounts](console.md#accounts)). The
Analytics view reads the account list with the window: when a new bucket moves a preset's end, on Refresh and Try again,
and at each interval under a custom range whose end has not passed ([Analytics](console.md#analytics)). The Pools view
reads the sessions and the keepalive overview its pool dialog draws when the dialog opens on a saved pool and again
after a key write from it, the overview also when the return rate's range is picked, and the Guardrails view reads the
pools and keys its dialog lists once per visit and again after a write, a dialog close and Refresh, neither at the
interval ([Pools](console.md#pools), [Guardrails](console.md#guardrails)). An account's catalog dialog draws a failed
Refresh's failure over the list it kept, until a read answers ([Accounts](console.md#accounts)).

Every finished request lands in the ledger as one row for the client request, one per attempt and one per routing
decision, folded into the hourly rollup the analytics read ([Statistics](statistics.md#the-ledger)). Each attempt
carries its token counts and the tier the router prices by, which a page or a route prices when it reads them, so a free
organization's and a usage-based seat's requests read unpriced in every figure ([Pricing](statistics.md#pricing), [Plan
meter](statistics.md#plan-meter)). A subscription account's plan-side figures read as one Pro-week figure, the share of
a Pro 7-day window the same tokens would spend, with the request's shares of its own account's windows as the hint where
one account is named, bounded by the windows the account's poll carries ([Plan meter](statistics.md#plan-meter)). The
metrics listener serves `/metrics` in Prometheus text format ([Metrics](statistics.md#metrics)), and the
`phoebis_attention` gauge carries what needs an operator ([Conditions and gauges](statistics.md#conditions-and-gauges)).
A failed credential refresh raises `refresh_failed` in the serious tone. A plan-side figure prices under the regime of
the client's entrypoint, the interactive regime (the desktop app, the IDE extension, the terminal and the third-party
desktop) at twice the 5-hour and 1.25 times the weekly capacity of the headless client and without the band, the relay
reads the entrypoint in its position in the user agent, and the ledger reconciles each account's completed windows
against the meter every hour. Both retention keys default to 365 days, `stats.retention_days` for the requests, the
samples and the routing log and `stats.rollup_retention_days` for the rollups, and the Settings view's About card shows
the two as the file sets them ([Retention](statistics.md#retention), [Settings](console.md#settings)). Each request row
carries the instant the ledger wrote it, `GET /admin/requests` reads the rows written after `landed_after` and answers
the newest stamp among the rows its filters admit as `landed_through`, at most a time-bounded read's end unless the read
passes `fixed_end`. The Requests view's auto reload polls by that stamp, and its live poll of a preset range reads to
the router's clock, so a long request that finishes below rows already shown lands between them in the same tint on the
next read ([Statistics](statistics.md#the-ledger), [Requests](console.md#requests)). A request log read over a fixed
range passes `fixed_end`, so its `landed_through` may pass the range's end and a row that landed after it is read once
([The ledger](statistics.md#the-ledger)). A reconciled window splits at a fall and sums its parts, a slid window writes
no row, the drift holds only where the rise and the priced share both reach 5%, and a window's two ends are its first
and last readings whatever their source ([Statistics](statistics.md#conditions-and-gauges)). The drift condition reads
the windows that ended inside the last eight days, so an account that serves nothing leaves it within eight days of its
newest window's end ([Statistics](statistics.md#conditions-and-gauges)). The hourly purge frees captured bodies in
slices of about 32 MiB, a larger body going alone in a slice of its own, so the ledger's writer waits behind one slice
at most, well inside its timeout ([Statistics](statistics.md#retention)). The usage poll reads a usage-based Enterprise
seat's answer: extra-usage amounts with a fraction part round to the minor unit, the body's `spend` object is read
beside them, and a failed parse carries the parser's reason ([Quota samples](statistics.md#quota-samples)). A
usage-based seat's requests file under the funding `usage_based`, priced in dollars at the API rates and kept by the
`billed` filter ([The ledger](statistics.md#the-ledger), [Plan meter](statistics.md#plan-meter)). A usage-based seat's
account view carries its spend window with the poll's amounts, the newest share and a reset estimated at 00:00 UTC on
the first of the next month until a header names one ([Quota samples](statistics.md#quota-samples)). The account view
answers the 7-day Fable under the one id `seven_day_fable` ([Quota samples](statistics.md#quota-samples)). The quota
history of an `oauth` account answers a head's sample and a poll's at one instant where both landed in one bucket
([Quota samples](statistics.md#quota-samples)). A request the router's own shutdown ends past the drain window is booked
`canceled` under the code `router_shutdown`, apart from a caller that left on its own under `client_gone` ([The
ledger](statistics.md#the-ledger)). The request log filters on the serving attempt's stop reason and refusal category,
and the analytics answer the refused requests in range per category ([Refused
requests](statistics.md#refused-requests)). The analytics answer the weekly allowance each subscription account left
unused at its resets in range, from the stored window reset events ([Quota samples](statistics.md#quota-samples)). The
reconciliation fold drops a dip in a window's readings that the next readings reverse past what the attempts predict, so
a transient zero poll counts its window's rise once and holds no `meter_drift`
([Statistics](statistics.md#conditions-and-gauges)). A window's reading carries the plan and the tier the account held
when it was taken, and a window's reset event and the allowance route answer that pair
([Statistics](statistics.md#quota-samples)). A `poll_backoff_ended` event names its cause, a clean poll or the account's
plan-ended pause ([Statistics](statistics.md#conditions-and-gauges)). The profile is read again on every third usage
poll, a quarter hour apart, so a tier or plan change reaches the router within a quarter hour
([Statistics](statistics.md#quota-samples)). A re-fold after a moved prompt-length threshold writes into no hour the
rollup purge dropped ([Statistics](statistics.md#retention)). Every route that prices a subscription row answers one
figure, the Pro-week figure, null where a row it sums priced nothing, and the ledger raises `model_unpriced`, an
attention item at the info level, for a plan-funded model that prices nothing ([Plan meter](statistics.md#plan-meter),
[Conditions and gauges](statistics.md#conditions-and-gauges)). The attention reader, the console's reads, the metrics
scrape and `/v1/usage` run beside each other on four read-only connections, and the ledger's writer reads its own budget
figures on its own connection ([The ledger writer](statistics.md#the-ledger-writer)). The ledger keeps planner
statistics from the first start on and refreshes them after each hourly purge ([The ledger
writer](statistics.md#the-ledger-writer)). The analytics series and burn routes and a pool's traffic route answer `400`
with the ledger's own words for a range that needs more points than a chart carries, a trailing partial bucket counted
as one ([API reference](api-reference.md)). A model filter that names a model's `[1m]` spelling selects that model's
rows on the request log, the analytics reads and the cache placements ([Requests](console.md#requests),
[Analytics](console.md#analytics)). A request row carries the serving attempt's account, speed, outcome and token
counts, written when it lands, and the rows written before the columns existed fill a slice at a time, one at each
start, which logs the rows left, and the rest in the hourly purge while the router runs
([Statistics](statistics.md#the-ledger)). Refusals count by the request's model and the category and the count answers
the fallbacks in range, a request that presented a refusal's fallback credit carries the credit's digest beside the
refused attempt's, and the request log filters on it ([Refused requests](statistics.md#refused-requests)). The
`slowed_long` condition holds on the time the slow-down held the account's requests over the trailing day, against twice
the stretch and at least an hour, and names the held time and the requests held ([Conditions and
gauges](statistics.md#conditions-and-gauges)). The hourly purge's slice of captured bodies ends after the body in
progress once a write is waiting, so a write waits for one body at most ([Retention](statistics.md#retention)). A
response head's `7d_oi` window is filed as the Fable week, so a head and a poll of one bucket write one Fable row and
the gauge holds one Fable series ([Quota samples](statistics.md#quota-samples)). The request log orders by its arrival,
attempts, duration, time to first byte, tokens in or tokens out, either way, and the routing log by its time either way,
each page continuing at its last row ([API reference](api-reference.md)). The request log prices each attempt through
the one price rule the fold uses, so a probe on a model the price list lacks reads no dollars, as the model's other
requests do ([Pricing](statistics.md#pricing)). An account's pause and resume land in the routing log as a registry
change carrying the `enabled` pair, and a registry change names the account or the key it changed, so the routing log
filtered by the account or the key lists it ([Routing log](console.md#routing-log)).

Every price is computed when it is read, from the token counts the ledger stores, at the price revision in force when
the request arrived. The price table and the subscription cost model are revision lists keyed by the date or the UTC
instant each revision takes effect, the cost model's unit is the Pro 7-day window, of which one percent is 900,000
units, so every subscription figure is a Pro-week figure the console reads in the chosen week, the account's tier and
the prompt-length band join the hourly rollups, a model the cost model does not weight prices nothing, its figure null,
and raises an attention item, and the cost model names each model's tier ([statistics](statistics.md#pricing),
[installation](installation.md#stats)). The shipped price table carries Claude Sonnet 4.6 and Claude Sonnet 4.5 and
reads Claude Fable 5's cache read at $1 per million tokens in every revision ([statistics](statistics.md#pricing)). The
subscription cost model weighs Claude Fable 5, Opus 5, Opus 4.8 and Sonnet 4.6 as the plan meter measured them on
2026-10-29, the measurement confirming Sonnet 5's weight ([statistics](statistics.md#plan-meter)).

The console prices every figure in the unit the shell's Unit switch names, one of four positions, Native, All in
dollars, Billed dollars and Dollar equivalent, Native by default (a subscription request as a share of the chosen week,
a billed request in dollars), with the Week control beside it, Pro week, Max week or Ultra week, Ultra week by default,
and a legend ([The shell](console.md#the-shell)). The cost reference names the plans Pro, Max and Ultra with Pro first
in its tier chooser, shows one row per model tier with the older models folded, dashes with "not weighed" for a model
without a weight, and a revision select ([analytics](console.md#analytics), [cost
reference](console.md#the-cost-reference)). The analytics, dashboard, keys and pools routes answer a null figure where a
subscription request priced nothing, its model without a weight in the cost model, and count the probes and kept-warm
credits a plan funded, so the Dashboard, Keys and Pools tiles read a dash with the hover "No subscription request
priced: its model has no weight in the cost model" and the Analytics Cost by chart notes the groups it draws unpriced in
range ([dashboard](console.md#dashboard), [analytics](console.md#analytics)). The Requests and Dashboard Cost cells
follow the unit legend per position: a subscription request reads its dollar equivalent under All in dollars and Dollar
equivalent and a dash under Billed dollars, and a billed request its dollars under All in dollars and Billed dollars and
a dash under Dollar equivalent ([dashboard](console.md#dashboard), [requests](console.md#requests)).

The console carries the pool dialog's load shedding switch, which turns the round off while the spent move keeps its own
toggle, its Slow-down group with the switch, the stretch threshold, the hold and a preview of the slowed members, its
warnings on settings that work against each other, the speed chip of a slowed account on the Quota, Accounts, Pools and
Sessions views, the time the slow-down held requests under the Analytics moves chart, and four conditions on the
attention line that read the routing log of the last day ([Pools](console.md#pools), [Quota](console.md#quota),
[Analytics](console.md#analytics), [Conditions and gauges](statistics.md#conditions-and-gauges)). The hint of the pool
dialog's Burn average field names the Custom preset in Load shedding as what unlocks the field
([Pools](console.md#pools)). A held request whose waits span two accounts names each wait with its own account in the
Requests hover, `waited for the slow-down 42 s on studio, for a slot 3 s on primary` ([Requests](console.md#requests)).

`phoebis recover-key` writes the file of a key it generated once the operator confirms, prepares the registry's schema
as a step of its own whose failure prints `the registry's schema could not be prepared for the recovery, so nothing was
discarded: {error}`, and a run that exits 1 or 2 removes the key file it wrote, naming it in one line, while every run
keeps the registry's lock file ([The storage key](operations.md#the-storage-key)). Every admin answer carries the commit
the binary was built from as `x-phoebis-build`, the system and about answers carry it as `build`, and the status screen
prints it under the version ([Is it up?](operations.md#is-it-up)). A `PHOEBIS_BUILD_COMMIT` set by hand is trimmed and
lowercased before the `x-phoebis-build` header, the `build` members and the bundle's commit read it
([Is it up?](operations.md#is-it-up)). The unit creates its state directory 0700, `phoebis health` runs without a
resolved admin secret, the release publishes `SHA256SUMS` alone, and `extra_ca_pem` trusts every anchor of a bundle
([Installation](installation.md#install-as-a-service)). The router's log lines print their instants in UTC with the zone
named, while the status screen keeps the host's zone ([Is it up?](operations.md#is-it-up)).
`recover-key` on a registry that has no canary but opens under the key seals the canary and exits 0 with a line naming
the rows it opened, discarding nothing ([The storage key](operations.md#the-storage-key)).
`phoebis check-config` refuses a registry or a ledger a newer build wrote with the start's own sentence and its restore
advice, under `reading the registry at {path}` or `reading the ledger at {path}`
([Upgrades and renewals](operations.md#upgrades)). `phoebis init` refuses anything at the data directory's path that is
not a directory, a dangling link among them, `proxy.upstream_gzip` refuses a bad word as a YAML error naming the key,
`phoebis tls reload` names the admin listener in its refusal, `phoebis backup` names the directory it could not create
or sync, and `phoebis status` probes the proxy beside its admin reads ([Commands](installation.md#commands),
[proxy](installation.md#proxy), [Is it up?](operations.md#is-it-up)). A registry from before the key canary proves the
storage key against its keepalive recipe bodies too, so a registry whose readable rows are all bodies opens under the
key that sealed them and refuses any other ([The storage key](operations.md#the-storage-key)).

Guardrails are named objects attached to pools and keys, each an ordered list of conditions over a request's headers,
derived facts, body members and messages, which in observe mode record the requests they match and in enforce mode
refuse them with a 403 before any account is selected ([guardrails](guardrails.md)). The build ships three presets for
the client's forks: the subagent status summary, the prompt suggestion and the away recap. A dry run evaluates a
definition against the captured bodies of a pool or a key. The console gains a [Guardrails](console.md#guardrails) view
with its dialog, the [Requests](console.md#requests) view a Guardrail filter and a Guardrails drawer section, and the
[Analytics](console.md#analytics) view a Guardrails card with the observed and blocked counts and the blocked bytes per
guardrail.
A guardrail block answers 403 `permission_error` with `x-should-retry: false`, so the client sends a blocked request
once per tick and heals nothing ([What a blocked client receives](guardrails.md#what-a-blocked-client-receives)).
The ledger flags every refusal the router issues before it asks an account, for the client's or a policy's cause, and
the hourly figures count such a request in a refusals map by code and nowhere else
([Refused requests](statistics.md#refused-requests)).
The refused requests show in a Refused tile on the Dashboard, a Refused column and a "Refusals by code" card on the
Analytics view and a `phoebis_refusals_total` counter.
The first start after the upgrade moves the stored refusals out of the counts into the map.
An attachment written from either dialog stamps every row whose list it changes, so a pool, key or guardrail save
opened before it is refused as stale.
The Attach box of the pool and key dialogs stays open while the operator picks.
The refusal count narrows by the client and user agent patterns the analytics read and holds their group patterns to the
same bound, and the request log and the routing log filter on an agent id
([Statistics](statistics.md#refused-requests)).
A guardrail block writes a `refused` decision row with the guardrail's name beside its request row, so the routing log
shows the block ([Guardrails](guardrails.md#observe-then-enforce)).
A guardrail write names a list of the wrong length first, then an unknown subject, operator or selector word, then the
first other fault by its index ([The conditions](guardrails.md#the-conditions)).

The router serves Claude Code alone: a request on the two Messages routes from any other client is refused `403`
`client_not_allowed` before its body is read, under every key ([Clients](routing.md#clients)). The contract gained the
version item, which refuses a `claude-cli/` version spelled any other way than the client's as `client_contract`, and
holds its version and session items on a token count and a keepalive probe ([The required
set](client.md#the-required-set)). A request is identified by its session header alone, and the session views key every
row by the client's own session id ([How a request is identified](client.md#how-a-request-is-identified)).

Each key carries a companion permission set, `companion_permissions`, two words in this version: `allowance`, whether
the key's companion may read the pool's allowance, and `keepalive`, whether the router warms the key's sessions' caches
from the companion's reports. A new key starts with both, the key dialog edits them as the Allowance tool and Cache
warming toggles, and the Keys table notes each word a key lacks ([The key's
permissions](companion-mod.md#the-keys-permissions), [Keys](routing.md#keys)). The set replaces the key's
`keepalive_opt_out` flag: the first start after the upgrade gives each existing key `allowance`, and `keepalive` to each
key whose flag was clear, so a key that had opted out holds `allowance` alone. The proxy listener answers `GET
/v0/companion/allowance` under the router key with the key's pool as the router reads it: each active account under a
pseudonym that is stable on one installation and reverses to nothing, its windows with their burn, projection and
runway, its spend against its budgets, and the asking session's pins, and the read writes no ledger row ([The companion
routes](relay.md#the-companion-routes)). The account's details and the admin API's account resource carry the pseudonym
as the Companion id ([Accounts](console.md#accounts)). The companion mod, a Claude Code plugin, replaces the keepalive
mod of the pre-release builds: it carries the lifecycle reports over unchanged on the keepalive protocol, adds four
settings, checks the router at session start and names a missing setting, a refused key or a silent router in the
transcript, and gives the model the `allowance` tool, which answers the route's body from a cache of ten seconds ([The
companion mod](companion-mod.md)).
