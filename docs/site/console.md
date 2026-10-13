# The console

This page is for the operator reading or using a view of the web console the admin listener serves. It covers the
shell first (the login screen, the sidebar, the theme switch and the expert view), then one section per view in the
sidebar's order, each with its screenshot, what the view shows, what each control does and the dialogs it opens. The
mechanism behind a view lives on its own page, which each section links.

## The shell

The login screen asks for the admin secret in one field, "Admin secret", with the button "Log in", which reads
"Logging in…" while the router answers. A wrong secret reads `That secret does not match.`, a login the throttle
refused `Too many failed logins, so try again in a minute.` (the throttle's figures are on
[Installation](installation.md#admin)), and no answer `The router did not answer.`. When the router accepted the
secret and the browser kept no session cookie, the screen says so: `The router accepted the secret, but the browser
kept no session cookie. The usual causes: a proxy that rewrites the path or drops Set-Cookie, cookies blocked for this
site, or a Secure cookie served over plain HTTP.` While the router answers the session read with neither a session nor
a 401, the screen reads `The router did not answer ({status}).`, the status left out when no answer came at all, beside
a "Retry" button that reads the session again. A session that ends while a view is open returns to the screen with
`Your session ended, so log in again.`

The sidebar reads the brand "Phoebis Router" over the views in this order: Dashboard, Quota, Accounts, Pools, Keys,
Models, Requests, Analytics, Sessions, Settings. Under them a group headed "Diagnostics" holds Routing log, shown while
the expert view is on or while the open view is a diagnostics view, so a link into the log opens it whatever the mode
and leaves the mode as you set it. The sidebar's foot carries the "Expert view" switch and the router's version as
`v{version}`. The top bar reads the open view's title (the document title is `{title} - Phoebis Router`), the "Unit"
switch with its legend's button, "Log out", and the theme switch "Theme" with Light, Dark and System, stored under
`phoebis.theme` (`system` until you pick one). The time format lives under `phoebis.timeFormat` (`24h` by default). The
theme, the time format and the unit are browser preferences, which the Settings view's Appearance card also sets, and
nothing in the shell changes a router setting. On a narrow window a menu button opens the sidebar over the view.

The "Unit" switch sets the unit the Dashboard, Pools, Keys, Requests and Analytics views price a request in, stored
under `phoebis.unit` (`native` until you pick one). Its button "What the units mean" opens the legend, which closes on
Escape or a click outside it, with one line per position in the switch's order:

- Native: "Each request in the unit its funding pays in: a subscription request as 20x window equivalents, an API key,
  overage or fast-mode request in dollars."
- All in dollars: "Every request in dollars at the API price list, a subscription request at its dollar equivalent."
- Billed dollars: "The overage, fast-mode and API key requests alone, the dollars the account holder is charged."
- Window shares: "A subscription account's requests as shares of its own 5-hour, 7-day and model-week windows. Reads
  where one account is named, since shares of different accounts' windows do not add."
- Dollar equivalent: "The subscription requests alone at the API price list."

The legend ends on what a 20x window equivalent is: the share of a Max 20x account's window the same tokens would spend,
the cost model's unit over the Max 20x capacity, so figures of different account tiers compare and add. The 5-hour
column carries the peak band's weight, the 7-day column the week's factor, and a model week its own capacity factor. A
figure marked `~` is an estimate: its model has no measured weight and reads the latest weighed model of its model tier,
and its hover reads "Estimated at the model tier's latest weighted model". On a narrow window every position but Native
reads a short label (`All $`, `Billed $`, `Shares` and `Equivalent $`), and at a phone's width the legend's button
leaves the top bar and stays on the Settings view's Unit row. The cost model behind the equivalents is on [Plan
meter](statistics.md#plan-meter).

The expert view, whose switch carries the title "Shows every field of the pool and key dialogs, the advanced ones
included.", is stored in the browser under `phoebis.expertView` and starts off. With it on, the pool dialog's rail
gains the groups Auto mode, Placement, Load shedding, Failover and Cache diagnostics, its other groups show the fields
Pin sessions, Select below utilization, Overage grace (min), Wind down (min), Add a cache marker to a request without
one and the cache TTL's per-model overrides, and the member utilization bar gains its drag handle. The key dialog gains
Server-side auto mode, Pool rules and List [1m] twins in the models listing, and the sidebar the Diagnostics group.
With the mode off, a group with hidden fields closes on `{n} more settings in the expert view` (`1 more setting` for
one), whose link turns the mode on, and a hidden field that holds a refusal shows anyway. Closing a dialog with unsaved
changes asks "Discard changes?" with "This dialog has changes that are not saved.", "Keep editing" and "Discard".

## Dashboard

![The Dashboard view](assets/dashboard-light.png#only-light)
![The Dashboard view](assets/dashboard-dark.png#only-dark)

The Dashboard reads its figures again every 30 seconds. A line above the tiles names what needs your attention, worst
first, each item a badge that opens the account, pool or key where it is fixed, and reads "Nothing needs attention"
otherwise. An item with no such place (a drift refusal that named no key, a parked record without a pool, a pool
deleted since, one of the router's own writers, a page unknown to the console) stays a plain badge. The conditions are
the ones the `phoebis_attention` gauge exports ([Conditions and gauges](statistics.md#conditions-and-gauges)), and the
pool exhausted item opens the [routing log](#routing-log) on the pool's exhausted decisions from the moment the
condition began. The model weight estimated item opens the [Models](#models) view. A parked ledger record adds "Replay
parked records", which answers `Landed {n}, {n} still parked`.

Four tiles follow, each with a sparkline over the last 24 hours: "Requests, 24h" with `{n} per hour`, "Tokens, 24h"
with `{pct} served from cache`, "Estimated cost, 24h" in the top bar's unit, and "Error rate, 24h" with "No requests",
`Most common: {code}` or "No errors". Under Native the cost tile reads the subscription traffic's 20x 5-hour equivalent
over the billed dollars, a side with no traffic left out, with `{usd} billed per hour` under them, or the equivalent per
hour while no request was billed. Under All in dollars and Billed dollars it reads those dollars with `{usd} per hour`,
under Dollar equivalent the subscription traffic's dollars at the API price list, and under Window shares a dash with
the note "shares do not add across accounts", since the tile sums every account. An estimated figure carries the `~` and
the estimate's hover. Under the tiles a line says when the figures were read, `As of {ago}`, by the oldest of the page's
reads, or names the read whose last refresh failed and why: `Last refresh of the {read} failed: {error}. Showing data as
of {ago}`, the read being the 24-hour figures, the accounts, the recent requests, the sessions or the settings.

Two rings, "5h remaining" and "7d remaining", show how much of the pooled 5-hour and 7-day capacity is left, under the
subtitle "Subscription accounts, 5-hour window, weighed by tier" (and its 7-day twin), the first with a "Cost
reference" link that opens the dialog on Tiers and peak band. Each subscription account is weighed by its tier's
capacity on the window from the subscription cost model and listed with its own remaining share, the ring's center
reads the share "of capacity left", and the line under it reads `{n} of {m} {tier} windows left`, such as 0.39 of 1.30
Max 20x windows. A window whose reset has passed with no reading since counts as a full allowance, and an account whose
tier the model does not size, or has no capacity for on the window, is named under the ring with the reason. Without a
subscription account the card reads "No subscription accounts" and "A subscription account reports the window this
card draws.", and with none sized "No listed account's tier is sized for the 5-hour window." (or 7-day). Each account
keeps one color across the Dashboard, the Quota view and the Analytics, in the order of its id, the eight colors
cycling past eight accounts, and an account the registry does not list draws muted.

"Accounts" draws one small card per account, linking to its detail, with the status badge, the email, "Overage active"
while requests bill as extra usage, the cooldown line, and "API key" for an API-key account. "Recent requests" ("Last
10 across every pool", with "All requests" opening the Requests view) lists Time, Key, Account, Model, Status, TTFB,
Tokens and Cost, the status badge reading `×{n}` for a request of several attempts. Cost reads a request's dollars
under the three dollar positions, its account's shares under Window shares as the Requests view's column does, and
under Native a subscription request's 20x 5-hour equivalent alone, with the other windows, the dollar equivalent and
the price list and cost model in force on hover. Where the table runs wider than its card it scrolls inside it.
"Sessions" ("Sticky pins and cache-warming agents", with "Open") reads the active pins as `active pins across {n}
accounts` and the agents as Agents, Busy, Idle and Parked. What each figure means is on [Statistics](statistics.md).

## Quota

![The Quota view: the Accounts section](assets/quota-light.png#only-light)
![The Quota view: the Accounts section](assets/quota-dark.png#only-dark)
![The Quota view: the Quota windows section](assets/quota-windows-light.png#only-light)
![The Quota view: the Quota windows section](assets/quota-windows-dark.png#only-dark)

The Quota view covers the subscription accounts alone, since an API-key account has no quota window. Its head counts
them, `{n} accounts`, and those needing attention, `{n} need attention`, and offers "Refresh all", whose hover reads
"reads the endpoint now unless the account is backing off, and records the reading as the scheduled poll does". A
section bar shows one of two sections at a time: Accounts, where the view opens, and Quota windows, which a link or a
reload with `#quota-windows` lands on. The readings are the ledger's [quota samples](statistics.md#quota-samples).

Accounts draws a card per account, sorted "Default order" or "Soonest recovery", every card with the same slots in the
same order: the header with the label (the email on hover) and the status badge, one row per window ("5h", "7d" and
"Fable"), the Extra usage line, the chips and the poll reading. Each live window's meter reads the share left and its
pace, `{window}: {pct} left, {n}% ahead of pace`, `{n}% behind pace` or `on pace`, with ` to its end at {time}` where
the pace runs to the subscription's end rather than the reset, and a mark at the share of the window still to come.
Under the meter the window reads `resets {ago}`, or `expired {ago}` with "no reading since the reset, {time}" on hover
for a window whose reset has passed with no reading since, which draws no share and an empty meter until the next
reading, or "not reported". A card with no window yet reads "No quota windows reported yet.", and one whose read failed
shows the error with `, next attempt {time}` and a "Retry" button in the windows' place. The "Extra usage" chip reads
`off`, `not in use`, `in use` or `not reported`, with the month's spend against its limit while extra usage is enabled,
in the serious tone while requests bill as extra usage, and the provider's reason inside the line with the whole
message on hover. The chips name the plan by its tier ("Plan"), when the plan of an account marked canceled ends (in
the serious tone inside a week), and when a trial ends ("Trial"). The poll reading says when the poll last asked the
endpoint and how it answered, with the whole message and the backoff on hover, and "Refresh" reads this account's
endpoint now. A card that shows an error, whose last poll failed, or that is backing off counts as needing attention.

Quota windows lays the same windows across a calendar titled "Quota windows", with its span and length (`two weeks` in
the Weekly mode, `three days` in the 5-hour mode), "Previous period", "Today" and "Next period" buttons, the mode
switch Weekly or 5-hour, an "Account" axis and a now marker. Each lane carries the chips "5-hour limit", "7-day
limit" and "Fable 7-day limit", each with its share left and its reset on hover, and one bar per window, the current
bar filling from the window's start to its used share, so a fill past the now line is ahead of pace and one short of
it behind. An account marked canceled with a saved end shows the end on its lane: no window opens after it, and the
window that spans it stops there. The legend reads current window, upcoming, used, past window, now and plan ends,
and the note "Each bar is one full quota window, drawn from when it opened to when it resets, and labeled with the
instant it resets. Lanes ending together compete for the same days."

## Accounts

![The Accounts view: the list with the selected account's detail](assets/accounts-light.png#only-light)
![The Accounts view: the list with the selected account's detail](assets/accounts-dark.png#only-dark)

The list pane offers "Search accounts", a status filter with "All statuses" and the six status labels, the sort Name,
Soonest reset, Most used or Status, and "Add account". The six labels, with the state each stands for, are Cooling
down (`cooldown`), Needs funding (`funding_required`), Paused (`paused`), Needs re-auth (`reauth_required`), Ready
(`ready`) and Not paired (`unpaired`). A row reads the label, the email, `pool {id}` or "no pool", "Overage" while
requests bill as extra usage and "extra usage on" while it is enabled and unused. The pane reads "The router holds no
account yet" with "Add account" before the first account, "No accounts match" with "Clear the search or the status
filter." under a filter, and "No account to show" until a row is selected. What the states mean and how an account
moves between them is on [Pairing an account](pairing.md).

The detail opens on the account's label with "Edit label", its email and its badges: the status, `quarantined` while
the last refresh's outcome is uncertain, `cooldown ends {ago}`, "Overage active" and "Routing paused", with `Overage
since {ago}` under them while a sample reports the overage's start. An account whose refresh token was rejected
carries the callout "Routing is off for this account. The refresh token was rejected ({error}). Re-pair to restore
it." The actions, with the dialog each opens:

| Button | Dialog | What it does |
| --- | --- | --- |
| "Pair" or "Re-pair", on a subscription account | `Pair {label}` or `Re-pair {label}` | Opens the pairing dialog described below. |
| "Verify", on an API-key account | "Verify account" | "Sends a models request to the upstream with the stored key and reports the status.", and reads `Verify: upstream answered 200` on success. |
| "Pause" or "Resume" | "Pause account" or "Resume account" | "Paused accounts stay in their pool but receive no new sessions. Pinned sessions migrate on their next request." |
| "Catalog" | `Catalog of {label}` | Opens the account's model list, described below. |
| "Clear funding flag", while funding is required | "Clear funding flag" | "Overage becomes eligible again on the next request. If the provider still refuses to fund it, the flag comes back with the new reason." |
| "Disconnect" | "Disconnect account" | `Deletes {label}'s credential. Pool memberships and request history stay, and re-pairing stores the credential again.` |
| "Delete account" | "Delete account" | `Removes {label} from its pool and deletes it. Request history stays in the ledger.` |

"Identity" lists, for a subscription account, Type ("OAuth login"), Plan with its start, Rate-limit tier, Ends and
Trial ends where known, Organization and Billing type, and "Edit end" opens the end editor: the checkbox "Cancelled",
the field "Ends" with the hint "Prefilled from the plan's start and its monthly renewal. Correct it from the date the
plan's billing settings show. An annual plan is marked cancelled in its final month only.", Save and Cancel, and
"Reload" where the row changed under the editor. The card closes on "Renewal date is not reported by the provider."
For an API-key account it lists Type ("API key") and Windows ("None reported, billed per token"). "Usage" ("Remaining
share of each window") draws the three windows as the Quota view does, "Extra usage this month" with its spend meter
and an "in use" badge, the reason the provider gave for disabling extra usage, and a "Fast mode" line with the share of
the fast lane's input and output buckets used and a chip while the fast lane cools, or reads "This account does not
report quota windows." "7-day trend" ("Utilization of each window, sampled every 5 minutes") draws the 5-hour, 7-day
and Fable 7-day windows of a subscription account, or "No samples yet" with "Quota samples appear after the first
successful quota fetch." "Cooling down" lists Scope ("Whole account", "Fast mode", "Fast mode credits" or the model),
Reason and Until with a Clear button per row, or "Nothing cooling down", and Clear asks "Clear cooldown": "The account
becomes eligible immediately. If the account is still rate limited, the next request re-enters cooldown." "Pool" links
the pool with `priority {n}`, or reads "Not a member of a pool." "Credential" lists Access token, Refresh token, Last
refresh and Last error for a subscription account, and Secret ("stored encrypted at rest") and Last error for an
API-key account. "Requests" reads Last 24h, Last 7d and Lifetime, with a "Routing log" link that opens the log on the
account's rows over the last hour.

"Catalog" opens `Catalog of {label}`: `As of {ago}` with "Refresh", and the models as Model, Name, Released, Input
tokens, Output tokens and Effort levels, a model past the long-context bound carrying a "long context" badge, or "The
upstream lists no models."

"Add account" opens the dialog of that name under "Subscription logins share your Claude.ai quota. API keys bill per
token against a Console workspace.": the choice "OAuth login" or "API key", Label ("Short name used in pools and
logs."), the id the label derives as `Id: {id}` with a "customize" link that opens the Id field, the "API key" field
for an API-key account, and "Start pairing" or "Add API key". For a subscription account the dialog says "Pairing
opens the Claude.ai login in your browser and asks you to paste the address it lands on. The router stores the refresh
token encrypted and renews the access token on its own." The pairing dialog, `Pair {label}` or `Re-pair {label}`, runs
two steps. "Log in and paste the address" explains itself, "Open the authorization link, sign in to Claude.ai, and
approve the router. The browser then lands on a callback address that will not load, since nothing listens on that
port. Paste the URL from the address bar, or the code alone.", with the link beside "Copy" and "Open in browser", the
field "Paste the URL your browser landed on, or the code", `Registered redirect: {uri}`, the session's expiry and
Submit. "Confirm" reads "Confirm the identity below to store the refresh token for this account." over the Email and
Organization the upstream reported, and Confirm stores it. The toasts read "API key added" and `Paired {email}`. The
flow, the states and the `curl` form are on [Pairing an account](pairing.md).

## Pools

![The Pools view: the jobs pool exhausted at its budget](assets/pools-light.png#only-light)
![The Pools view: the jobs pool exhausted at its budget](assets/pools-dark.png#only-dark)

The view opens on "A pool groups accounts behind a set of keys. Requests pick a member by priority, stick to it per
session, and fail over when a member hits a rate limit." and "Create pool", then the tiles Pools, Accounts in pools,
Keys, "Requests, 24h" and "Cost, 24h". "Cost, 24h" and each card's Cost 24h read in the top bar's unit: under Native the
day's subscription traffic as its 20x 7-day equivalent over its billed dollars, a side with no traffic left out, and
under Window shares a dash, except on a card whose one member is a subscription account, which reads that account's
share of its own 7-day window. Every field's meaning is on [Pools, keys and routing](routing.md#pools).

Each card carries the pool's name, its status badge (active, degraded, disabled or exhausted), its keys as a link
("No keys yet", "1 key", `{n} keys`, or "Keys unavailable" when the keys read failed), an edit button and "Delete".
A strip of badges names the features the pool has on, "server-side auto mode off", "load shedding", "cache TTL" and
"keepalive", each opening the dialog on the group that edits it, the last followed by the pool's resolved ceilings in
whole hours, `idle {h} · parked {h} · horizon {h}` (a range such as `44h to 71h` where the pool's combinations differ).
One sentence says what the status means, and attention rows under it list the dashboard's conditions filtered to this
pool, one row per condition with its badge, its subject linked where it is fixed and its detail, the age on hover. The
pool's own exhausted row opens the [routing log](#routing-log) on its exhausted decisions since the condition began.

The serving table lists who serves next: Tier (`P{n}`, or `plan P{n}`, `overage P{n}` and `API keys P{n}` where the
pool prefers subscription accounts), Account, Standing and Pins. Standing reads `next` for the member a fresh session
lands on, `eligible`, `after plan quota` for a member in overage ranked after the plan-funded ones, `at the Pace floor`
or `winding down {left}` (on hover `Winding down to the plan's end at {time}`) for a member that serves only as the
last resort, and for a member passed over `cooldown {left}` (on hover `In a cooldown that ends {time}`), `overage until
{time}` (`overage` where the reset is unknown), `at budget` or `at budget until {time}` with the spend against the
budget, and `unavailable` with the account's status on hover. The caption names the strategy, `Pace`, `Priority, then
random` or `Round robin`, with `subscription accounts first` appended where the pool prefers subscription accounts,
and its hover notes "A request for a Fable model is placed on the Fable window instead, which this card does not draw."
Pins reads the member's active pins, one per agent and model as the card's Active pins fact counts them, a muted `0`
for none.
Then "Traffic, 24h" draws requests per hour with a 3-hour average, or "No traffic in the last 24h", and the facts
Members, Keys, Requests 24h, Cost 24h, Active pins and Moves in the last hour, whose figure opens the routing log on
the pool's moved decisions and whose hover lists the moves by reason. "Delete" asks `Delete pool {name}`: "Deletes the
pool and its memberships. Its keys must be moved or revoked first."

The pool dialog, "Create pool" or `Edit pool {name}`, is grouped: a rail "Pool settings" on the left, one group open
at a time, a status line reading "Every group saves together." or `Save is held: {reason}` (a pool that changed since
you opened it holds the save until "Reload"), and Cancel beside "Create pool" or "Save pool". On create the rail marks
Identity and Members "needed" and Keys "after saving". The groups by rail parent, with what the expert view adds:

| Rail parent | Groups | In the expert view alone |
| --- | --- | --- |
| Pool | Identity, Members, Keys, Models, Auto mode | Auto mode |
| Routing | Selection, Placement, Pinning | Placement, and Pin sessions under Pinning |
| Load | Utilization thresholds, Load shedding, Failover, Overage and plan end | Load shedding, Failover, Select below utilization under Utilization thresholds, and Overage grace (min) and Wind down (min) under Overage and plan end |
| Caching | Cache TTL, Keepalive | Add a cache marker to a request without one and the per-model overrides, both under Cache TTL |
| Visibility | Body capture, Cache diagnostics | Cache diagnostics |

Selection's Strategy field explains the chosen strategy under it. Under Pace it reads "Pace scores each member by the
allowance left in the window it reads, the model's own Fable week where the model has one, else the 7-day window, per
hour until that window resets or the subscription ends, within the first priority tier that has an eligible account. An
unsampled account counts as untouched, and ties are random. Each account's windows are on the Accounts page."
Utilization thresholds opens on "A window's pace is the share of it that has elapsed, to its reset, or to the
subscription's end when that comes first. A member runs ahead of pace by the points its used share exceeds the elapsed
share, read on its 5-hour and its 7-day window. The readings are the Quota page's.", holds the Shed ahead of pace switch
with its margin and, in the expert view, Select below utilization with its threshold, and closes on the "Member
utilization" bar, whose hint reads "Each tick is a member's latest 5-hour or 7-day reading, as on the Quota page. The
lower track reads each window's lead over pace, from 100 points behind to 100 ahead, with the shedding margin and the
cool band under it." The bar draws a tick per member window on the used track and on the lead track, the latter around
"on pace" with the margin as a line (hover `Shedding margin: {n} points`), the cool band "cool under here" and the
legend "5-hour reading", "7-day reading" and "Shedding margin", a used-track tick's hover reading `{account}: {window}
{used}` and a lead-track tick's `{account}: {window} {used} used, {elapsed} elapsed to its end at {time}, {lead}` (the
end named where the pace runs to the subscription's end), and in the expert view the "Select-below threshold" handle,
dragged or moved with the arrow keys. In the expert view, Load shedding closes on a "Preview" line, the round's dry run
as the fields stand: "No round runs while shed ahead of pace is off." while the switch is off, `No member is ahead of
pace by the margin: the closest is {account}, {lead} to its end at {time}.` while none is (the end named as in the hover
above), and otherwise `{n} members run ahead of pace by the margin: {account} ({window}, {lead} to its end at {time}).`
followed by `A round there would move {n} sessions ({n} warm), {pct} of the hot members' recent burn together, in window
share, to a cool sibling.`, or by `A round there would mark {n} sessions ({n} warm), {pct} of the hot members' recent
burn together, in window share, but no sibling reads under the margin less 5 points, so the sessions would be marked and
stay.`, the unit clause reading `, in dollars` for a burn in dollars and the together clause giving way to `of each
member's recent burn on average` where the members' units differ. Until the cost model is read the line says so and
counts no window-share pin. Keys holds "Create key" and the pool's keys as Key, Prefix and Status with a menu of "Move
to…" and "Revoke". "Move to…" opens `Move key {name}` ("A key reaches only its own pool's members, so moving it ends
its current session pins.") with the destination Pool, and "Revoke" asks `Revoke {name}`: `Requests with {name} are
rejected immediately. The key stays in this pool and its history stays in the ledger.` A key created from here opens
"Key created", the dialog the Keys section describes. Keepalive holds the switch "Keep prompt caches warm", the three
ceilings and the return rate: a ceiling left off reads the break-even it resolves to, `Break-even: {range}`, one
switched on past the break-even reads `Capped at the break-even, {range}`, the idle window reads its cost as a share of
one re-write, `Costs {pct} of one re-write`, and while the field reads 0 or more than the break-even, the window in
effect, `capped at the break-even, {time}`, with `at the saved {time}` for an unsaved edit. Return rate by idle age
lists the pool's ten buckets with the windows that returned out of those that reached it and a bar of the rate, or "No
idle windows recorded yet". The ceilings are explained under [The four fields](keepalive.md#the-four-fields) and the
buckets under [The return rate by idle age](keepalive.md#the-return-rate-by-idle-age).

## Keys

![The Keys view](assets/keys-light.png#only-light)
![The Keys view](assets/keys-dark.png#only-dark)

The view opens on the tiles Keys, Active, Lifetime requests and Lifetime cost, and the card "Lifetime cost by key"
("Relative spend across all keys"), a bar per key. Lifetime cost, each bar's figure and the cost line under a key's
"Usage, 7d" read in the top bar's unit: under Native the tile stacks the subscription traffic's 20x 7-day equivalent
over the billed traffic's dollars, and a bar's figure and a cost line read a key's equivalent beside its billed dollars,
a side with no traffic left out, and under Window shares a dash, since a key spans its pool's accounts. The table's
toolbar offers "Search keys", the status filter All statuses, Active, Disabled, Expired or Revoked, the pool filter "All
pools", and "Create key". The columns are Name, Prefix, Pool, Models, "Usage, 7d", Expiry and Status, and each row ends
in its menu. Under the name a row notes the capture setting, `bodies captured, {h}h` or `bodies not captured`, with
"inherited" where the key follows its pool and "keepalive off" for a key opted out of cache warming.
Models reads `{n} rules`, with "(replace)" where they replace the pool's, or "Pool rules" or "No rules".
Expiry reads the date or "Never". Status carries the key's state with `used {ago}` and, after a rotation, `{n} sessions
still on the previous secret`. The menu offers Edit, Rotate (`Rotate {name}`), Revoke (`Revoke {name}`: "Requests with
this key are rejected immediately. Its history stays in the ledger.") and Delete (`Delete {name}`: "Removes the key and
detaches its request history from the key name. This cannot be undone."). Before the first key the table reads "No keys
yet" with "Create key". What a key's settings mean is on [Keys](routing.md#keys).

"Create key" and Rotate end on the dialog "Key created" or "Key rotated", which shows the secret once with a copy
button under "Copy the secret now. The router stores only a hash, so it cannot be shown again." and "Set it as
`CLAUDE_CODE_OAUTH_TOKEN` on the client. The Settings page has the full snippet.", and closes on "Done".

The key dialog, "Create key" or `Edit {name}`, is grouped like the pool dialog, with the rail "Key settings" and the
groups Key (Name, Pool, Notes), Access (Enabled on an existing key, Expiry,
and Server-side auto mode with Inherit, On or Off), Models (Model rules, with Pool rules as Extend pool rules or
Replace pool rules, and List [1m] twins in the models listing), Sessions (Cache warming opt-out) and Capture (Capture
request and response bodies as Inherit from pool, an override in hours or off for this key). Server-side auto mode,
Pool rules and List [1m] twins show in the expert view. While the key captures, the Capture group warns what the
bodies hold, the warning [Keys](routing.md#keys) quotes.

## Models

![The Models view](assets/models-light.png#only-light)
![The Models view](assets/models-dark.png#only-dark)

The view opens on "The rules a key's requests are judged by, the first match deciding, and the models accounts refused:
a refused model cools on that account alone while the request fails over." beside a Key select, the chosen key kept in
the page's address as `key` (the first key leaves it out). A note under them reads "An entry naming an undated model is
written with a trailing * (claude-sonnet-4-5*), because the router routes, prices and records that model under its
dated snapshot." The card `Processed rules for {key}`, subtitled "Extends the pool's rules, and the models listing
shows [1m] twins" (or "Replaces", or "no [1m] twins"), lists the processed rules as Level, Rule (Allow or Deny), Pattern
and Cooling, the last naming the accounts cooling for a model the rule matches. Without a key it reads "No keys: a
key's processed rules show here once one exists.", and without rules "No rules: every model the pool's accounts serve
is allowed." Then one block per model, titled by the model's short name, lists the accounts cooling for it as Account,
Scope, Reason and Until with a Clear button per row, which asks "Clear cooldown" with the sentence the Accounts view
uses, or one "Cooling down" block reads "Nothing cooling down" while nothing cools. The button "View rejections and
permits in the routing log" opens the [routing log](#routing-log) on the model rejected and model permitted rows of
the last 24 hours. The rule grammar and the levels are on [Model rules](routing.md#model-rules).

## Requests

![The Requests view: the table with its client column](assets/requests-light.png#only-light)
![The Requests view: the table with its client column](assets/requests-dark.png#only-dark)

The filter bar opens on "Search id, session, key, account, model, error, message id", which matches the request id,
the session, the key, the model, the error code, and an attempt's account, upstream request id or upstream message id,
literally (`_` and `%` are characters). The range offers Last hour, Last 6 hours, Last 24 hours (the default) or
"Custom range" with two instants. The multi-selects Keys, Pools, Accounts and Models follow, then "Client pattern" and
"User agent pattern", which take a pattern (`claude-cli/2.1.*`, `*sdk-cli*`: `*` for any run of characters, `?` for
one, a backslash quoting either, and a value with no wildcard matching in full), then Status, Outcome, Cache miss
reason, Agent type, Turn origin, Effort and Request class, the Kind select (Any kind, Inference, Count tokens, Probe),
Speed, the "Held only" switch, the "Follow" switch, Refresh and the Columns control. The account filter keeps the
requests whose serving attempt ran on the account, so a request that failed over lists under the account that answered
it. Each multi-select opens with a Select all row, and the search and the two pattern fields apply 400 milliseconds
after the last keystroke, on Enter, or when the field loses focus. Every filter, the range and the page size live in
the page's address, so a reload or a shared link keeps them, and "Reset" clears them.

The table offers thirteen columns, Time, Key, Pool, Account, Model, Client, Kind, Transport, Status,
Attempts, TTFB, Tokens and Cost, with Kind, Pool and Transport hidden until the Columns control turns them on, and
shows Time, Model, Status and Attempts alone while the drawer is open. The model column reads the family and version
with the whole id on hover. The client column reads the class the router derived from the user agent
(`claude-code/<entrypoint>` or `other`), whole on hover, with the agent type beneath it on a subagent's
turn.
Status reads the code with the outcome
word beside it where the request did not succeed (`canceled`, `exhausted`, `failed`, `incomplete` or `rejected`). One
row is one client request however many attempts it took: Attempts reads `2 attempts` with the trail of account and
status on hover, nothing for a request of one attempt, and a `held {time}` badge for a request that waited for a slot
while every account was at its cap. Cost reads in the top bar's unit. Under Native a request a subscription window paid
reads its 20x 5-hour equivalent over its 20x 7-day one (`1.47% 20x 5h` over `0.244% 20x 7d`), with every window, the
model week among them, and the dollar equivalent on hover. Under the four other positions it reads its account's own
window shares, each with its window's short word (`0.12% Fable week` over `0.42% 5h`, the full labels on hover). An
overage, fast mode or API-key request reads dollars under every position, and a figure an estimate priced leads with
`~`, its hover adding "Estimated at the model tier's latest weighted model". The table pages by "Rows" (10, 25, 50 or
100, 25 by default) with First, Previous and Next, holds the requests up to the moment you opened it so paging does not
shift, and Refresh brings it up to now. An empty table reads "No requests match" with "Widen the date range or clear a
filter." What each figure means is on [Statistics](statistics.md).

Follow, off by default, reads the list every 5 seconds with the range ending now, takes every row that arrived since
the read before, up to eight pages, and lands a new row at the top in the accent tint until the next read. Past eight
pages a line reads "More rows arrived than Follow could fetch. Reload to read them.", and Next reads on from the last
row Follow fetched. A filter change keeps Follow on, paging to an older page turns it off, and the open drawer pauses
it.

Selecting a row opens the "Request details" drawer, with the request's id and time, a "Routing log" link that opens the
[routing log](#routing-log) on the request a quarter hour either side, and these sections:

- Outcome: Status, Outcome, Account, Model, Canonical model when it differs from the model the client spelled, TTFB,
  Duration, Error, Client drift when set, then the serving attempt's Answer (the stop reason, the speed, the
  geography, the service tier) and Cache (the miss reason, `{read} read, {written} written`, where the prompt
  diverged), and a Diagnostics row naming who put the cache diagnostics member on the body: "sent by the client",
  "added by the router" or none.
- Phases: a waterfall over Received, Selected, Written, Head, First content, Stream end and Finished, a phase the
  ledger holds no instant for reading "not measured".
- Usage: Input (uncached), Cache read, "Cache write, 5m", "Cache write, 1h", "Output, thinking", "Output, plain",
  Server tool calls and Estimated cost in the top bar's unit: a subscription request's 20x equivalents under Native, or
  its account's window shares under the four other positions, one chip per window with the dollar equivalent after them,
  and a billed request's dollars. Its hover names the serving account's tier (`Account tier {tier}`) and the price list
  and cost model in force when the request arrived (`Priced at the API prices of {date} and the cost model of {date}`).
- Request: Session, Agent, Client with the request class badge, User agent (such as
  `claude-cli/2.1.269 (external, cli)`), Key, Pool, Kind, and one `Attempt {n}` row per attempt with the upstream's
  request id and, on the answer that served, its message id.
- Request shape: Effort (the level the upstream applies), Thinking, Max tokens, "Tools, system blocks, messages",
  Context edits, Temperature, "Speed, service tier" and Auto mode, "not sent" where absent.
- Client hints: Agent type, Compaction, Context compacted, Turn origin and Tool time before this turn.
- Captured bodies: the request and the response, each with Copy all and "Open wide", which draws a body of any size
  pretty-printed in a window of its lines and a streamed answer as its events, or "Not captured for this key. Enable
  body capture on the key to record the exchange." or "Bodies for this request have aged out of retention." What
  capture stores and for how long is on [Body capture](statistics.md#body-capture).

A request the ledger does not hold reads "Request not found" with "It may have aged out of the ledger."

## Analytics

![The Analytics view: the Traffic tab](assets/analytics-light.png#only-light)
![The Analytics view: the Traffic tab](assets/analytics-dark.png#only-dark)

The sticky filter bar offers the range 1h, 24h, 7d, 30d or Custom (From and To, "End must be after start." while the
pair is wrong), "Group by" with Account, Client, Key, Effort, Kind, Model, Pool, Request class, Speed, User agent or
"Session (Burn only)", the multi-selects Key, Pool, Account and Model, "Client pattern" and "User agent pattern" in the
Requests view's grammar, a "User agent groups" box with one pattern per line, where each pattern names a group and every
other agent groups under `other` (`claude-cli/2.1.*` and `claude-cli/2.0.*` as two groups), a "Session" id filter, and
the switch "Include cache probes". The user agent grouping reads the agent's first product token, `claude-cli/2.1.269`
for `claude-cli/2.1.269 (external, cli)`. Filtered to one session the view sums the request log instead of the hourly
rollups and says so, over the first 10,000 requests of the range. Under that filter the Cost split reads "Cost split is
not available for a session view.", the Write cost vs read cost legend reads a dash for each cost, and Cache warming
reads "Cache warming figures are not available for a session view." with "not in the request log" under its probe tiles,
since a request row carries one cost total and no probe or kept-warm fact. The range, the grouping, every filter, the
client groups and the probes switch live in the page's address. A link that carries `unit` sets the top bar's Unit
switch to that position and leaves the address, so a shared link opens on its unit. The section bar under it shows one
section at a time, Traffic, Burn, Latency, Cache, Cost, Cache warming or Accounts, and a range with nothing reads "No
traffic in this range." What each figure means is on [Statistics](statistics.md).

Traffic reads `{n} requests, {pct} errors` and draws Requests by outcome (OK, Rate limited, Failed, Incomplete and
Exhausted), Errors by code, Failovers, Migration rewrites with Moves by reason, Requests by speed, Effort levels, and
the table Traffic by the chosen group with Requests, OK, Rate limited, Failed, Incomplete, Exhausted, Rejected, Error
rate, Failovers, Search and Fetch.

Burn follows the page's grouping, and "Session (Burn only)" groups it by session while every other section keeps the key
grouping, since no rollup carries a session. Of the filters, the Accounts filter and the user agent groups alone apply
to it, which its head says while another filter is set. Plan per hour draws three cards, "5-hour per hour", "7-day per
hour" and "Model week per hour", the last while a group's traffic ran on a model with a week of its own, each with one
line per group. Under Window shares a line is the share of a subscription account's own window, which reads under the
account grouping or with one account named in the Accounts filter, and under every other position it is the group's
subscription traffic per hour in 20x equivalents of the window. The 5-hour card shades the peak band's hours behind its
lines, and each card's note carries a "Cost reference" link that opens the dialog on Tiers and peak band. A hollow dot
marks a bucket whose model has no measured weight and reads the latest weighed model of its model tier, with an
"estimated" entry in the legend and a `~` on the legend's and the tooltip's figures. A bucket whose traffic ran on a
model of no known tier, or on a tier the cost model has no capacity for on that window, prices no share there and draws
no point, under a "model unlisted" entry, and under Window shares an account whose tier the cost model does not size
reads "tier unknown". Spend per minute charts the dollars alone, a subscription account's among them for its fast-mode
and overage answers. The section closes on Member budgets, Pool, Account, Spent, Budget and Period ends, one row per
budget a member carries, a member at its budget marked and one the ledger has not reported on reading "not reported".

![The Analytics view: the Burn tab's plan chart shading the peak band](assets/analytics-burn-light.png#only-light)
![The Analytics view: the Burn tab's plan chart shading the peak band](assets/analytics-burn-dark.png#only-dark)

Latency reads "Percentiles read from merged per-hour histograms" and draws one row per metric, Time to first byte,
Time to first content token, Total duration and Output tokens per second, each with a percentile chart (p50, p90, p95,
p99) and a Distribution card, then Latency by the chosen group with TTFB p50, TTFB p99, TTFT p99, Duration p99 and
tok/s p50.

Cache reads `{pct} hit ratio, {n} expired between turns, {pct} of input read from cache` and draws Tokens (Uncached
input, Cache read, Cache write 5m, Cache write 1h, Thinking, Output), Hit ratio and cached share, Write cost vs read
cost, Misses by reason with its per-group table, Cache by model (Reads, Writes 5m, Writes 1h, Hit ratio, Cached share,
Read cost, Write cost) and Placements by rung (Rung, Placements, Hits, Hit share, Tokens read, Tokens promised). Write
cost vs read cost and Cache by model's Read cost and Write cost read in the top bar's unit. Under Native they read the
subscription requests' 20x 7-day equivalent, the card's subtitle naming the billed requests' dollars and each Cache by
model cell carrying them on a line under it, and the card draws the billed dollars while no subscription request is in
range. Under the three dollar positions they read the position's dollars, and under Window shares the named account's
7-day shares, the card reading "Shares of one account's windows alone: name one account in the Accounts filter." and
Cache by model dashes under the same note while no account is named. Every count in Misses by reason but the expired
column links to the Requests view on those requests with the page's filters carried over, and a count the Requests view
cannot list exactly stays plain with the reason on hover.

Cost reads in the top bar's unit. Under Native the tiles read Cost, the subscription requests' 20x 7-day equivalent
over the billed requests' dollars, a side with no requests left out, and Cost per request and Cost per 1k output
tokens the same two ways. Cost by the chosen group stacks the equivalent per bucket with "Billed by {group}" beside it
in dollars, the Cost split draws a ring per side by token kind, the subscription requests' with its 7-day equivalent in
the center and the billed requests' in dollars, and the table reads Requests, Cost, 20x 5-hour, the model week's 20x
column, Per request and Per 1k output, its Cost the 20x 7-day equivalent, and Cost, Per request and Per 1k output each
with the billed dollars on a line under the equivalent. A subscription side the route could not price on the 7-day
window, as when a request ran on a model of no known tier, reads a dash, and the Cost split draws no ring for it. Under
All in dollars, Billed dollars and Dollar equivalent Cost draws the tiles Estimated cost, Billed dollars or Dollar
equivalent, Cost per request and Cost per 1k output tokens, then Cost by the chosen group as a chart and a table
(Requests, Cost, Input, Cache read, Cache write, Output, Per request, Per 1k output) and the Cost split as one ring in
dollars (Input, Cache read, Cache write, Output). Under Window shares, which reads under the account grouping or with
one account named in the Accounts filter, the tiles read Window share, Per request and Per 1k output, the chart one line
per group, the table Plan requests, 5-hour, 7-day, the model week, Per request and Per 1k output, and the Cost split the
whole in dollars. Every position draws Fast mode spend, and an estimated figure carries the `~` and the estimate's
hover. The prices behind the dollars are on [Pricing](statistics.md#pricing), and the cost model behind the shares and
the equivalents on [Plan meter](statistics.md#plan-meter).

![The Analytics view: the Cache warming tab](assets/analytics-cache-warming-light.png#only-light)
![The Analytics view: the Cache warming tab](assets/analytics-cache-warming-dark.png#only-dark)

Cache warming draws the tiles Probe cost and Kept warm, Probes by outcome (OK, Migrated, Held, Skipped, Failed), Wasted
probes (Wasted probes, Wasted cost, Share of probe cost), Kept-warm requests, Kept-warm tokens and Cache warming by
pool (Probes OK, Held, Skipped, Failed, Kept-warm requests, Kept-warm tokens, Probe cost). Probe cost, Wasted cost,
Share of probe cost and the by-pool Probe cost read in the top bar's unit: under Native the plan probes' 20x 7-day
equivalent over the billed probes' dollars, under the three dollar positions the position's probes in dollars, and under
Window shares the named account's share of its 7-day window, a dash under "Shares of one account's windows alone: name
one account in the Accounts filter." while none is named. What a probe is and when it counts as wasted is on
[Keepalive](keepalive.md).

Accounts draws Peak 5-hour utilization, then Window usage and Pace under a "Window" switch whose positions are the
windows the range sampled, 5-hour, 7-day, 7-day Fable and "7-day, extra usage". Pace reads "The lead over pace per
bucket: above zero ahead, below behind, from the quota samples, before the router's burn projection", each account's
hover adding its Pace score, the remaining share over the hours from the sample's instant to the window's reset. The
table Accounts in range lists Peak 5h, Peak 7d, "Peak 7d, extra usage", Time in overage, Time in cooldown, Pins
created, Migrated in and Migrated out. The section keeps reading the rollups under a session filter, since they carry
no session, and its subtitle says so.

## Sessions

![The Sessions view: the tiles and the table with a fork badge](assets/sessions-light.png#only-light)
![The Sessions view: the tiles and the table with a fork badge](assets/sessions-dark.png#only-dark)

The view opens on "Every session within the session expiry, with the account each model is pinned to and each agent's
cache-warming recipe." with the disclosure "How pins and recipes work", whose paragraph says what a pin and a recipe
are, and a "Cost reference" link that opens the dialog on Plan rates. The tiles read Sessions (`{n} of {max}`, the
engine's cap), Agents (`{n} of {max}` with `{n} busy, {n} idle, {n} parked`, less the subagents the mod released while
their session lives, which draw no line and keep their slot until the session leaves, named on hover), Retained
(`{n} MiB of {max} MiB`, "prompts kept for probes"), "Cache probes, 24h" (a bar by outcome and `{n} ok ({pct}), {n}
migrated, {n} held, {n} skipped, {n} failed`), "Expired between turns, 24h" ("misses cache warming exists to prevent")
and "Bus gaps" ("times the keepalive engine missed bus events"). A tile whose read failed draws a dash with the reason
on hover, and Try again under the tiles reads them again. The view reads its sessions every 15 seconds and its tiles
once a minute. The probes and the recipe are explained on [Keepalive](keepalive.md), the mod's words under
[The mod](keepalive.md#the-mod) and the recipe's states under [The five states](keepalive.md#the-five-states).

The card "Sessions" ("One row per session. Open a row for the cache-warming recipe each agent carries") offers "Expand
all" or "Collapse all" and the Columns control. The Session column is always drawn, and the ten others are Key,
Model, Account, Last used, Requests, Last request, Agents, Companion, Cache warming and Next probe, with
Agents, Last request and Next probe hidden by default. The session cell cuts a long id with an ellipsis,
whole on hover.
A session that branched from another conversation at depth 2 or deeper carries a fork
badge under its id, `fork, on another account` or `fork, same account` where the session it branched from is listed
with a pinned account, and `fork at 3` otherwise, with the base, the depth and the base's account on hover, and
following the badge scrolls to the base's row. Companion reads the mod's state, `reporting` with how long ago the
person was last seen at the terminal, `silent` or `none`. Cache warming reads the pin state badge, `busy`, `idle`,
`parked`, `released` or `unknown`, with the recipe summary, or "no recipe". Next probe reads the instant, "pending" or
"not scheduled". Before the first session the card reads "No live sessions" with "A session appears here on its first
request, or as soon as the keepalive mod registers it, and drops off once it passes the session expiry."

A row's menu offers "View in analytics" and "View in requests" once the session has ledgered traffic, "View in routing
log", which opens the log on that session's decisions an hour either side of its last request, and "Release". Release
asks "Release session": `Session {id} loses its pin to {account} and every recipe on it is dropped. The next request
picks an account again and the prompt cache on {account} goes cold.`, and answers "Session released".

Opening a row reads `Created {time}, {ago}` and one line per agent that has sent a real request, as Agent, Model,
Account, TTL, Last request, Last probe, Next probe ("on first stream event" or "not scheduled"), Probes / credited and
State, main first, then the busy, idle and parked agents. State reads the recipe's state word (`ended`, `held`, `not
retained`, `paused` or `retained`) with the cause beside it and the sentence of each on hover, and under them the
mod's lifecycle word for the agent with a parked agent's park kind and its deadline or wake time (`parked · background
· deadline in 1h 12m`). A session whose every agent the mod released reads "The mod released every agent of this
session. Their records keep their slots of the cap until the session leaves the engine." in the lines' place, keeps its
released badge and an Agents count of 0. A session without a recipe reads "no recipe" and `Nothing is scheduled because
{reason}.`

### The cost reference

The "Cost reference" link opens the dialog "Cost reference" under "What a million tokens costs: in dollars at list
price, and as a share of a subscription window." on three tabs, Tiers and peak band, Plan rates and API prices. The
Sessions view's head and the pool dialog's Warm sessions hint open it on Plan rates, the pool dialog's Write cost hint
on API prices, and the subtitle of the Dashboard's 5-hour ring and the notes of the Burn section's Plan per hour cards
on Tiers and peak band. It opens on the entries in force now. Its footer carries a "Revision" select over both files'
revision dates, newest first, a date not yet in force marked `(next)`, and choosing a date redraws every tab at the
entry each file had in force on that date, or at a file's first entry when the date comes before it. A single date
across the two files shows no select. The footer names each table's entry as a chip, `API prices, effective {date},
shipped` (or `file`) and `Subscription cost model, effective {date}, shipped` (or `file`), the hover naming the key and
the path that set it or would replace it at the next start (`stats.api_prices_path`, `stats.subscription_costs_path`).
The console has no import control: a table is replaced by a file at that path, read at the next start.

![The cost reference dialog on Plan rates](assets/cost-reference-plan-rates-light.png#only-light)
![The cost reference dialog on Plan rates](assets/cost-reference-plan-rates-dark.png#only-dark)

Tiers and peak band draws Windows by tier, a row per tier the cost model sizes with its 5-hour and 7-day window as
bars labeled as a multiple of the smallest tier's window, a Peak band switch that redraws them for a request completing
inside the band, and the Peak band card: a week, Monday to Sunday in the browser's time zone, with the band's hours on
each day it covers, a marker at now, a caption naming this week's hours here and in the zone the cost model defines
them in, whether the band runs now and until when, the next change of the local hours, and each window's capacity
factor. Plan rates draws the share of a tier's 5-hour and 7-day window that a token count of each type spends, per
model, for a request completing outside or inside the band, with a tier chooser (the anchor tier Max 20x first), the
band control and a token field in millions with quick picks for 100k, 1M and 10M, a weight column per model, a Fable
week row under a Fable model's 7-day group, and a warning tint on a cell past a full window. A model the cost model does
not weigh, on a model tier with a weighed model, reads that tier's latest weighed model, each of its cells marked with a
`~` and its weight cell reading `estimated, as {model}`, and the caption adds "A ~ marks an estimate: the model reads
the latest weighed model of its model tier." A model of no model tier, or of one with no weighed model, keeps its dashes
and reads "not weighed". API prices draws the dollar list prices per million tokens, input, cache read, cache write 5m,
cache write 1h and output, with a fast-mode row under each model that serves fast mode and a row under each model priced
by prompt length, under the caption "USD per million tokens at list price" and its note "An API-key account pays these.
A subscription account pays them in overage and for fast-mode answers." Both tables show one row per model tier, its
newest model, in the cost model's `tier_order` with `model_tier` naming each model's tier and a model of no tier last,
and fold the older models under one row at the table's foot that names their count, "1 older model" or `{n} older
models`, and expands to them in the same order. The fold is shared by both tables and starts closed at each opening. The
price rows, the weights and how a probe is priced are on [Pricing](statistics.md#pricing), which lists the shipped
revision in force, and the dialog shows an earlier entry under the Revision select.

## Settings

![The Settings view: Appearance, About and the client block](assets/settings-light.png#only-light)
![The Settings view: Appearance, About and the client block](assets/settings-dark.png#only-dark)

"Appearance" ("Stored in this browser only.") sets the Theme, Light, Dark or System ("System follows the operating
system preference."), the Time format, 12h or 24h ("Applies to every timestamp and axis tick."), and the Unit, which
mirrors the top bar's switch with a legend button of its own ("The unit every view prices a request in. Native reads a
subscription request as 20x window equivalents and a billed request in dollars."), the three browser preferences the
shell reads. "About" lists Version, Profile, Metrics listener, Ledger path, API prices path and Subscription costs path
(each as the path or "not configured", then its table's entry in force, its revision count and the next date while one
is pending, as `effective {date}, {n} revisions, next {date}`, the last reading `{problem}` in their place while the
cost model reports a problem), Storage path, and Admin secret as "configured" or "not configured", each path's hover
naming the bootstrap key that sets it, and closes on "The values the configuration file sets are on the installation
page, and `phoebis check-config` prints this router's."

"Downstream client configuration" ("Put it in the env block of the client's settings file or export it in the shell
that launches the client, and replace the token with a key from the Keys page.") shows the seven lines that point
Claude Code at the router, with "Copy snippet". The base URL carries the proxy listener's scheme (`https` where the
listener terminates TLS) and the port it binds, on the placeholder host `router.example.com`, and the hint under the
block reads `Inference is served on {proxy}, the address the proxy listener binds, so the base URL carries its scheme
and port. Replace the placeholder host with the name that reaches the listener, and the scheme and the port too where a
TLS proxy or a port mapping stands between Claude Code and it.` One definition per variable follows, with
`CLAUDE_CODE_PROMPT_CACHE_TTL=5m` defined although it is not in the block.
[The environment block](client.md#the-environment-block) on The client explains each line. The view writes browser
preferences alone and changes no router setting.

## Routing log

![The Routing log view: a decision card open](assets/routing-log-light.png#only-light)
![The Routing log view: a decision card open](assets/routing-log-dark.png#only-dark)

The Routing log sits under the sidebar's Diagnostics heading, shown while the expert view is on or while the log is
open, and every link into the log opens it whatever the mode. It is one stream of routing decisions and the events
that explain them, newest first, 25 rows a page with First, Previous and Next. The rules a decision follows are on
[Pools, keys and routing](routing.md).

The filter bar reads the range (Last hour by default, Last 3 hours, Last 6 hours, Last 24 hours or "Custom range" with
From and To), "Kinds" (the decision kinds, then the events, with "Probe decisions (hidden by default)" and "Warm prefix
expired (hidden by default)" last in their groups, the button reading "Kinds (2 hidden)" until another set is chosen),
Keys, Pools, Accounts, Models, "Session id" (a prefix matches) and "Request id", the "Follow" switch, which reads
"polling every 5 s, the range ends now" while on, and Refresh. Every filter lives in the page's address (a preset range
by its name, a custom range as `from` and `to`, two UTC instants, the lists as comma lists), so a filtered view is a
link and Back returns to the view before it. A request filter opens a line above the table that counts the request's
decisions and events and offers three widenings, "this session" (an hour either side), "this account" (a quarter hour
either side) and "this minute", and any other narrowing draws the same line with what the rows are narrowed to. Follow
behaves as on the Requests view, up to eight pages a read and the line "More rows arrived than Follow could fetch.
Reload to read them." past them. An empty range reads "No decisions or events in this range." with "Widen the range or
clear a filter."

A row reads Time (a decision's request beneath it as a link to the request's drawer), Kind (a decision in a filled
badge with `attempt 2/3` beneath a failover attempt, an event in an outlined one), Session (the first eight characters
with the agent beneath), Account (a move names the account it left and the one it landed on), Model, By, Fact, and
Details, which opens the request's drawer. By names what decided the row in one chip: the rule that fired (`spent
rule`), the rung that placed it (`static rung`), the strategy and the tier's priority (`Pace P0`, whose hover is the
Pace hint), where a hold waited (`cohort`, `session pin`), what exhausted the pool (`budget`, `cooldown`, `fast lane`),
the refusal (`client drift`, `key revoked`), `no rule fired` for a renewal, or an event's reason (`window spent`, `429
on 5h`). Fact gives the value that made it so as a muted label with its value, `gap`, `idle`, `prefix`, `until`, or a
shedding rule's window facts, `5h ahead`, `7d elapsed` and `5h to its subscription's end at` or `7d to its
subscription's end at` where the pace runs to the subscription's end. A verdict reads in one color across the rows,
the card and the strip: good when it served the request, warning when it changed its course, critical for a refusal or
an exhaustion, and neutral for none and no. Every account, key, pool, model, session and request a row names is a link.

Every row opens beneath itself. A decision row loads its record and draws the decision card. Its verdict line names the
decision with its accounts and its pool as links ("Moved secondary to primary", "Attempt 2 of 2: placed on primary",
"Exhausted in jobs") and chips for the tier, the rule or the rung, the pin's warmth and the cache verdict (`cache
pending` before the attempt reports usage). "How the router decided" draws the walk as a strip of stations in the
engine's order, each with its verdict, the path the decision took in the accent, every branch it did not take as a
stub, a step the record does not carry dashed and reading "not run", and the three ways out as termini, Chosen, Held
for a slot and Refused, the one reached filled in its tone. A click or a hover on a station, a candidate or a terminus
opens its facts in a popover, and Tab, Enter, Space and Escape walk them from the keyboard. "Candidates, in the walk's
order" lists every pool member with the chosen or held-on member tinted: Account with its plan beneath, Tier (`plan
P0`, `overage P0`, `keys P1`), 5h, 7d and Fable as three lines each (the utilization, the reset, the reading's source
and age), In flight against the cap, the Pace score under a header naming the window it read, `Pace, 7-day left per
hour` or `Pace, Fable week left per hour` (`Pace` where no subscription candidate names one), each cell's hover reading
`{pct} of the 7-day window left over {n} h to its reset at {time}: {rate}. Inside its tier, the highest above the pace
floor wins.` (or `to its subscription's end at {time}`), Prefix, and Verdict with the one chip that decided it (`429 on
5h until 15:42`, `budget $1.10 of $5.00`), whose hover says why the member was passed over. Three folds, "Request
facts", "Pin and warmth" (a record that pins nothing reads why: not looked up or pinning off) and
"Settings in force" (the nineteen settings, or "The key and its pool had left the
registry"), stay closed until opened. "Outcome" lists the pin written, the cache-map entries the answer wrote
(`pending` until it ends), the cache read with the TTL tier of its deepest entry, the hold, the answer an exhaustion or
a refusal gave, and the spend against a budget. The foot repeats the three widenings after "Widen:", names the record's
version as `Record v{n}`, and "Copy as JSON" copies the record route's answer whole. A record the retention purged
reads "The record left with the retention", and one still loading "Loading the record".

An event row opens to a panel of pairs: When to the second in UTC, the account and the pool as links, then the kind's
own pairs. An event that names a request ends with "Source request", which opens the log on that request a quarter
hour either side, and "Details", which opens the request's drawer. An account state change names who moved the state:
`credential refused`, `credit balance`, `funding retried`, `refresh failed`, `by hand` or `paired`, and a deleted
account's last row reads `deleted`. The kinds and what each row records:

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

Other views open the log on what they show: a Requests row and its drawer on the request a quarter hour either side,
the Sessions row menu's "View in routing log" on the session's decisions an hour either side of its last request, a
pool card's "Moves in the last hour" figure on the pool's moved decisions over the last hour, the card's pool
exhausted row and the dashboard's pool exhausted item on the pool's exhausted decisions from the moment the condition
began, the "Routing log" action on an account's Requests card on the account's rows over the last hour, and the Models
view's button on the model rejected and model permitted rows of the last 24 hours.
