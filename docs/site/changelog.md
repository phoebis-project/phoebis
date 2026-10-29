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
([Client drift](relay.md#client-drift)). A session that switches model between its turns names no previous message in
its cache diagnostics on its first request to another account, like any move
([The body edits](relay.md#the-body-edits)).
A body carrying a fallback credit token from a refusal is placed on the account that issued it, the relay keeping the
issuer under the token's digest in memory for an hour ([Admission](relay.md#admission)).
The router requires the session header, the class header, the attribution line, `metadata.user_id` and the agent and
compaction headers of a Claude Code request and refuses the rest `403` `client_contract`
([The required set](client.md#the-required-set)), restores `cc_prompt_id` from the client's prompt-id header and
`cc_prev_req` by class, blanks the account uuid on an API-key leg, and prices the client's auto-mode classifier calls
that carry the classifier marker at no share of a plan window
([Auto mode behind the router](relay.md#auto-mode-behind-the-router)). Every caller-credential refusal
(`auth_ambiguous`, `auth_malformed`, `auth_missing` and `auth_unknown`, the registry's key refusal among them) carries
`x-should-retry: false` on every route that answers it, so the client does not resend it
([Refusal codes](client.md#refusal-codes)). The router is measured against Claude Code 2.1.310 on Linux x64, the build
the profile and the fixtures come from ([The pinned client build](development.md#the-pinned-client-build)).

A key routes into one pool ([Pools, keys and routing](routing.md)), and every request it presents is placed on one
account by the session's pin, the pool's strategy and each account's quota. A live pin moves only by one of five rules,
spent, overage, return to plan, drain and shed, every move passing a runway gate on its destination (the pin's cold
rewrite and the burn of its sessions carried to the window's horizon under the pool's ceiling) and landing where the
pool's strategy ranks the accounts the session fits, the longest runway breaking the strategy's tie, `return_to_plan`
choosing when a session on an API-key account returns to a subscription account (`always`, `when_cold` or `never`, with
`return_to_plan_dwell_seconds` under `always`), the shed rule reading every plan window's projection over the ceiling at
its horizon after a wait, a spent window about to reset waited out in place, and the routing log recording a shedding
mark that clears as `mark cleared`, a reservation that leaves without its usage as `reservation expired` and a spent
window that drops back under its reset as `window cleared` ([Moves](routing.md#moves),
[The windows](routing.md#the-windows-the-router-works-from), [Routing log](console.md#routing-log)). With a pool's
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
5-hour round fires in the window's first two burn averages, the cheapest pick prices a rewrite in the Max 20x equivalent
of the pin's weekly window, and the projection, the runway and the stretch weigh the band's hours inside the horizon. A
moved pin's stamp names how it moved, a shedding round or the failover that landed it, which the status page's pins
block and the routing log's record card read as `shed from` or `failed over from` the account it left
([Is it up?](operations.md#is-it-up), [Routing log](console.md#routing-log)). After a slow-down round a `wall_reached`
condition gives how long before the window's reset the wall arrived in place of the projection error, and the ledger
keeps each account's longest of the hour ([Conditions and gauges](statistics.md#conditions-and-gauges)). The shed rule's
wait belongs to the window whose reset its first reading over was read under, so a reading whose reset moved an hour or
more opens a new window whose wait starts again ([Moves](routing.md#moves)). A pin only auxiliary requests use stays
warm while its last use is younger than its cache TTL ([Moves](routing.md#moves)). A shedding round that walks no pin
and leaves one out off the window that fired reads `off_window`, and while a subscription account runs on its plan the
round orders its pins by the span's average it prices them by ([Moves](routing.md#moves)). A placement's own fit and
Pace score leave the session's own standing row on the account it left out of the burn
([Pools, keys and routing](routing.md#the-windows-the-router-works-from)). A usage-based Enterprise seat, classed from
its stored profile, is placed as a paid source beside the API-key accounts and never reads as in overage
([Two kinds of account](pairing.md#two-kinds-of-account), [Selection](routing.md#selection)). A usage-based seat whose
spend share reads one or more is held out for every model until the spend window's reset, the share expires at the
reset it was read under, and the fast lane's credits read follows the same expiry ([Overage](routing.md#overage)). A
usage-based seat's poll share and a share-only head's share lapse at the head reset the router holds from a head
observed at or before the reading while that reset is still ahead of it, and the account view's `spend` reads the
share's lapse from the router wherever the router holds the reading ([Overage](routing.md#overage)). A session's runway
that reaches past its binding window's next reset reads unbounded on `GET /admin/sessions` and in the decision record's
fit table, while the fit's verdict reads the computed figure against the horizon ([The windows the router works
from](routing.md#the-windows-the-router-works-from)). A request whose connection could not be opened is retried once
after about a second, a reused connection that died is retried at once, and a request unsent twice answers 502
`upstream_unreachable` ([Cooldowns and failover](routing.md#cooldowns-and-failover)). A window read under its limit
after a predicted spent edge emits `window_cleared` as a stored reading's fall does, and the clear ends the attention
condition the spent event built ([The windows the router works from](routing.md#the-windows-the-router-works-from)). A
restored wall keeps its anchor across a restart, so within-margin readings after the start do not hold the account spent
past the window's length ([The windows the router works from](routing.md#the-windows-the-router-works-from)).
A session's creation time is its main agent's first placement, kept across a failover, a move and a second pin, so the
Sessions view's order by creation is stable ([Routing](routing.md#selection)).
A usage-based seat's old plan readings start no shedding round ([Routing](routing.md#moves)).

With `keepalive.enabled` on a pool the router keeps a replayable copy of every eligible request a session sends and,
shortly before the cache entry would expire, sends a probe that reads it ([Keepalive](keepalive.md)). The policy's
ceilings, `max_idle_seconds`, `max_parked_seconds` and `scheduled_horizon_seconds`, bound how long a recipe is probed,
the parked and horizon ceilings capped at their break-even and the idle ceiling taken as set
([The four fields](keepalive.md#the-four-fields)). The router reads session liveness and agent state from the keepalive
mod, a Claude Code plugin inside the client's process that holds no policy ([The keepalive mod](keepalive-mod.md)), over
[The keepalive protocol](relay.md#the-keepalive-protocol). A probe kept past its recipe's end follows a model's
resolution to its dated id, the kept-warm credit goes to the newest probe that covered the entry over a later one that
did not cover it, and a failed turn decides no probe, so the retry after it earns the credit
([Which requests re-arm a recipe](keepalive.md#which-requests-re-arm-a-recipe)). A chain of probes on one model, speed
and account, where each later probe read the entry back, settles against its last probe's entry, so a request on that
lineage that reads the cache inside that entry answers every probe of the chain, and a completed compaction stops the
recipe until the next main turn ([Which requests re-arm a recipe](keepalive.md#which-requests-re-arm-a-recipe)). A
pool's idle ceiling above the break-even is taken as set, and the resolved share can read above one for it
([The ceilings](keepalive.md#the-ceilings)). A kept-warm credit's saving prices under the regime of its request's client
([What the ledger records](keepalive.md#what-the-ledger-records)). The keepalive mod refuses a key file whose content is
blank as it refuses one it cannot read ([Configure](keepalive-mod.md#configure)). An arrival after a completion the
keepalive missed measures the kept-warm gap from the turn that answered, and a probe under a twin spelling of a dated
model id is dated ([Which requests re-arm a recipe](keepalive.md#which-requests-re-arm-a-recipe)). The keepalive mod
keeps its owner and instance across a `/clear` and retries a start the router refused for a live holder once that
holder's lease lapses, and the router warns once per session at the first request that names another owner while the
holder's lease is live and lets such a request take the session over once the lease has lapsed
([The owner header](keepalive-mod.md#the-owner-header)). A released agent's record leaves the engine one pool cache TTL
after its release, freeing its slot of the agent cap ([The five states](keepalive.md#the-five-states)). A failed or
canceled request leaves the last answered recipe armed, and a probe's transient failure retries on a ladder bounded by
the entry's expiry ([The five states](keepalive.md#the-five-states)).
A message's text-only content array hashes as its blocks up to the last one carrying the cache marker, a single block as
its text, an idle window counts a return on either of the last two message boundaries of the request before it, and a
refreshing request whose chain holds neither boundary while its answer read the cache is logged and counted per client
version, every chain hash of such a body moving once at the deploy, so the cache map and the anchors of running sessions
start cold ([Keepalive](keepalive.md#the-return-rate-by-idle-age), [Relay](relay.md#the-boundary-chains)).
The idle-return counters are kept by pool and UTC day, so the keepalive overview answers the return rate over the last 7
or 30 days, the counters starting afresh at the deploy ([Keepalive](keepalive.md#the-return-rate-by-idle-age)).
The keepalive mod keeps a subagent the router named released unannounced when a later answer omits it, until its next
report, so a released record's departure frees its slot ([The keepalive protocol](relay.md#the-keepalive-protocol)).

The admin listener serves the web console behind a login with the admin secret, with the views in the sidebar's order,
Dashboard, Quota, Accounts, Pools, Keys, Guardrails, Requests, Analytics, Sessions and Settings, and under them the
Diagnostics group holding Routing log and Meter ([The console](console.md)). Every list view opens on one filter bar
holding the slots it has in one order: the range pills, the search, Add filter with its filters as chips under the bar,
the view's own controls, and at the right edge a split Refresh button, whose dropdown sets auto reload, and the count of
what the view lists. `/models` opens the Keys view, where a saved key's dialog shows its Effective list, the accounts
cooling for a model show on the Accounts view and the model rejected and model permitted rows in the Routing log, and a
model a decision card, an event's detail or a log row names opens the Routing log on its rows over the last day ([The
shell](console.md#the-shell), [Keys](console.md#keys), [Routing log](console.md#routing-log)). The Accounts, Analytics
and Pools views tell a bundle that changed under the tab from an error while drawing, each with its own words and a
Reload button ([The shell](console.md#the-shell)). The console shows a reload banner when the router answers from a
newer build than the open tab's bundle, and an attention condition the console does not know reads as Unknown condition
with its kind on hover ([The shell](console.md#the-shell), [Dashboard](console.md#dashboard)). The expert view, a switch
in the sidebar's foot, shows every field of the pool and key dialogs, the advanced ones included, and the sidebar's
Diagnostics group ([The shell](console.md#the-shell)). The login screen offers "Stay logged in", which keeps the browser
logged in for 30 days from the last visit, across a router restart, until a secret rotation or a logout of a kept login,
which ends every kept login ([The shell](console.md#the-shell)). The top bar wraps a view title it cannot hold beside
its controls onto a second line at a phone's width, so the Routing log page fits the window's width ([The
shell](console.md#the-shell)). The unit legend takes the focus when it opens and hands it back to its button when it
closes on Escape, and the Settings view's Appearance rows wrap their controls under their labels at a phone's width
([The shell](console.md#the-shell), [Settings](console.md#settings)). A key's processed rules are read on `GET
/admin/keys/{id}/models` and in the key dialog, and the model weight estimated attention item opens no page ([API
reference](api-reference.md)). Each `GET /admin/sessions` row reads from the main agent's conversation pin, or the
newest conversation pin of another agent where main holds none, and lists the row's conversation pins as agent lines
with their effort and their warmth, a session no companion reports leaving the list after the non-companion timeout of
the sessions block ([API reference](api-reference.md), [Installation](installation.md#sessions),
[Keepalive](keepalive.md#in-the-console)).
The admin API is documented by an OpenAPI document the router serves, with an interactive
reference page on the admin listener ([API reference](api-reference.md)). The account's `has_extra_usage_enabled` reads
the stored profile's `organization` member, as its rate-limit tier does ([API reference](api-reference.md)). The account
create and edit refuse a body that is not a JSON object with `body must be a JSON object`, and the pool members route
answers `body must carry a members list` to an object without one ([API reference](api-reference.md)). The Dashboard,
Guardrails, Pools and Settings views fit a 360 px window with no horizontal scroll ([Dashboard](console.md#dashboard),
[Guardrails](console.md#guardrails), [Pools](console.md#pools), [Settings](console.md#settings)). The Dashboard's
account cards and the Accounts list read `No window since the plan ended {ago}` on an account whose plan ended
([Dashboard](console.md#dashboard), [Accounts](console.md#accounts)). The Dashboard's
attention line keeps one row and folds the items past it behind a "+N more" badge, and a pool card's attention rows end
with their age ([Dashboard](console.md#dashboard), [Pools](console.md#pools)). The Dashboard's Estimated cost tile reads
the day's subscription traffic against the 7-day window under Native ([Dashboard](console.md#dashboard)). The
Dashboard's remaining cards and the Burn cards price a Team or Enterprise seat at its tier where the cost model holds
the row, and the remaining cards name a free plan, a usage-based seat or an unheld tier under the ring with the reason,
which the Burn cards read as tier unknown under Window shares ([Dashboard](console.md#dashboard),
[Analytics](console.md#analytics)). The
Dashboard's remaining cards list each account with the most of its own window left first
([Dashboard](console.md#dashboard)). A Quota card's Refresh draws its reading the moment it answers, at the reading's
instant ([Quota](console.md#quota)). A usage-based seat draws one spend bar wherever the window bars are drawn, with its
amounts, its reset marked estimated until a header names it, and a lane on the Quota windows timeline
([Quota](console.md#quota)). An account in the `disconnected` or `reauth_required` state reports no quota window, so
neither a console view nor the status screen draws its last reading ([Quota](console.md#quota)). Each window the
accounts route serves carries its lead over pace, which the console's pace words, pace mark and pace orders read
([Quota](console.md#quota)). On a screen under 900 px wide the Quota cards' reset label takes a line of its own under
the meter ([Quota](console.md#quota)). The Quota cards sort by 5-hour, 7-day or 7-day Fable pace, the account furthest
ahead first, and the Quota windows timeline draws each bar in its account's color, each lane stacking its account and
its chips above its bars on a narrow screen ([Quota](console.md#quota)). The Quota cards' reset line and the window bars
of the Dashboard and the Accounts list read the share the projection leaves, such as `19% left at the reset`
([Quota](console.md#quota)). The Quota windows timeline draws its now marker and its day grid over the bars, so the
marker stands at the instant's place on them and no grid line crosses a lane's stacked account and chips on a narrow
screen ([Quota](console.md#quota)). The Quota view leaves out an account that is disconnected or needs
re-authentication, which keeps its row and its state on the Accounts view ([Quota](console.md#quota)). The Quota view
leaves out an account whose plan ended while its flag stands, as it leaves out a disconnected one, and the Accounts
detail's Usage card names the ended plan ([Quota](console.md#quota), [Accounts](console.md#accounts)). Under the 12-hour
time setting the Quota timeline's short cells read "6 AM" in a cell wide enough for it, with fewer cells at a given
width ([Quota](console.md#quota)). A pairing confirm on a slot sealed under another storage key answers 409 and names
the account, so the dialog can offer Disconnect ([Pairing](pairing.md#with-curl)). A pairing whose confirm meets a
credential sealed under another storage key reads the router's sentence and offers Disconnect, which clears the
credential and starts the pairing again from its first step ([Accounts](console.md#accounts)). A usage poll's 401 moves
the account to `reauth_required`, and any other failed poll, except a 429 or the usage 403 an ended plan explains, logs
one warning and writes a `poll_failed` event ([Pairing](pairing.md#account-states)). A usage-based seat, the rate-limit
tier `default_claude_zero` or the seat tier `enterprise_usage_based`, reads no overage word on the Accounts view, and
its Identity list reads "usage-based" beside its rate-limit tier ([Accounts](console.md#accounts)). An account whose
plan ended reads Free beside its Plan ended row, the plan the router resolved after the end
([Accounts](console.md#accounts)). An account whose plan ended reads the instant in its Identity list until a profile
read or a pairing finds a plan again ([Accounts](console.md#accounts)). An account whose profile reports no plan is
flagged plan ended, which pauses its usage poll and places nothing on it until a profile read or a re-pair finds a plan
again ([Pairing](pairing.md#account-states)). While an account's plan-ended flag stands, `GET /admin/accounts` and
`/{id}` answer its `windows` empty, as they do for a disconnected account ([Pairing](pairing.md#account-states)).
Flagging the plan ended closes an open usage-poll backoff run, so `poll_backoff` clears with the pause
([Statistics](statistics.md#conditions-and-gauges)). An account's plan reads as one word resolved from its profile's
organization type first and its rate-limit tier second (`free`, `pro`, `max_5x`, `max_20x`, `max`, `team`, `enterprise`
or `usage_based`), and a free organization or a usage-based seat prices no capacity, so the router reads it as an
untiered account ([Pairing](pairing.md#two-kinds-of-account), [API reference](api-reference.md)). An account's badge,
its Plan row and the Accounts search read the plan the router resolved from the organization type and the tier, "Free"
for a free organization and "Usage-based" for a seat, as do the Quota card's chip, the capacity rings and the routing
log's candidates ([Accounts](console.md#accounts), [Dashboard](console.md#dashboard), [Quota](console.md#quota),
[Routing log](console.md#routing-log)). An account's detail draws "Allowance left at reset", each weekly window's share
left unused at its last four resets in the top bar's unit, a term's end counting as a reset marked "term ended"
([Accounts](console.md#accounts)). An account's detail draws "Meter drift", the newest completed window of each kind
against the table with the windows off the band in 30 days, opening the Meter view on the account
([Accounts](console.md#accounts)). An account's end editor names the browser's zone beside its field
([Accounts](console.md#accounts)). An account's end editor opens empty for a running plan, and its hint names the date
the plan's billing settings show as the end ([Accounts](console.md#accounts)). An extra usage reading with no amounts
draws its state alone, with no amounts and no meter, on the Accounts view's Usage card and the Quota cards
([Accounts](console.md#accounts), [Quota](console.md#quota)). The account's cancellation flag is spelled `canceled` on
the account view, the account edit, the routing log's term facts and the `wind_down_started` event, with the error code
`account_canceled`, the probe outcome `canceled` and the pairing session's status `canceled`
([Accounts](console.md#accounts), [API reference](api-reference.md)). The Accounts view orders its list through the
filter bar's sort control, a key and a direction carried in the address, Name ascending by default
([Accounts](console.md#accounts)). The Accounts view shows a paired account's first reading once the confirm's poll
lands, a few seconds after the confirm, with "The first quota reading is in flight." in its Usage card until then and a
read of the list every 2 s for up to 30 s while auto reload is off ([Accounts](console.md#accounts)). The allowance
block and the Analytics card price each weekly reset at the tier the account ran under at that reset
([Accounts](console.md#accounts), [Analytics](console.md#analytics)). A new pool's dialog starts with cache warming off
([Pools](console.md#pools)). A pool card's Active pins fact counts every pin under the pool's keys, as the Dashboard's
Sessions card does, and reads how many sit on a former member under the figure ([Dashboard](console.md#dashboard),
[Pools](console.md#pools)). The pool dialog's idle window set above the break-even reads a note that the ceiling lies
above it and prices its cost from the set value, while the parked and horizon ceilings stay capped at the break-even
([Pools](console.md#pools)). The Pools dialog's return rate reads the last 7 or 30 days ([Pools](console.md#pools)). The
Pools view names the third selection tier `metered` in its serving table and its Selection hint, the tier holding the
API-key accounts and the usage-based seats ([Pools](console.md#pools)). The Keys view's Pool filter draws one chip per
pool, and a pool the address names that the router does not list shows its chip until the pools load
([Keys](console.md#keys)). A request whose answer the upstream refused draws the refusal's category as a warning badge
beside its status, and the Outcome group of Add filter offers Stop reason and Refusal category
([Requests](console.md#requests)). A request whose stream was cut after its head reads what cut it in its drawer's Error
row ([Requests](console.md#requests)). An auto reload on a custom range reads once a request received inside the range
and written after an earlier read, as a preset range does ([Requests](console.md#requests)). Each captured body block in
the request drawer has a find field that counts the lines holding a term, opens the wide window on a hit, marks every
occurrence on the drawn rows and walks the hits with Enter and Shift+Enter ([Requests](console.md#requests)). On the
Requests view a custom range's late row that lands below the rows shown draws on the next auto reload read
([Requests](console.md#requests)). On the Requests view and the Routing log the list's late answer replaces the page an
auto reload poll stood in for it, and Next past an overrun under a preset reads to the router's clock
([Requests](console.md#requests), [Routing log](console.md#routing-log)). The pressed range pill names its range and its
end on hover, and when the tab comes back to a read at once the rows it brought keep their tint through the read after
it ([Requests](console.md#requests), [Routing log](console.md#routing-log)). The Requests and Routing log Time cells
print the date before the time on a row older than a day, and the Routing log's columns fit their content
([Requests](console.md#requests), [Routing log](console.md#routing-log)). The Requests and Routing log views filter on
an agent id through the Agent filter of the Subject group, carried in the address as `agent_id`
([Requests](console.md#requests), [Routing log](console.md#routing-log)). The Requests, Sessions and Routing log tables
lay their columns out by one rule, with a minimum per column kind, one fill column and a set wider than the card
scrolling inside it, so the Requests table fits 1280 px with its default columns ([Requests](console.md#requests),
[Sessions](console.md#sessions), [Routing log](console.md#routing-log)). The Requests view's empty state reads "Widen
the range or clear a filter.", as the Routing log's does, and a request the router refused before it resolved a model
reads a dash for its model ([Requests](console.md#requests)). Unchecking Select all in a filter's list clears every
check, the inverse of checking it, and an empty list keeps its meaning ([Requests](console.md#requests), [Routing
log](console.md#routing-log)). A deleted key's group reads `<id> (deleted, created <time>)`, its creation instant in the
viewer's zone ([Analytics](console.md#analytics)). The Accounts section's card "Unused allowance by account" ranks the
subscription accounts by the share of their weekly windows left unused over the last four weeks, with the fleet's
figures ([Analytics](console.md#analytics)). The Analytics session view reads the session's rows through the route's
session filter, and each auto reload re-reads the first page alone ([Analytics](console.md#analytics)). The Analytics
view's Burn tab lays Plan per hour's three cards and Spend per minute out two by two where three would share a row and
the fourth would sit alone ([Analytics](console.md#analytics)). The Analytics view's Cache warming tab spans its last
card across the row where that card would sit alone, as the Burn tab does ([Analytics](console.md#analytics)). The
Analytics view's model grouping lists a request refused before it resolved a model under "no model"
([Analytics](console.md#analytics)). The Burn section closes on Member budgets, its reconciliation rows listed on the
Meter view ([Analytics](console.md#analytics), [Meter](console.md#meter)). The Cache warming tab shows what the probes
saved and the net saving after their cost, as tiles, a chart and a Saved column per pool
([Analytics](console.md#analytics)). The Client and User agent filters list the range's values and take a typed pattern,
each value a chip ([Analytics](console.md#analytics)). The Cost by chart's note names the unpriced groups it draws as
gaps ([Analytics](console.md#analytics)). The Cost split's subscription rows and the Plan per hour legend figures name
their window in full on hover ([Analytics](console.md#analytics)). The ledger records a null diagnostics member on a
request that named a message as `none` and a comparison the upstream was still running as `pending`, which Misses by
reason and its per-group table read as No divergence and Pending ([Analytics](console.md#analytics)). The Refusals by
category card narrows under the Client and User agent filters and its links carry the client pattern
([Analytics](console.md#analytics)). The Traffic section's Refusals by category card counts the answers the upstream
refused by category, each bar opening the Requests view on them ([Analytics](console.md#analytics)). The Traffic table's
Error rate reads a dash for a group with no requests ([Analytics](console.md#analytics)). The unused allowance card and
the Accounts detail's allowance block draw their shares under Window shares without the cost model, so a failed settings
read leaves them in place ([Analytics](console.md#analytics), [Accounts](console.md#accounts)). Under the Analytics
classifier grouping the requests with no classifier call are labeled `none`, as the Requests facet names them
([Analytics](console.md#analytics)). Without the Model week card the Burn tab's three cards share one row from 1344 px
([Analytics](console.md#analytics)). On the Sessions view a released subagent's record keeps its slot of the agent cap
for one cache TTL after the release and then leaves the engine ([Sessions](console.md#sessions)). The Sessions view
offers Companion only, Finished sessions (Show finished sessions) and Released agents (Show released agents) as filters
in its address, hides a finished session by default, and counts the rows the filters keep on its tiles
([Sessions](console.md#sessions)). The Sessions view orders its rows and their lines by the session's creation time,
newest first, or by its last activity through the filter bar's sort control ([Sessions](console.md#sessions)). The
Sessions view reads each row from main's conversation pin with an Effort column, draws a line for every pin of every
session, main's among them, with Effort, Context and a warm or cold state on a pin no recipe stands on, links each
line's agent to the request log and its account to the Accounts view, and lists the sessions the router lists within its
non-companion timeout ([Sessions](console.md#sessions)). The Sessions view reads the context in tokens from main's
conversation pin on every row and from each line's own pin, and the Requests column shows through the Columns control to
make room ([Sessions](console.md#sessions)). The Settings page's About card lists the session expiry and the
non-companion timeout, the two keys of the `sessions` block, each as a duration with its seconds beside it and its hover
naming the key ([Settings](console.md#settings)). The Settings page's About card names the bootstrap's body capture
limit beside its two retention lines ([Settings](console.md#settings)). A decision card's cache-map row folds a record
of four entries or more to one chip per tier with the count and the depth range and the latest three depths, behind a
control that lists every entry ([Routing log](console.md#routing-log)). An event panel's When reads in the viewer's
zone, and a shedding round's Rewrite cell names the window as the cost figures do ([Routing
log](console.md#routing-log)). A renewal after a hold carries the wait it followed on its card ([Routing
log](console.md#routing-log)). A guardrail's update records each changed field with its value before and after, as an
account's, a key's or a pool's update does ([Routing log](console.md#routing-log)). A request refused before it resolved
a model reads a dash in the Routing log's Model cell, as it does on the Requests view ([Routing
log](console.md#routing-log)). The candidate table of a routing decision, "Candidates, by rank", lists the chosen member
first and the ranked members by rank ([Routing log](console.md#routing-log)). The pick line of a decision names the tied
accounts, the runway that narrowed them and whether a draw chose among them ([Routing log](console.md#routing-log)). The
registry's change log records an edit of a pool, a key or an account as each changed field's value before and after, an
account edit that replaced the secret says so without its value, and a registry change's Account cell links the account
or the key it edited ([Routing log](console.md#routing-log)). A pool save that changes nothing writes no change row and
no event, so the change log shows real changes alone ([Routing log](console.md#routing-log)). The Routing log draws a
guardrail block's refusal record with the guardrail's name as a second chip ([Routing log](console.md#routing-log)). The
Routing log draws a pool save's keepalive policy as dotted fields ([Routing log](console.md#routing-log)). The routing
log lists a failed usage poll as `poll failed` and a stream cut after its head as `stream interrupted`, and a shedding
round that walks no pin and leaves one out off the window that fired reads `off window` ([Routing
log](console.md#routing-log)). The routing log lists an account whose plan ended as `plan ended` and the plan's return
as `plan restored`, each with the read's source and the profile's organization type, rate-limit tier and subscription
status ([Routing log](console.md#routing-log)). The Routing log reads a backoff's close by its cause, Resumed under a
clean poll and Ended by the plan's end under the plan's end ([Routing log](console.md#routing-log)). The Routing log's
Pace cell and hover read a lapsed usage-based seat's remaining share as a percent ([Routing
log](console.md#routing-log)). The Routing log's window reset panel reads Read from, the usage sample or the clock walk
([Routing log](console.md#routing-log)). The Meter view badges a ratio only where the priced share reaches the rise's
floor ([Meter](console.md#meter)). The Meter view in the Diagnostics group lists every subscription account's completed
windows of the last 30 days grouped by account and window kind, the weekly windows apart, each group collapsed to its
latest ratio, its off count and a 30-day ratio spark, and the Meter drift condition opens it on the account
([Dashboard](console.md#dashboard), [Meter](console.md#meter)). A confirmation toast shows ahead of the failures queued
behind the one on screen. A whole-day or whole-hour duration drops its zero part, so three whole days read `3d` and an
hour `1h`. Every data table alternates its row tone, with a hovered or selected row reading over it ([The
console](console.md)). The page behind an open dialog holds still and keeps its scroll position. The embedded console
builds on Node 26 or later, which the build script checks before npm runs ([Installation](installation.md#build)).
A pool's change rows name each keepalive policy member they moved as `keepalive.<member>`, and a new pool's row lists
the policy ([Console](console.md#routing-log)).
A Team or Enterprise seat prices under its tier word where the cost table holds it, and the log says when it does not
([Pairing](pairing.md#account-states)).
A plan-ended account answers the plan and the tier its last profile named, its earlier readings keeping the plan they
were priced under ([Pairing](pairing.md#account-states)).
`POST /admin/keys` and the key edit refuse an unknown member with `400` as the pool routes do ([API
reference](api-reference.md)).

Every finished request lands in the ledger as one row for the client request, one per attempt and one per routing
decision, folded into the hourly rollup the analytics read ([Statistics](statistics.md#the-ledger)). Each attempt
carries its token counts and the tier the router prices by, which a page or a route prices when it reads them, so a free
organization's and a usage-based seat's requests read unpriced in every figure ([Pricing](statistics.md#pricing), [Plan
meter](statistics.md#plan-meter)). A subscription account's plan-side figures read as a share of its 5-hour and 7-day
windows ([Plan meter](statistics.md#plan-meter)). The metrics listener serves `/metrics` in Prometheus text format
([Metrics](statistics.md#metrics)), and the `phoebis_attention` gauge carries what needs an operator
([Conditions and gauges](statistics.md#conditions-and-gauges)). A failed credential refresh raises `refresh_failed` in
the serious tone. A plan-side figure prices under the regime of the client's entrypoint, the interactive regime (the
desktop app, the IDE extension, the terminal and the third-party desktop) at twice the 5-hour and 1.25 times the weekly
capacity of the headless client and without the band, the relay reads the entrypoint in its position in the user agent,
and the ledger reconciles each account's completed windows against the meter every hour. Both retention keys default to
365 days, `stats.retention_days` for the requests, the samples and the routing log and `stats.rollup_retention_days` for
the rollups, and the Settings view's About card shows the two as the file sets them
([Retention](statistics.md#retention), [Settings](console.md#settings)). Each request row carries the instant the ledger
wrote it, `GET /admin/requests` reads the rows written after `landed_after` and answers the newest stamp among the rows
its filters admit as `landed_through`, at most a time-bounded read's end unless the read passes `fixed_end`. The
Requests view's auto reload polls by that stamp, and its live poll of a preset range reads to the router's clock, so a
long request that finishes below rows already shown lands between them in the same tint on the next read
([Statistics](statistics.md#the-ledger), [Requests](console.md#requests)). A request log read over a fixed range passes
`fixed_end`, so its `landed_through` may pass the range's end and a row that landed after it is read once ([The
ledger](statistics.md#the-ledger)). A reconciled window splits at a fall and sums its parts, a slid window writes no
row, the drift holds only where the rise and the priced share both reach 5%, and a window's two ends are readings of one
source ([Statistics](statistics.md#conditions-and-gauges)). The drift condition reads the windows that ended inside the
last eight days, so an account that serves nothing leaves it within eight days of its newest window's end
([Statistics](statistics.md#conditions-and-gauges)). The hourly purge frees captured bodies in slices of about 32 MiB, a
larger body going alone in a slice of its own, so the ledger's writer waits behind one slice at most, well inside its
timeout ([Statistics](statistics.md#retention)). The usage poll reads a usage-based Enterprise seat's answer:
extra-usage amounts with a fraction part round to the minor unit, the body's `spend` object is read beside them, and a
failed parse carries the parser's reason ([Quota samples](statistics.md#quota-samples)). A usage-based seat's requests
file under the funding `usage_based`, priced in dollars at the API rates and kept by the `billed` filter
([The ledger](statistics.md#the-ledger), [Plan meter](statistics.md#plan-meter)). A usage-based seat's account view
carries its spend window with the poll's amounts, the newest share and a reset estimated at 00:00 UTC on the first of
the next month until a header names one ([Quota samples](statistics.md#quota-samples)). The account view and the
Dashboard answer the 7-day Fable under the one id `seven_day_fable` ([Quota samples](statistics.md#quota-samples)). The
quota history of an `oauth` account answers a head's sample and a poll's at one instant where both landed in one bucket
([Quota samples](statistics.md#quota-samples)). A request the router's own shutdown ends past the drain window is booked
`canceled` under the code `router_shutdown`, apart from a caller that left on its own under `client_gone`
([The ledger](statistics.md#the-ledger)). The request log filters on the serving attempt's stop reason and refusal
category, and the analytics answer the refused requests in range per category
([Refused requests](statistics.md#refused-requests)). The analytics answer the weekly allowance each subscription
account left unused at its resets in range, from the stored window reset events
([Quota samples](statistics.md#quota-samples)).
The reconciliation fold drops a dip in a window's readings that the next readings reverse past what the attempts
predict, so a transient zero poll counts its window's rise once and holds no `meter_drift`
([Statistics](statistics.md#conditions-and-gauges)).
A window's reading carries the plan and the tier the account held when it was taken, and a window's reset event and the
allowance route answer that pair ([Statistics](statistics.md#quota-samples)).
A `poll_backoff_ended` event names its cause, a clean poll or the account's plan-ended pause
([Statistics](statistics.md#conditions-and-gauges)).
The profile is read again on every third usage poll, a quarter hour apart, so a tier or plan change reaches the router
within a quarter hour ([Statistics](statistics.md#quota-samples)).
A re-fold after a moved prompt-length threshold writes into no hour the rollup purge dropped
([Statistics](statistics.md#retention)).

Every price is computed when it is read, from the token counts the ledger stores, at the price revision in force when
the request arrived. The price table and the subscription cost model are revision lists keyed by effective date, the
account's tier and the prompt-length band join the hourly rollups, a model the cost model does not weight reads its
model tier's latest weight as an estimate and raises an attention item, and the cost model names each model's tier
([statistics](statistics.md#pricing), [installation](installation.md#stats)). The shipped price table carries Claude
Sonnet 4.6 and reads Claude Fable 5's cache read at $1 per million tokens in every revision
([statistics](statistics.md#pricing)).

The console prices every figure in the unit the shell's Unit switch names, native by default (a subscription request as
20x window equivalents, a billed request in dollars), with a legend beside it. The cost reference shows one row per
model tier with the older models folded, and a revision select ([analytics](console.md#analytics),
[cost reference](console.md#the-cost-reference)). The analytics, dashboard, keys and pools routes flag each window a
subscription request priced nothing on, its model in no tier the cost model weighs, and count the probes and kept-warm
credits a plan funded, so the Dashboard, Keys and Pools tiles read a dash with the hover "No subscription request
priced on this window: its model has no tier the cost model weighs" and the Analytics Cost by chart notes the groups it
draws unpriced in range ([dashboard](console.md#dashboard), [analytics](console.md#analytics)). The Requests and
Dashboard Cost cells follow the unit legend per position: a subscription request reads its dollar equivalent under All
in dollars and Dollar equivalent and a dash under Billed dollars, and a billed request its dollars under All in dollars
and Billed dollars and a dash under Dollar equivalent ([dashboard](console.md#dashboard),
[requests](console.md#requests)).

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

The router serves Claude Code alone: a request on the two Messages routes from any other client is refused `403`
`client_not_allowed` before its body is read, under every key ([Clients](routing.md#clients)). The contract gained the
version item, which refuses a `claude-cli/` version spelled any other way than the client's as `client_contract`, and
holds its version and session items on a token count and a keepalive probe ([The required
set](client.md#the-required-set)). A request is identified by its session header alone, and the session views key every
row by the client's own session id ([How a request is identified](client.md#how-a-request-is-identified)).
