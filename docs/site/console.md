# The console

This page is for the operator reading or using a view of the web console the admin listener serves. It covers the
shell first (the login screen, the sidebar, the theme switch and the expert view), then one section per view in the
sidebar's order, each with its screenshot, what the view shows, what each control does and the dialogs it opens. The
mechanism behind a view lives on its own page, which each section links. Every data table alternates its row tone, and
a row's hover or selection reads over it.

Every list view (Accounts, Analytics, Guardrails, Keys, Pools, Quota, Requests, the Routing log and Sessions) opens on
one filter bar, which draws the slots the view has in one order: the range pills, the search, Add filter, the view's own
controls with its primary action last, among them on a view with an order the Sort control, a button reading the key and
its direction whose popover offers the view's keys and Ascending or Descending, the choice carried in the address, and
at the right edge the split Refresh button and the count of what the view lists. When the window is too narrow for the
bar's controls on one line, the trailing ones wrap onto a second row. Add filter opens a popover over the view's
filters, and each filter in force stands as a chip on a line under the bar. The Refresh button's body reads the view
again now, and its dropdown sets auto reload to Off, 5 s, 10 s, 30 s or 1 min, 5 s when a view opens and off on Requests
and the Routing log. Auto reload waits while the tab is hidden, while a drawer, a dialog or a menu is open and on an
older page, with a muted mark and the cause in the dropdown's hover. The filters, the search and the range live in the
view's address, so a reload or a shared link keeps them, and the interval stays with the open page. An Accounts address
that names no account, or names one the router no longer holds, opens the first account the router lists among the rows
the list shows, or the router's first account where the filters hide every row.

## The shell

The login screen asks for the admin secret in one field, "Admin secret", with the button "Log in", which reads
"Logging in…" while the router answers. Under the field, the checkbox "Stay logged in", off by default, keeps this
browser logged in for 30 days, renewed on every visit. A secret rotation ends a kept login earlier, and so does "Log
out" on a kept login, which ends every kept login. No view lists the open logins. A wrong secret reads `That secret does
not match.`, a login the throttle refused `Too many failed logins, so try again in a minute.` (the throttle's figures
are on [Installation](installation.md#admin)), and no answer `The router did not answer.`. When the router accepted the
secret and the browser kept no session cookie, the screen says so: `The router accepted the secret, but the browser
kept no session cookie. The usual causes: a proxy that rewrites the path or drops Set-Cookie, cookies blocked for this
site, or a Secure cookie served over plain HTTP.` While the router answers the session read with neither a session nor
a 401, the screen reads `The router did not answer ({status}).`, the status left out when no answer came at all, beside
a "Retry" button that reads the session again. A session that ends while a view is open returns to the screen with
`Your session ended, so log in again.`

The sidebar reads the brand "Phoebis Router" over the views in this order: Dashboard, Quota, Accounts, Pools, Keys,
Guardrails, Requests, Analytics, Sessions, Settings. Under them a group headed "Diagnostics" holds Routing log and
Meter, shown while the expert view is on or while the open view is a diagnostics view, so a link into either view opens
it whatever the mode and leaves the mode as you set it. The sidebar's foot carries the "Expert view" switch and the
router's version as `v{version}`. The top bar reads the open view's title (the document title is `{title} - Phoebis
Router`), the "Unit" switch with its legend's button, "Log out", and the theme switch "Theme" with Light, Dark and
System, stored under `phoebis.theme` (`system` until you pick one). When the router answers from a build other than the
one the console's files were built from, a banner above the top bar reads `The router runs a newer build than this
console. Reload to pick it up.` beside a "Reload" button, and it stays in view with the top bar until the page reloads.
The time format lives under `phoebis.timeFormat` (`24h` by default). The theme, the time format and the unit are browser
preferences, which the Settings view's Appearance card also sets, and nothing in the shell changes a router setting. On
a narrow window a menu button opens the sidebar over the view. At a phone's width the top bar wraps a view title it
cannot hold beside its controls onto a second line.

The Accounts, Analytics and Pools views load their code on first open. A view that fails to draw tells a bundle that
changed under the tab ("This page could not load") from an error while drawing ("This page hit an error"), each with its
own words and a Reload button.

The "Unit" switch sets the unit the Dashboard, Pools, Keys, Requests and Analytics views price a request in, stored
under `phoebis.unit` (`native` until you pick one). Its button "What the units mean" opens the legend, which takes the
focus, closes on Escape or a click outside it, and hands the focus back to its button on Escape, with one line per
position in the switch's order:

- Native: "Each request in the unit its funding pays in: a subscription request as 20x window equivalents, an API key,
  fast-mode, overage or usage-based request in dollars."
- All in dollars: "Every request in dollars at the API price list, a subscription request at its dollar equivalent."
- Billed dollars: "The API key, fast-mode, overage and usage-based requests alone, the dollars the account holder is
  charged."
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
included.", is stored in the browser under `phoebis.expertView` and starts off. With it on, the pool dialog's rail gains
the groups Auto mode, Placement, Slow-down, Failover and Cache diagnostics, and its other groups show the fields Pin
sessions, Windows' Burn average (s), Load shedding's Preset, Shed cap (%), Shed dwell (s), Warm sessions and Preview,
Overage grace (min), Return to plan, Return dwell (s), Wind down (min), Add a cache marker to a request without one and
the cache TTL's per-model overrides. The key dialog gains Server-side auto mode, Pool rules and List [1m] twins in the
models listing, and the sidebar the Diagnostics group. With the mode off, a group with hidden fields closes on
`{n} more settings in the expert view` (`1 more setting` for one), whose link turns the mode on, and a hidden field that
holds a refusal shows anyway. Closing a dialog with unsaved changes asks "Discard changes?" with "This dialog has
changes that are not saved.", "Keep editing" and "Discard".

## Dashboard

![The Dashboard view](assets/dashboard-light.png#only-light)
![The Dashboard view](assets/dashboard-dark.png#only-dark)

The Dashboard reads its figures again every 30 seconds. A line above the tiles names what needs your attention, worst
first, each item a badge that opens the account, pool or key where it is fixed, and reads "Nothing needs attention"
otherwise. The line keeps one row of items and folds the badges past it behind a "+N more" badge that opens the full
list, so the tiles hold still. An item with no such place (a drift refusal that named no key, a parked record without a
pool, a pool deleted since, one of the router's own writers, the model weight estimated item, whose estimate shows in
the cost reference dialog, a page unknown to the console or a condition the console does not know, which reads "Unknown
condition" with its kind on hover) stays a plain badge. The
conditions are the ones the `phoebis_attention` gauge exports ([Conditions and
gauges](statistics.md#conditions-and-gauges)), and the pool exhausted item opens the [routing log](#routing-log) on the
pool's exhausted decisions from the moment the condition began. The four routing conditions read as one sentence each,
the subject first, then the count, the
settings they name (with the pool's values where the item carries them) and the start, such as
`main: 4 rounds marked nothing, no destination had runway, window_ceiling 95 %, since 14:45` for a pool whose rounds
found no destination with runway. An account's count reads `2 moves landed and the 5h window was spent within the hour`
where moves onto it were followed by a spent window, or `1 wall reached on the 5h window` where it reached its wall,
followed by `the last round projected 96 %` where a projection round on that window came before, or by
`the wall came 25 min before the reset` where a slow-down round came before (`the wall came at the reset` for a wall at
or after the reset). An account slowed past twice its pool's stretch threshold within the day reads, under the label
"Slowed past twice the stretch",
`studio: slowed 2 h 8 min of the last 24 h, over 1 h, slowest 0.35x, slow_down_stretch_seconds 1800, since 09:12`, the
bar after `over`, the lowest factor of its spans and the pool's threshold. Each opens the [routing log](#routing-log) on
the pool's shedding rounds, the account's moves and spent windows, its spent windows, or its slow-down starts and ends,
from the moment the condition began. An account whose meter drifted reads
`acct-a: 5h charged 2.0x the table, 7d 1.3x, since 14:20`, each off window with the ratio the meter charged against the
cost model, under the label "Meter drift against the table", and opens the [Meter](#meter) view on the account. A
parked ledger record adds "Replay parked records", which answers
`Landed {n}, {n} still parked`. Where the line is too narrow for its lead, the "+N more" badge and that button with its
answer together, the button and its answer take a second row.

Five tiles follow, each with a sparkline over the last 24 hours: "Requests, 24h" with `{n} per hour`, "Tokens, 24h"
with `{pct} served from cache`, "Estimated cost, 24h" in the top bar's unit, "Error rate, 24h" with "No requests",
`Most common: {code}` or "No errors", and "Refused, 24h" with `Most common: {code}` or "None", which counts the
requests the router refused before any account was asked. "Requests, 24h" and the error rate leave those out and say
so on hover, "Requests the router asked an account for. The refused requests are in the Refused tile." on the first and
"Errors among the requests the router asked an account for, the refused requests left out" on the second. The Refused
tile's hover names the refusals it counts apart from the router's own, "Requests refused for the client's or a policy's
cause before any account was asked. The router's own refusals, account_canceled, accounts_unavailable,
body_budget_exhausted, key_body_reads, ledger_behind and router_unavailable, count as requests instead." Under Native
the cost tile reads the subscription traffic's 20x 7-day equivalent over the billed dollars, a side with no traffic left
out, with `{usd} billed per hour` under them, or the equivalent per hour while no request was billed. Where a
subscription request of the day priced nothing on the 7-day window, its model in no tier the cost model weighs, the
subscription side reads a dash with the hover "No subscription request priced on this window: its model has no tier the
cost model weighs", and the note under it reads the dash per hour while no request was billed. Under All in dollars and
Billed dollars it reads those dollars with `{usd} per hour`, under Dollar equivalent the subscription traffic's dollars
at the API price list, and under Window shares a dash with the note "shares do not add across accounts", since the tile
sums every account. An estimated figure carries the `~` and the estimate's hover. Under the tiles a line says when the
figures were read, `As of {ago}`, by the oldest of the page's reads, or names the read whose last refresh failed and
why: `Last refresh of the {read} failed: {error}. Showing data as of {ago}`, the read being the 24-hour figures, the
accounts, the recent requests, the sessions or the settings.

Two rings, "5h remaining" and "7d remaining", show how much of the pooled 5-hour and 7-day capacity is left, under the
subtitle "Subscription accounts, 5-hour window, weighed by tier" (and its 7-day twin), the first with a "Cost
reference" link that opens the dialog on Tiers and peak band. Each subscription account is weighed by its tier's
capacity on the window from the subscription cost model and listed with its own remaining share, the most of its window
left first, the ring's center reads the share "of capacity left", and the line under it reads
`{n} of {m} {tier} windows left`, such as 0.39 of 1.30 Max 20x windows. A window whose reset has passed with no reading
since counts as a full allowance, and an account on a free plan, one whose tier the model does not size, or one that has
no capacity for on the window, is named under the ring with the reason. Without a subscription account the card reads
"No subscription accounts" and "A subscription account reports the window this card draws.", and with none sized "No
listed account's tier is sized for the 5-hour window." (or 7-day). Each account keeps one color across the Dashboard,
the Quota view and the Analytics, in the order of its id, the eight colors cycling past eight accounts, and an account
the registry does not list draws muted.

"Accounts" draws one small card per account, linking to its detail, with the status badge, the speed chip beside it on a
slowed account (`0.5x`, as [Quota](#quota) describes it), the email, the window bars or the spend bar of a usage-based
seat, "Overage active" while requests bill as extra usage, the cooldown line, and "API key" for an API-key account.
"Recent requests" ("Last 10 across every pool", with "All requests" opening the Requests view) lists Time, Key, Account,
Model, Status, TTFB, Tokens and Cost, the status badge reading `×{n}` for a request of several attempts. Time is the
column that gives way: the time reads whole, and the request id under it is cut by the room past its first twelve
characters, whole on hover, while Key and the other columns read whole. Cost reads as the Requests view's column does:
under All in dollars and Dollar equivalent a subscription request's dollar equivalent over `equivalent`, under Billed
dollars a dash and under Window shares its account's shares, and a billed request's dollars under each of the four but
Dollar equivalent, where it reads a dash. Under Native a subscription request reads its 20x 5-hour equivalent alone,
with the other windows, the dollar equivalent and the price list and cost model in force on hover, a client classifier
call's hover leading with "Free on the plan: a client classifier call takes no window share", and a billed request its
dollars. The column header's hover names what each kind of request reads under the position. Where the table runs wider
than its card it scrolls inside it. "Sessions" ("Sticky pins and cache-warming agents", with "Open") reads the active
pins as `active pins across {n} accounts` and the agents as Agents, Busy, Idle and Parked. What each figure means is on
[Statistics](statistics.md).

## Quota

![The Quota view: the Accounts section](assets/quota-light.png#only-light)
![The Quota view: the Accounts section](assets/quota-dark.png#only-dark)
![The Quota view: the Quota windows section](assets/quota-windows-light.png#only-light)
![The Quota view: the Quota windows section](assets/quota-windows-dark.png#only-dark)

The Quota view covers the subscription accounts that are paired, not awaiting re-authentication and whose plan has not
ended, since an API-key account has no quota window. Its bar offers "Refresh all", which reads the endpoint of each of
those accounts and whose hover reads "reads the endpoint now unless the account is backing off, and records the reading
as the scheduled poll does", and at the right edge the split Refresh button, whose body reads the account list again
("Reads the list again now"), and the count of those accounts, `{n} accounts`, followed in the critical ink by those
needing attention, `, {n} need attention`. Auto reload starts at 5 s. A section bar shows one of two sections at a time:
Accounts, where the view opens, and Quota windows, which a link or a reload with `#quota-windows` lands on. A card's
reading is the router's reported sample where the router holds one for the window and the ledger's [quota
sample](statistics.md#quota-samples) otherwise. The router answers no quota window for an account in the `disconnected`
or `reauth_required` state or whose plan ended (its `plan_ended_at` set), whatever reading it held before, so no view
draws one for it.

Accounts draws a card per account, sorted "Default order", "Soonest recovery", "5-hour pace", "7-day pace" or "7-day
Fable pace", every card with the same slots in the same order: the header with the label (the email on hover) and the
status badge, one row per window ("5h", "7d" and "Fable"), the Extra usage line, the chips and the poll reading. A
usage-based seat's card draws one Spend row in the windows' place and no Extra usage line, and "Soonest recovery" reads
its spend window's reset. The three pace orders put the account furthest ahead of that window's pace first, the one
whose used share runs furthest past the share of the window elapsed (the lead the router answers for each window), and
the accounts without the window, with the window expired or without a lead on it last. Each live window's meter reads
the share left and its pace, `{window}: {pct} left, {n}% ahead of pace`, `{n}% behind pace` or `on pace`, with ` to its
end at {time}` where the pace runs to the subscription's end rather than the reset, and a mark at the share of the
window still to come. On a screen under 900 px wide the reset label takes a line of its own under the meter. Under the
meter the window reads `resets {ago}`, followed on a window the router projects by the share the projection leaves at
the window's horizon: `resets in 2h 13m, 19% left at the reset` where the horizon is the reset (within a minute),
`resets in 3d 5h, 56% left in 5 h` where it comes earlier (five hours out on a weekly window, or the account's end), and
`{pct} left now` once it has come. A window the sessions spend before its horizon reads the time until it runs out,
`spent in 2 h 10 min`, counted back from the projection at the horizon along the burn, so the span follows the router's
forecast with the charges and the reservations the last reading does not hold yet, and `0% left now` once that time has
passed. The reset and the projection split over two lines where the column does not hold them on one, and the hover adds
the burn behind the projection, `the sessions on this account burn 8.5% an hour`. On a slowed account the 5-hour
window's line ends in the speed chip, `0.5x` in the warning tone (the account's slow-down factor to two decimals, a
trailing zero dropped, `0.35x`), whose hover reads `slowed since 17:40: the sessions on this account would pass the
ceiling 42 min before the reset, over the 30 min threshold` (`under` while no round can fire). The chip draws while the
factor is under 1 and the 5-hour window the read came from has not reset since, and the Dashboard's account cards, the
Accounts view, the Pools cards and the Sessions view draw the same chip with the same hover
([Slow-down](routing.md#slow-down)). The window bars of the Dashboard's account cards and of the Accounts list draw the
projection alone in the reset's place, in a compact form (`19% left at reset`, `56% left in 5 h`) that splits between
the share and the horizon where the bar is narrow, with the reset's instant on hover, and keep the reset where the
window has no projection. The Accounts detail's bars read the whole label, split between its two parts where the bar is
narrow, and the 5-hour bar's label carries the chip on a slowed account. The Pools dialog's Preview, the Sessions Runway
column and the routing log keep the used share, the figure that the window ceiling of a pool is measured against. A
window whose reset has passed with no reading since reads `expired {ago}` instead, with "no reading since the reset,
{time}" on hover, and draws no share and an empty meter until the next reading, and a window the account does not report
reads "not reported". A card with no window yet reads "No quota windows reported yet.", and one whose read failed shows
the error with `, next attempt {time}` and a "Retry" button in the windows' place. A usage-based seat's Spend row reads
the share of its monthly spend limit left, `59% left`, over the meter, with the amount spent of the limit in the
reading's currency on hover and `resets {ago}` under it, and no pace, since the router projects no burn on the seat.
With no limit set its chip reads the amount spent, `$1,025.00 spent`, and no meter draws, and a seat with no reading yet
reads "No spend reading yet." in the windows' place. Until a reset header names the spend window's reset, the router
estimates it as 00:00 UTC on the first of the next month, and the line reads `resets {ago}, estimated`, the word in the
muted ink, with `resets {time} (estimated: no reset header has been seen for this window yet)` on hover. The "Extra
usage" chip reads `off`, `not in use`, `in use` or `not reported`, with the month's spend against its limit while extra
usage is enabled and the reading carries the amounts, in the serious tone while requests bill as extra usage, and the
provider's reason inside the line with the whole message on hover. The chips name the plan the router resolved from the
organization type and the tier ("Plan", reading "Free", "Pro", "Max 20x" or "Usage-based", the tier on hover), when
the plan of an account marked canceled ends (in the serious tone inside a week), and when a trial ends ("Trial"). The
poll reading says when the poll last asked the endpoint and how it answered, with the whole message and the backoff on
hover, and "Refresh" reads this account's endpoint now. A card that shows an error, whose last poll failed, or that is
backing off counts as needing attention.

Quota windows lays the same windows across a calendar titled "Quota windows", with its span and length (`two weeks` in
the Weekly mode, `three days` in the 5-hour mode), "Previous period", "Today" and "Next period" buttons, the mode switch
Weekly or 5-hour, an "Account" axis and a now marker over the bars at the instant's place in the span. It draws one lane
per listed account with a window or a spend reading, and each lane carries the chips "5-hour limit", "7-day limit" and
"Fable 7-day limit", each with its share left and its reset on hover, and one bar per window in the account's color, the
current bar filled from the window's start by its used share in a stronger tone, so a fill past the now line is ahead of
pace and one short of it behind, the upcoming window dashed and the elapsed ones muted. A usage-based seat's lane
carries one chip, "Spend limit", with the amount left of the limit (`$1,475.00 of $2,500.00 left`, or the amount spent
where no limit is set) and the reset with the estimate's word on hover, and one bar per monthly spend window, the
current one from one calendar month before the reset to the reset, filled by the share spent and labeled as a plan bar
is. The current bar is labeled with the share left and its reset, `59% left · Sep 19 20:10`, or with the share alone
where the bar is too narrow for both, and a bar too narrow for its label keeps it on hover. On a narrow screen each lane
stacks its account and its chips above its bars, the now marker and the day grid cross the bars alone, the marker's pill
under the day labels, and the day axis prints short labels. A 5-hour cell too narrow for its full labels prints the hour
alone, `06` under the 24-hour time format and `6 AM` under the 12-hour one. Under the 12-hour format the 5-hour mode
draws fewer, wider cells where twelve would be too narrow for `12 AM`. An account marked canceled with a saved end shows
the end on its lane: no window opens after it, and the window that spans it stops there. The legend reads current
window, upcoming, used, past window, now and plan ends, and the note "Each bar is one full quota window, drawn from when
it opened to when it resets, and labeled with the instant it resets. Lanes ending together compete for the same days."

## Accounts

![The Accounts view: the list with the selected account's detail](assets/accounts-light.png#only-light)
![The Accounts view: the list with the selected account's detail](assets/accounts-dark.png#only-dark)

The bar above the list and the detail offers "Search accounts", Add filter with the Status filter ("All statuses" and
the six status labels, its chip reading such as "status: Paused"), the Sort control with Name (ascending by default),
Soonest reset (ascending, an account with no reset ahead last under either direction), Most used (descending) or Status
(ascending), and "Add account", and at the right edge the split Refresh button and the count of the rows the list shows,
`{n} accounts`. The search, the sort with its direction and the status live in the page's address, so a link or a reload
keeps them, and they stay while an account opens. Auto reload starts at 5 s and waits while a dialog or an editor of the
detail is open, or while its save runs, and while it is off the page reads the list every 2 s after a pairing's confirm,
with the same wait, until the account's first reading lands or 30 s pass. The six labels, with the state each stands
for, are Cooling down (`cooldown`), Needs funding (`funding_required`), Paused (`paused`), Needs re-auth
(`reauth_required`), Ready (`ready`) and Not paired (`unpaired`). A row reads the label with a badge that names the plan
the router resolved from the organization type and the tier ("Free", "Pro", "Max 5x", "Max 20x", "Max", "Team",
"Enterprise" or "Usage-based", none on an API-key row), the status badge with the speed chip beside it on a slowed
account, the email, `pool {id}` or "no pool", "Overage" while requests bill as extra usage and "extra usage on" while it
is enabled and unused. The pane reads "The router holds no account yet" with "Add account" before the first account, "No
accounts match" with "Clear the search or the status filter." under a filter, and "No account to show" until a row is
selected. What the states mean and how an account moves between them is on [Pairing an account](pairing.md).

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

"Identity" lists, for a subscription account, Type ("OAuth login"), Plan, the plan the router resolved, with its start
(`Max 20x since {date}`), Rate-limit tier (`{tier}, usage-based` on a usage-based seat), Ends where known, Plan ended
where set, Trial ends where known, Organization and Billing type, and "Edit end" opens the end editor: the checkbox
"Canceled", the field `Ends ({zone})`, whose label names the browser's time zone, with the hint "The date the plan's
billing settings show as its end. An annual plan is marked canceled in its final month only. The time is in this
browser's zone.", Save and Cancel, and "Reload" where the row changed under the editor. Beside the saved end the row
answers `plan_ended_at`, the instant a profile read found the account's organization holding no plan, which the router
reads as an end that has passed until a profile read or a pairing finds a plan again, and the Quota view leaves the
account out while the flag stands ([Account states](pairing.md#account-states)). The card closes on "Renewal date is not
reported by the provider." For an API-key account it lists Type ("API key") and Windows ("None reported, billed per
token"). "Usage" ("Remaining share of each window") draws the three windows as the Quota view does, or a usage-based
seat's spend bar, "Spend limit" with `{used} of {limit}` over its meter and its reset line, the speed chip beside the
5-hour bar's label on a slowed account, "Extra usage this month" on every account but a usage-based seat, with an "in
use" badge and its amounts and spend meter where the reading carries them, the reason the provider gave for disabling
extra usage, and a "Fast mode" line with the share of the fast lane's input and output buckets used and a chip while the
fast lane cools, or reads "This account does not report quota windows." ("No spend reading yet" on a usage-based seat,
or "No quota window is read since the plan ended {ago}." on an account whose plan ended), or "The first quota reading is
in flight." from a pairing's confirm until the account's first reading lands, where a usage-based seat reads "No spend
reading yet". "7-day trend" ("Utilization of each window, sampled every 5 minutes") draws the 5-hour, 7-day and Fable
7-day windows of a subscription account, or "No samples yet" with "Quota samples appear after the first successful quota
fetch."

"Allowance left at reset" draws, for a subscription account that is not a usage-based seat, one column per weekly window
(the 7-day window, then each model week): the share left unused at the latest reset with "left at the reset of {date}"
("left at {date}" and the badge where that reset is the term's end), the three weeks before it with their figures, and
"Total, {n} weeks", each week's hover naming its reset's instant and where its reading came from. The figures follow the
top bar's unit: the share of the account's own window under Window shares, and under every other position the share's
20x window equivalent through the account's tier, a dash where the cost model holds no capacity for the account's plan
(a Free plan among them, the card then noting it). A week the account's term ended inside counts as a reset with the
badge "term ended", a clock-walk reset reads the last reading before it as the event carries it, and the card reads "No
weekly reset recorded yet. The first shows once this account's week ends." before the first. The card reads its weeks
once per account, again on the bar's Refresh, once a weekly window resets and once the account's term end moves (its
cancellation flag, saved end or plan-ended flag), and a failed read draws its error with Try again.

"Meter drift" draws, beside it, one line per window kind the account's completed windows hold: the newest completed
window's ratio of the upstream's rise over the router's priced share (a warning badge where the rise and the priced
share each reach 5% and the ratio leaves 0.8 to 1.25, muted where the band does not judge it), its end and the windows
off the band over all of the trailing 30 days, with "Meter" opening the [Meter](#meter) view on the account, or "No
completed window yet".

"Cooling down" lists Scope ("Whole account", "Fast mode", "Fast mode credits" or the model), Reason and Until with a
Clear button per row, or "Nothing cooling down", and Clear asks "Clear cooldown": "The account becomes eligible
immediately. If the account is still rate limited, the next request re-enters cooldown." "Pool" links the pool with
`priority {n}`, or reads "Not a member of a pool." "Credential" lists Access token, Refresh token, Last refresh and Last
error for a subscription account, and Secret ("stored encrypted at rest") and Last error for an API-key account.
"Requests" reads Last 24h, Last 7d and Lifetime, with a "Routing log" link that opens the log on the account's rows over
the last hour.

"Catalog" opens `Catalog of {label}`: `As of {ago}` with "Refresh", and the models as Model, Name, Released, Input
tokens, Output tokens and Effort levels, or "The upstream lists no models."

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
Organization the upstream reported, and Confirm stores it. Where the router refuses the confirm because the account
holds a credential sealed under another storage key, the step reads `{label} holds a credential sealed under another
storage key, so disconnect it before pairing again` and offers Disconnect beside Cancel in Confirm's place. Disconnect
deletes the sealed credential and starts a fresh pairing at the first step, "Log in and paste the address". The toasts
read "API key added" and `Paired {email}`. The flow, the states and the `curl` form are on [Pairing an
account](pairing.md).

## Pools

![The Pools view: the jobs pool exhausted at its budget](assets/pools-light.png#only-light)
![The Pools view: the jobs pool exhausted at its budget](assets/pools-dark.png#only-dark)

The view opens on its bar, "Create pool" with the split Refresh button and the count `{n} pools` at the right edge, then
"A pool groups accounts behind a set of keys. Requests pick a member by priority, stick to it per session, and fail over
when a member hits a rate limit." on a line of its own, then the tiles Pools, Accounts in pools, Keys, "Requests, 24h"
and "Cost, 24h". Auto reload starts at 5 s and waits while the pool dialog or the Delete pool confirm is open. "Cost,
24h" and each card's Cost 24h read in the top bar's unit: under Native the day's subscription traffic as its 20x 7-day
equivalent over its billed dollars, a side with no traffic left out and the subscription side a dash with the
Dashboard's hover where a request of the day priced nothing on the 7-day window, and under Window shares a dash, except
on a card whose one member is a subscription account, which reads that account's share of its own 7-day window. Every
field's meaning is on [Pools, keys and routing](routing.md#pools).

Each card carries the pool's name, its status badge (active, degraded, disabled or exhausted), its keys as a link ("No
keys yet", "1 key", `{n} keys`, or "Keys unavailable" when the keys read failed), an edit button and "Delete". A strip
of badges names the features the pool has on and the state of its load shedding, in the rail's order: `1 guardrail` or
`{n} guardrails` with any attached, "server-side auto mode off", `sheds above 95 %` (the pool's window ceiling) with the
pool's load shedding on or `shedding off` with it off, `slows above 2 h 30 min` (the pool's stretch threshold as a span,
`slows above 30 min` on a pool set to 1800) with its slow-down on, "cache TTL" and "keepalive", each opening the dialog
on the group that edits it, the last followed by the pool's resolved ceilings in whole hours, `idle {h} · parked {h} ·
horizon {h}` (a range such as `44h to 71h` where the pool's combinations differ). One sentence says what the status
means, and attention rows under it list the dashboard's conditions filtered to this pool, one row per condition with its
badge, its subject linked where it is fixed and its detail, ending with its age, the whole on hover. On a narrow card
the detail and the age take a line of their own under the badge. The pool's own exhausted row and its Rounds without a
destination row open the [routing log](#routing-log) on its exhausted decisions or its shedding rounds since the
condition began, and an account's Destination spent within the hour row opens it on the account's moves and spent
windows, its Wall reached under the ceiling row on its spent windows, and its Slowed past twice the stretch row on its
slow-down starts and ends. An account's Meter drift against the table row opens the [Meter](#meter) view on the
account.

The serving table lists who serves next: Tier (`P{n}`, or `plan P{n}`, `overage P{n}` and `metered P{n}` where the pool
prefers subscription accounts, the metered tier holding the API-key accounts and the usage-based seats), Account,
Standing and Pins. Standing reads `next` for the member a fresh session lands on, `eligible`, `after plan quota` for a
member in overage ranked after the plan-funded ones, `winding down {left}` (on hover `Winding down to the plan's end at
{time}`) for a member that serves only as the last resort, and for a member passed over `cooldown {left}` (on hover `In
a cooldown that ends {time}`), `overage until {time}` (`overage` where the reset is unknown), `at budget` or `at budget
until {time}` with the spend against the budget, and `unavailable` with the account's status on hover. On a card whose
pool has the slow-down on, Standing carries the speed chip of a slowed member, `0.5x` with the Quota view's hover where
the account is loaded. The caption names the strategy, `Pace`, `Priority, then random` or `Round robin`, with
`subscription accounts first` appended where the pool prefers subscription accounts, and its hover notes "A request for
a Fable model is placed on the Fable window instead, which this card does not draw." Pins reads the member's active
pins, one per agent and model as the card's Active pins fact counts them, a muted `0` for none. Then "Traffic, 24h"
draws requests per hour with a 3-hour average, or "No traffic in the last 24h", and the facts Members, Keys, Requests
24h, Cost 24h, Active pins, Warnings and Moves in the last hour. The card's request figures, the count in the chart's
head, the bars per hour and Requests 24h, leave out the requests the router refused before any account was asked. Active
pins counts every pin under the pool's keys, as the Dashboard's Sessions card does. A pin outlives its account's
membership, so where the figure exceeds the sum of the Pins column, a line under it reads `{n} on a former member`, or
`{n} on former members` for more than one. Warnings counts the pool's warnings with their sentences on hover, and its
figure opens the dialog on the group of the first warning's first field. The figure of Moves in the last hour opens the
routing log on the pool's moved decisions, and its hover lists the moves by reason. "Delete" asks `Delete pool {name}`:
"Deletes the pool and its memberships. Its keys must be moved or revoked first."

The pool dialog, "Create pool" or `Edit pool {name}`, is grouped: a rail "Pool settings" on the left, one group open
at a time, a status line reading "Every group saves together." or `Save is held: {reason}` (a pool that changed since
you opened it holds the save until "Reload"), and Cancel beside "Create pool" or "Save pool". On create the rail marks
Identity and Members "needed" and Keys and Guardrails "after saving", and the Keepalive group's "Keep prompt caches
warm" switch starts off. The groups by rail parent, with what the expert view adds:

| Rail parent | Groups | In the expert view alone |
| --- | --- | --- |
| Pool | Identity, Members, Keys, Models, Guardrails, Auto mode | Auto mode |
| Routing | Selection, Placement, Pinning | Placement, and Pin sessions under Pinning |
| Load | Windows, Load shedding, Slow-down, Failover, Overage and plan end | Slow-down, Failover, Burn average (s) under Windows, Preset, Shed cap (%), Shed dwell (s), Warm sessions and Preview under Load shedding, and Overage grace (min), Return to plan, Return dwell (s) and Wind down (min) under Overage and plan end |
| Caching | Cache TTL, Keepalive | Add a cache marker to a request without one and the per-model overrides, both under Cache TTL |
| Visibility | Body capture, Cache diagnostics | Cache diagnostics |

Selection's Strategy field explains the chosen strategy under it. Under Pace it reads "Pace scores each member by its
spare rate: the allowance left in the window it reads, the model's own 7-day Fable window where the model has one, else
the 7-day window, per hour until that window resets or the subscription ends, less what its sessions burn of that window
an hour, within the first priority tier that has an eligible account. An API-key member scores the room left in its
worst bucket. An unsampled account counts as untouched, and ties are random. Each account's windows are on the Accounts
page."

Windows opens on "The windows the router works from: the share of each it keeps every account under, and the span each
session's burn is averaged over." and holds `Window ceiling (%)`, from 50 to 100, with the hint "A move or a new session
lands on a member only where its windows stay under this share until they reset, or for the next five hours on a weekly
window. A round runs on a member whose window would pass it.", and in the expert view `Burn average (s)`, from 600 to
7200, with the hint "The time constant of each session's burn average. The projection, the slow-down and the round read
it. A 5-hour window younger than twice it fires no round. A slowed member's holds reach their full length over it.
Editable under the Custom preset in Load shedding." It closes on the "Member utilization" bar, whose hint reads "Each
tick is a member's latest 5-hour or 7-day reading, as on the Quota page. Drag the ceiling's handle, or focus it and use
the arrow keys." The bar is one track of used shares holding both windows' readings, a tick per member window with the
hover `{account}: {window} {used}` and the legend "5-hour reading" and "7-day reading", and the ceiling stands on it in
every view as the "Window ceiling" handle, dragged or moved with the arrow keys, with the field following it.

Load shedding opens on "A member whose window would pass the ceiling before its reset, or within five hours on a weekly
window, runs a round that marks the sessions that cover the excess, and each moves to a sibling with runway at its next
request. With the slow-down on, a round fires when the time the slow-down adds passes the stretch threshold, and removes
only what the slow-down cannot cover." Its first field, in every view, is the `Load shedding` toggle, with the hint
"Off, no round runs and nothing is marked. The ceiling still holds every move and new session, and Migrate when spent
keeps its own switch." With it off no round runs, the group's other fields stay editable, and the spent move keeps its
own toggle. The expert view adds the preset (Balanced, the default, Fast, Conservative or Custom, the first three
filling the cap, the average under Windows and the dwell and leaving the switch as it is, Custom opening the three to an
edit with the hint "Set the cap, the burn average under Windows, and the dwell by hand."), `Shed cap (%)`, from 10 to
100, with the hint "The most of the member's burn one round removes. The round removes what brings the window under the
ceiling, up to this.", `Shed dwell (s)` and Warm sessions, and closes on a "Preview" line, the round's dry run as the
fields stand. `Burn average (s)` sits under Windows and `Migrate when spent` under Failover, which opens on "How long a
request waits out a throttle on its member, how many requests one member serves at once, and whether a spent window
moves the session at once." The Preview replays the trigger from the projections the accounts route serves for each
member and window and from the sessions' served burn, an estimate of what the router reads.

With the switch off the Preview opens on `Load shedding is off: no round runs.` and names the projection alone. It reads
`No member projects over the ceiling on any window: the closest is {label}, {window} {p} {horizon}.` while no member's
projection passes the ceiling, and otherwise `1 member projects over the ceiling: {label}.` or
`{n} members project over the ceiling: {list}.`, followed by
`A round there would mark {k} sessions ({warm} warm), {excess} of the excess, to move to a sibling with runway.`
(`1 session (1 warm)` for one), by `No sibling has runway for them, so nothing would be marked.` where no sibling has
runway for the sessions a round would mark, or by `A round there would mark no session:` with the reason it marks none,
`none is priced.` where the members over the ceiling hold no priced session, `each is its member's last.` where every
priced session is the last on its member, which a round never marks, `each is warm.` where Warm sessions is never and
every priced session is warm, or `none burns.` where the round's target is zero. A member whose 5-hour window is under
the hold is read on its next window in the order, and where its 5-hour projection reads over the ceiling, the line names
the round the hold took away:
`{label}'s 5-hour window is {age} old, under the hold of {hold}, so its round reads the next window.` `{p}` is a whole
percent with a space before the sign (`99 %`), `{horizon}` `at` a clock time on the 5-hour window and `in` a span on a
weekly one (`in 5h`), `{window}` the window's label (`5h`, `7d` or `7-day Fable`), `{excess}` a percent an hour with one
decimal (`9.5 %/h`), `{list}` the members in the Members group's order, and `{age}` and `{hold}` spans to the minute
(`45 min`, `1 h`).

Slow-down, in the expert view, opens on "The slow-down holds each request of a session before it is forwarded, so an
account whose sessions would pass the ceiling before the reset reaches it under the ceiling instead. A session waits its
own cycle times the slow-down, so a fast session waits more than a slow one and each keeps its share." and holds the
`Slow-down` toggle, with the hint "Holds requests on a member whose five-hour window would pass the ceiling. Auxiliary
requests, compactions, the fast lane, probes and token counts are never held.", `Stretch (s)`, from 60 to 18000, with
the hint "The time the slow-down may add to a session before a round moves sessions off the member, with load shedding
on. Under it the slow-down holds alone.", `Hold (s)`, from 1 to 300, with the hint "The longest one request waits. 120 s
leaves 60 s of the client's 180 s first-byte window for the upstream's head.", and a "Preview" line, which reads the
switch and the threshold as the dialog holds them and the slow-down the accounts route serves on each member, so it
shows what the save would do. With the switch off it reads `Slow-down is off: no request is held.`, with it on and no
member slowed `No member is slowed.`, with one `1 member is slowed: {label} at {factor}x ({stretch} of {threshold}).`
followed by `A session there waits its cycle times {wait}, up to {hold}.`, and with more
`{n} members are slowed: {label} at {factor}x ({stretch} of {threshold}), ....` followed by
`A session on {slowest label} waits its cycle times {wait}, up to {hold}.`, where `{factor}` is the member's factor as
the chip prints it, `{stretch}` and `{threshold}` spans (`42 min of 30 min`), `{wait}` one over the slowest member's
factor less one, to one decimal (`1.0` at 0.5), and `{hold}` the hold in seconds (`120 s`). Where the slowest member is
at a factor of 0 the second sentence reads `A session there waits the full {hold}.` with one member slowed and
`A session on {slowest label} waits the full {hold}.` with more. The mechanism is on [Slow-down](routing.md#slow-down).

Overage and plan end holds the Overage policy radio in every view and, in the expert view, `Overage grace (min)`,
`Return to plan`, `Return dwell (s)` and `Wind down (min)`. `Return to plan` is a radio, Always ("at once when cold,
after the dwell when warm"), When cold ("only when the move is free: a compaction, a restart or a lane change") and
Never ("the session stays until the member refuses it"), with the hint "When a session on an API-key member returns to a
subscription member. Under Never it leaves the metered member on expiry, a cooldown or a mark alone."
`Return dwell (s)`, from 0 to 86400, with the hint "Under Always, how long a warm session stays on the API-key member
before it returns. 0 returns it at once.", is enabled under Always. Under When cold or Never it stays editable while its
value is one the dialog refuses (not a whole number from 0 to 86400, which holds the save as typed) and while it holds
the focus, so the keystroke that brings a refused value back in bounds leaves it editable until the focus leaves.
`Wind down (min)` carries the hint "This long before a canceled subscription ends, its account takes new sessions only
as the last resort and cold sessions move off. Inside it the account is no destination for a move. The end is set on the
Accounts page." The rules are on [Moves](routing.md#moves).

The pool routes answer each pool with its warnings on every list, create and save: static checks on settings that work
against each other, none of which blocks a save. The dialog draws the saved pool's warnings above its groups, one line
per warning with its sentence and `Go to {group}.` naming the group of its first field, and a new pool shows none until
it is saved. The card's Warnings fact counts them. The ten checks, by code:

| Code | Holds when | Sentence |
| --- | --- | --- |
| `affinity_under_cache_ttl` | the cache TTL policy is on and adds a marker to a request without one, and `affinity_ttl_seconds` is under the longest TTL the policy sets | "The session affinity ends before the cache TTL the pool injects, so a pin can expire while its cache is warm and the next request can land cold elsewhere." |
| `cap_under_cohort` | `in_flight_cap` is under the most agents of one session among the pool's live pins | `The in-flight cap is under the largest live cohort of {n} agents, so subagents of one session wait for a slot they share.` |
| `cap_under_quarter` | `shed_cap` is under 0.25 | "A shed cap under 25 percent removes at most a quarter of the burn per round, so a window far over the ceiling needs several rounds and dwells to come under it." |
| `ceiling_low` | `window_ceiling` is under 0.8 | "A ceiling under 80 percent leaves a fifth of every window unused and moves sessions that would have finished the window warm." |
| `ceiling_without_margin` | `window_ceiling` is 1.0 | "A ceiling at 100 percent leaves no margin for a stale reading or a large turn, so a session can reach the wall before the round or the gate reads it." |
| `hold_over_window` | `slow_down_hold_seconds` is over 150 | "A hold over 150 seconds leaves under 30 seconds of the client's first-byte window for the upstream's head, so a slowed request can time out at the client and be sent again." |
| `return_at_once` | `return_to_plan` is `always` and `return_to_plan_dwell_seconds` is 0 | "A return dwell of zero returns every warm session on its first request at the metered member, so the member serves one turn per session and the move's rewrite is paid each time." |
| `shedding_off` | `load_shedding` is off and `slow_down` is off | "With load shedding off no round marks sessions on a member whose window would pass the ceiling, so a session stays until its window is spent or its account refuses it." |
| `spent_moves_off` | `migrate_when_spent` is off | "With spent moves off a session stays on a spent account until the upstream refuses it, so the destination gate never runs for that move." |
| `warm_never` | `warm_sessions` is `never` | "With warm sessions never a round stops after the cold sessions, so the excess is covered only while cold sessions carry it." |

Keys holds "Create key" and the pool's keys as Key, Prefix and Status with a menu of "Move to…" and "Revoke". "Move
to…" opens `Move key {name}` ("A key reaches only its own pool's members, so moving it ends its current session
pins.") with the destination Pool, and "Revoke" asks `Revoke {name}`: `Requests with {name} are rejected immediately.
The key stays in this pool and its history stays in the ledger.` A key created from here opens "Key created", the dialog
the Keys section describes. Guardrails lists the pool's guardrails in the order they run, each with its name, `off` on a
disabled one, its mode as a badge (enforce in the warning tone, observe muted), move up, move down and remove, then
"Attach guardrail", a box over every guardrail of the registry with the pool's own ones checked, which stays open while
the operator picks: a check adds its guardrail to the end of the list at once and an uncheck removes it, "Select all"
adds every guardrail the box lists and its uncheck removes them all, and Escape or a click outside closes the box, the
save writing the list as it stands. While the registry holds no guardrail the control is disabled with "No other
guardrail is left to attach" on hover. Under the list the hint reads "Top to bottom. A pool's guardrails run before a
key's own." and the link "Manage guardrails" opens the [Guardrails](#guardrails) view. On create the group reads
"Guardrails can be attached once the pool is saved." and no control. Keepalive holds the switch "Keep prompt caches
warm", the three ceilings and the return rate: a ceiling left off reads the break-even it resolves to, `Break-even:
{range}`, a parked or horizon ceiling switched on past the break-even reads `Capped at the break-even, {range}`, and the
idle window reads its cost as a share of one re-write priced from the set value, `Costs {pct} of one re-write`, with `at
the saved {time}` for an unsaved edit. While the idle field reads 0 the cost line adds the window in effect, `capped at
the break-even, {time}`, and while the field reads more than the break-even's low end the note `Above the break-even,
{range}` follows on a line of its own with the break-even's figures, since the window runs as set. Return rate by idle
age lists the pool's ten buckets with the windows that returned out of those that reached it and a bar of the rate, or
"No idle windows recorded yet". The ceilings are explained under [The four fields](keepalive.md#the-four-fields) and the
buckets under [The return rate by idle age](keepalive.md#the-return-rate-by-idle-age).

## Keys

![The Keys view](assets/keys-light.png#only-light)
![The Keys view](assets/keys-dark.png#only-dark)

The view opens on its bar: "Search keys", Add filter with the Pool and Status filters, "Create key", and at the right
edge the split Refresh button and the count of the keys the table lists, `{n} keys`. Status offers All statuses, Active,
Disabled, Expired or Revoked. Pool takes any number of pools, each as a chip of its own, such as "pool: jobs" and "pool:
all", and a pool the address names that the router does not list keeps its chip until the pools load. The search and the
filters live in the page's address, so the pool card's key count link lands on its pool's keys. Auto reload starts at 5
s and waits while a dialog or a row's menu is open. The tiles Keys, Active, Lifetime requests and Lifetime cost follow,
and the card "Lifetime cost by key" ("Relative spend across all keys"), a bar per key. Lifetime cost, each bar's figure
and the cost line under a key's "Usage, 7d" read in the top bar's unit: under Native the tile stacks the subscription
traffic's 20x 7-day equivalent over the billed traffic's dollars, and a bar's figure and a cost line read a key's
equivalent beside its billed dollars, a side with no traffic left out, the tile, the bar's figure and the cost line each
reading the subscription side as a dash with the Dashboard's hover where a request priced nothing on the 7-day window,
and its bar draws no fill and sorts last, and under Window shares a dash, since a key spans its pool's accounts. The
columns are Name, Prefix, Pool, Models, "Usage, 7d", Expiry and Status, and each row ends in its menu. Under the name a
row notes the capture setting, `bodies captured, {h}h` or `bodies not captured`, with "inherited" where the key follows
its pool and "keepalive off" for a key opted out of cache warming. Models reads `{n} rules`, with "(replace)" where they
replace the pool's, or "Pool rules" or "No rules". Expiry reads the date or "Never". Status carries the key's state with
`used {ago}` and, after a rotation, `{n} sessions still on the previous secret`. The menu offers Edit, Rotate (`Rotate
{name}`), Revoke (`Revoke {name}`: "Requests with this key are rejected immediately. Its history stays in the ledger.")
and Delete (`Delete {name}`: "Removes the key and detaches its request history from the key name. This cannot be
undone."). Before the first key the table reads "No keys yet" with "Create key". What a key's settings mean is on
[Keys](routing.md#keys).

"Create key" and Rotate end on the dialog "Key created" or "Key rotated", which shows the secret once with a copy
button under "Copy the secret now. The router stores only a hash, so it cannot be shown again." and "Set it as
`CLAUDE_CODE_OAUTH_TOKEN` on the client. The Settings page has the full snippet.", and closes on "Done".

The key dialog, "Create key" or `Edit {name}`, is grouped like the pool dialog, with the rail "Key settings" and the
groups Key (Name, Pool, Notes), Access (Enabled on an existing key, Expiry, and Server-side auto mode with Inherit, On
or Off), Models (Model rules, with Pool rules as Extend pool rules or Replace pool rules, and List [1m] twins in the
models listing), Guardrails (the pool's guardrails first in gray, each marked "inherited", then the key's own in their
order with move up, move down, remove and "Attach guardrail", a box over every guardrail of the registry the pool does
not attach with the key's own ones checked, which stays open while the operator picks: a check adds its guardrail to the
end of the list at once and an uncheck removes it, "Select all" adds every guardrail the box lists and its uncheck
removes them all, and Escape or a click outside closes the box, the save writing the list as it stands, the control
disabled with "No other guardrail is left to attach" on hover while the registry holds nothing beyond the pool's, under
the hint "The pool's guardrails run first and cannot be dropped here. This key's own run after them." and the link
"Manage guardrails", reading "Guardrails can be attached once the key is saved." on create), Sessions (Cache warming
opt-out) and Capture (Capture request and response bodies as Inherit from pool, an override in hours or off for this
key). Server-side auto mode, Pool rules and List [1m] twins show in the expert view. While the key captures, the Capture
group warns what the bodies hold, the warning [Keys](routing.md#keys) quotes.

A saved key's dialog shows its Effective list under the Models group: the list the key is judged by, top to bottom, with
the level each entry comes from and, where accounts cool for a model the entry names, a cooling count whose hover names
them.

## Guardrails

![The Guardrails view: a preset's guardrail observing beside an enforcing one](assets/guardrails-light.png#only-light)
![The Guardrails view: a preset's guardrail observing beside an enforcing one](assets/guardrails-dark.png#only-dark)

The view opens on its bar: "Create guardrail", whose menu offers "Blank guardrail" and, under the heading "From a
preset", one row per preset the build ships, three rows in the order of their ids (Away recap, Prompt suggestion,
Subagent status summary), then at the right edge the split Refresh button and the count `{n} guardrails` (`1 guardrail`
for one). Under it one sentence: "A guardrail checks each request against its conditions before any account is selected.
Observe records the matches, enforce refuses them with a 403." Auto reload starts at 5 s and waits while a dialog or a
row's menu is open. The columns are Name (the description muted beneath), Mode (a badge `observe` or `enforce`), Enabled
(`on`, or `off` muted), "Applies to" (`{n} pools, {m} keys`, `1 pool, 0 keys` in the singular, the names on hover,
`nowhere` muted when none), "Matches, 24h" (`{observed} observed` over `{blocked} blocked` from the last day's counts, a
dash while unread) and Preset (the preset's name with `v{n}`, a `customized` badge where the conditions left the stamped
version, an `update available` badge where the build ships a newer one, a dash without a stamp), and each row ends in
its menu: Edit, Duplicate, Enable or Disable, "Switch to enforce" or "Switch to observe", and Delete. Duplicate opens
the dialog in create mode with the source's fields, the name `{name} copy`, no attachment and the preset stamp kept, so
the copy does not run twice on the source's keys and still says where it came from. Delete asks `Delete guardrail
{name}`: "Removes the guardrail from every pool and key it is attached to. Requests it matched keep its id in the log."
Before the first guardrail the table reads "No guardrails yet" with "Create guardrail". What a guardrail is, the presets
and the conditions are on [Guardrails](guardrails.md).

The guardrail dialog, "Create guardrail" or `Edit guardrail {name}`, is grouped like the pool dialog, with the rail
"Guardrail settings", the groups Identity, Conditions, "Applies to" and Test, the status line "Every group saves
together." or `Save is held: {reason}` with "Reload" on a stale save, and Cancel beside "Create guardrail" or "Save
guardrail". No group is expert-only. Identity holds Name (the hint "The guardrail's id is made from this name when the
guardrail is created." with the derived id on a create), Description, Enabled (the hint "A disabled guardrail stays
attached and is skipped.") and Mode as Observe or Enforce (the hint "Observe records each match on the request. Enforce
refuses a matching request with a 403 before any account is selected."). A guardrail created from a preset copies the
preset's name, mode and conditions and leaves Description empty for your own words, and its Identity group opens on the
line `From the preset {name} v{n}` with the preset's explanation muted under it and the badges `customized` and `update
available` where they apply. Where the build ships a newer version, a notice reads "This build ships version {m} of the
preset." with the button "Update to the current preset", which replaces the conditions with the current version's, after
which the notice reads "Saving takes version {m}." until the save.

Conditions holds the editor, one row per condition in the written order: a Subject select (Agent type, Body, Client,
Header, Kind, Message, Model, Request class, User agent), the subject's own cell ("Header name" with the placeholder
`x-claude-code-agent-type`, "Path" with `tools[*].name`, or a Selector of any message, any assistant message, any user
message, first user message, last message, last assistant message and last user message), an Operator select with the
subject's operators (is absent, equals, is greater than, is less than, matches, is present), and a Value cell for
equals, is greater than, is less than and matches (`^Describe` as the placeholder under matches), which for Kind is a
select of Count tokens or Inference. A subject change resets the operator to the subject's first and clears the cells it
lacks. Each row has move up, move down and remove, "Add condition" appends a row up to 16 with the count `{n} of 16`,
and the hint reads "Every condition must hold. The router checks the cheap conditions first, whatever their order here,
and the message text last." The dialog holds the save on a row it cannot read (`Condition {n}: needs a header name`,
`needs a path`, `needs a selector`, `needs a value`, `{value} is not a number`, `the pattern is longer than 2048
characters`, or `the path ...` with the path grammar's own sentence), on an empty name ("The guardrail needs a name."),
a taken id (`Another guardrail already has the id {id}.`), a reserved one (`The id {id} is reserved.` for `presets` and
`test`), a name or description past 256 bytes and a count outside 1 to 16 ("A guardrail holds 1 to 16 conditions."). A
pattern the router refuses on save lands under its row as `Condition {n}: pattern does not compile: {message}`.

"Applies to" holds two pickers, Pools and Keys, each a button reading `{n} pools` or "No pools" (`{n} keys` or "No
keys", a key labeled `{label} ({pool})`) that opens a searchable list with "Select all", under the hint "A pool's
guardrails cover every key of the pool. A guardrail attached here joins the end of each pool's and key's order, which
their own dialogs arrange." Test holds "Sample from" as Pool or Key with the pool or key select, Sample as 50, 200 or
500, and "Run test", disabled while the conditions fail the dialog's own rules. The result reads `{matched} matched,
{unmatched} unmatched, {indeterminate} indeterminate of {sampled} sampled` over a table of Time, Request (linked to the
Requests view) and Verdict (`matched`, `failed at condition {n}` or `condition {n} needs the live header`), the matched
rows first. An empty sample reads `No captured bodies for {name} in the last {h} hours.`, or where the key's effective
capture or the pool's reads off, "Body capture is off, so there is nothing to test against. Turn it on in the key's
Capture group." with the link "The pool's keys" to the Keys view filtered to the pool. The toasts read "Guardrail
created", "Guardrail saved" and "Guardrail deleted". What the test evaluates and what a blocked client receives are on
[Testing a guardrail](guardrails.md#testing-a-guardrail).

## Requests

![The Requests view: the table with its client column](assets/requests-light.png#only-light)
![The Requests view: the table with its client column](assets/requests-dark.png#only-dark)

The filter bar holds one row: the range, the search, Add filter, the Columns button, and at the right edge the split
Refresh button and the count of rows shown ("25 shown, more available"). The range offers Last hour, Last 6 hours, Last
24 hours (the default), Last 7 days, Last 30 days or Custom range with From and To, drawn as the pills 1h, 6h, 24h, 7d,
30d and Custom, the pressed pill naming its range and its end on hover. Custom opens From and To in a popover on the
window the table shows, where Apply stays disabled under "End must be after start." while the pair is wrong, an end past
now reads as now, and the pill prints an applied span ("13:00 to 14:30", the date before each time across days). The
search, "Search id, session, key, account, model, error, message id", matches the request id, the session, the key, the
model, the error code, and an attempt's account, upstream request id or upstream message id, literally (`_` and `%` are
characters), and applies 400 milliseconds after the last keystroke, on Enter, or when the field loses focus. Add filter
opens a "Filter by" field over the filters in four groups: Routing (Account, Key, Model, Pool), Outcome (Cache miss
reason, Guardrail, Held only, Outcome, Refusal category, Status, Stop reason), Request (Agent type, Classifier, Client,
Effort, Kind, Request class, Speed, Turn origin, User agent) and Subject (Request, Session). Choosing one opens its
values: a list with a Select all row that applies on each click (checking it checks every option and unchecking it
clears every check), a pattern field for Client and User agent (`claude-cli/2.1.*`, `*sdk-cli*`: `*` for any run of
characters, `?` for one, a backslash quoting either, and a value with no wildcard matching in full), an id field for
Request and Session, which match the id exactly, or the Kind choices (Any kind, Inference, Count tokens, Probe), while
Held only applies at once. Guardrail lists the registry's guardrails by name, then the ids of deleted guardrails the
loaded rows still carry as `{id} (deleted)`, then `none` for a request no guardrail matched
([Guardrails](guardrails.md)). Stop reason lists the stop reasons the console names (`end_turn`, `max_tokens`,
`model_context_window_exceeded`, `pause_turn`, `refusal`, `stop_sequence`, `tool_use`) and any other the loaded rows
carry, Refusal category the categories it names (`cyber`) and any other the loaded rows carry, each list ending on
`none` for a request whose serving attempt carries no value or that no attempt served. A pattern or an id applies on
Enter or Apply. Each filter in force stands as a chip on a line under the bar, such as "account: primary, nightly" or
"held only": a click on the chip opens its values again, its x removes it, the chips past five fold behind "+N more",
and from two chips on "Clear all" removes them and keeps the range, the search and the page size. The account filter
keeps the requests whose serving attempt ran on the account, so a request that failed over lists under the account that
answered it. Every filter, the range and the page size live in the page's address, so a reload or a shared link keeps
them.

The table offers thirteen columns, Time, Key, Pool, Account, Model, Client, Kind, Transport, Status, Attempts, TTFB,
Tokens and Cost, with Kind, Pool and Transport hidden until the Columns control turns them on, and shows Time, Model,
Status and Attempts alone while the drawer is open. Every column but Time is as wide as its widest value, and Time takes
the card's free width: the request id under the time is cut by the room with an ellipsis, whole on hover, and reads
whole where the card holds it, on a wide screen or beside the drawer. Time keeps room for the widest time the clock
draws, so a time never cuts, and under the 12-hour clock, whose times run wider, the default columns can run past the
card at 1280 px and scroll inside it, as any set of columns wider than the card does. A row older than a day prints its
date before the time, so a 7-day or 30-day range reads without the hover. The model column reads the family and version
with the whole id on hover, and a dash for a request the router refused before it resolved a model. The client column
reads the class the router derived from the user agent (`claude-code/<entrypoint>` or `other`), whole on hover, with the
agent type beneath it on a subagent's turn. The drawer's Client row names a session that branched from another
conversation, `fork of {base} at 3`, with the base's whole id on hover. Status reads the code, a warning badge with the
refusal's category where the upstream refused the answer (`none` where it gave no category, its hover "The upstream
refused this answer. Open the request for the stop reason and the recommended model.", the request having succeeded),
and the outcome word beside it where the request did not succeed (`canceled`, `exhausted`, `failed`, `incomplete` or
`rejected`). One row is one client request however many attempts it took: Attempts reads `2 attempts` with the trail of
account and status on hover, nothing for a request of one attempt, and a `waited 42 s` badge for a request that waited
through a hold, for a slot while every account was at its cap, for a reset, for the slow-down or for the rewrite in
flight, whose hover names each wait with its reason in the row's order and the account it waited on, `waited for the
slow-down 42 s, for a slot 3 s on {account}` or `waited for the rewrite in flight 2 s on {account}`, or each with its
own account where the waits span two, `waited for the slow-down 42 s on {account}, for a slot 3 s on {other}` (a row
from before the reasons reads `waited 1 s on {account}`, and a wait under a second reads `1 s`). Cost reads in the top
bar's unit, its header's hover naming what each kind of request reads under the position. Under Native a request a
subscription window paid reads its 20x 5-hour equivalent over its 20x 7-day one (`1.47% 20x 5h` over `0.244% 20x 7d`),
with every window, the model week among them, and the dollar equivalent on hover. Under Window shares it reads its
account's own window shares, each with its window's short word (`0.12% 7-day Fable` over `0.42% 5h`, the full labels on
hover), under All in dollars and Dollar equivalent its dollar equivalent over `equivalent` with every 20x equivalent on
hover, and under Billed dollars a dash with the hover "Nothing billed: a subscription request". An API-key, fast mode,
overage or usage-based request reads dollars under every position but Dollar equivalent, where it reads a dash with the
hover "Billed in dollars, no subscription equivalent". A figure an estimate priced leads with `~`, its hover adding
"Estimated at the model tier's latest weighted model", and a dash carries no mark. A client classifier call on a
subscription account reads `0%` under Native and Window shares, its 20x equivalents and its window shares zero, with the
hover "Free on the plan: a client classifier call takes no window share" ahead of the estimate, its dollar equivalent
over `equivalent` under All in dollars and Dollar equivalent with the same hover ahead of its zero 20x equivalents, and
a dash under Billed dollars, while on an overage or API-key account it reads its dollars as any billed request does. The
table pages by "Rows" (10, 25, 50 or 100, 25 by default) with First, Previous and Next, holds the requests up to the
moment you opened it so paging does not shift, and the split Refresh button's body brings it up to now. An empty table
reads "No requests match" with "Widen the range or clear a filter." and "Reset filters", which clears the filters, the
search and the range. What each figure means is on [Statistics](statistics.md).

Auto reload is off when the view opens. The split Refresh button's dropdown sets it to Off, 5 s, 10 s, 30 s or 1 min,
and the choice stays with the open page, out of its address. At each interval it reads the list again, a preset's range
ending at the router's clock and a custom range keeping its own end, where a request received inside it and written
after a read draws on the next read, takes every row that arrived since the read before, up to eight pages, and lands a
new row at the top in the accent tint until the next read. A long request can finish below rows already shown: it lands
between rows already shown, in the same tint, on the next auto reload. On a custom range whose rows fit on the page
shown, a request that lands below them after a read draws on the next read in the accent tint, and where the range runs
on past the page a late row below it stays for Next. Past eight pages a line reads "More rows arrived than auto reload
could fetch. Reload to read them.", and Next reads on from the last row the poll fetched, under a preset to the router's
clock. A poll whose newest page stood in for a list read that failed gives way to the list once it answers. A filter
change keeps the interval. The poll waits while the tab is hidden, while the drawer is open, on an older page until you
go back to the first, and while a menu or popover under the bar is open, with the interval kept and the cause named in
the dropdown's title. When the wait ends a full interval or more after its last answer, it reads at once. When the tab
comes back to a read at once, the rows that read brought keep the tint through the read after it. A custom range whose
end lay more than a minute in the past when the page took it has nothing new to read, so the dropdown reads Off and
stays disabled, while a link whose range ends at the moment it was made reads on, each read refreshing the rows inside
it.

Selecting a row opens the "Request details" drawer, with the request's id and time, a "Routing log" link that opens the
[routing log](#routing-log) on the request a quarter hour either side, and these sections:

- Outcome: Status, Outcome, Account, Model, Canonical model when it differs from the model the client spelled, TTFB,
  Duration, Error (the error code, which a stream cut after its head follows with the relay's words for the cut, such
  as `stream_undecodable: error decoding response body`), Client drift when set, Client contract when set (the item
  the refusal named), then the serving attempt's Answer (the stop reason, the speed, the geography, the service tier)
  and Cache (the miss reason, `{read} read, {written} written`, where the prompt diverged), and a Diagnostics row
  naming who put the cache diagnostics member on the body: "sent by the client", "added by the router" or none.
- Guardrails: one row per guardrail that matched, in evaluation order, the guardrail's name (or `{id} (deleted)` for one
  the registry no longer holds) with a `blocked` badge in the critical tone or an `observed` badge muted, shown where
  the request carries a match ([Guardrails](guardrails.md)).
- Phases: a waterfall over Received, Selected, Written, Head, First content, Stream end and Finished, a phase the
  ledger holds no instant for reading "not measured".
- Usage: Input (uncached), Cache read, "Cache write, 5m", "Cache write, 1h", "Output, thinking", "Output, plain", Server
  tool calls and Estimated cost in the top bar's unit: a subscription request's 20x equivalents under Native or its
  account's window shares under Window shares, one chip per window with the dollar equivalent after them, its dollar
  equivalent with `equivalent` under All in dollars and Dollar equivalent and a dash under Billed dollars, and a billed
  request's dollars, or a dash under Dollar equivalent. Its hover names why a dollar figure or a dash stands, as the
  Cost cell's hover does, the serving account's tier (`Account tier {tier}`) and the price list and cost model in force
  when the request arrived (`Priced at the API prices of {date} and the cost model of {date}`). A client classifier
  call's row reads its zero figures under Native and Window shares and its dollar equivalent under All in dollars and
  Dollar equivalent, with the hover "Free on the plan: a client classifier call takes no window share".
- Request: Session, Agent, Client with the request class badge and the fork line where the session branched from another
  conversation, User agent (such as `claude-cli/2.1.310 (external, cli)`), Key, Pool, Kind, and one `Attempt {n}` row
  per attempt with the upstream's request id and, on the answer that served, its message id.
- Request shape: Effort (the level the upstream applies), Thinking, Max tokens, "Tools, system blocks, messages",
  Context edits, Temperature, "Speed, service tier", Auto mode and Classifier (`client` for the client's own auto-mode
  classifier call, `none` otherwise), "not sent" where absent.
- Client hints: Agent type, Compaction, Context compacted, Turn origin and Tool time before this turn.
- Captured bodies: the request and the response, each with Copy all and "Open wide", which draws a body of any size
  pretty-printed in a window of its lines and a streamed answer as its events, or "Not captured for this key. Enable
  body capture on the key to record the exchange." or "Bodies for this request have aged out of retention." A find field
  under each body's head counts the lines that hold its term, whatever the letter case, reading "3 of 12" or "No match".
  Enter, Previous hit or Next hit opens the wide window on the current hit, where Enter and Shift+Enter walk the hits
  and wrap at either end, every occurrence on the drawn rows is marked and the current hit's row is tinted. Escape or
  Clear find empties the field. What capture stores and for how long is on [Body capture](statistics.md#body-capture).

The "Widen to" line under the request's time links this minute, this session and this account, each opening the list
in place of the drawer over the clock minute that holds the request, its session an hour either side, or its account a
quarter hour either side, with every other filter dropped, the page size kept and the link left out where the request
has no session or no account.

A request the ledger does not hold reads "Request not found" with "It may have aged out of the ledger."

## Analytics

![The Analytics view: the Traffic tab](assets/analytics-light.png#only-light)
![The Analytics view: the Traffic tab](assets/analytics-dark.png#only-dark)

The sticky filter bar holds one row: the range, Add filter, "Group by" and, while the page groups by user agent, the
"Groups (N)" button, then at the right edge the split Refresh button. It has no search and no count. The range offers
Last hour, Last 6 hours, Last 24 hours (the default), Last 7 days, Last 30 days or Custom range with From and To ("End
must be after start." while the pair is wrong), the pills and the popover as on the Requests view. Add filter opens a
"Filter by" field over the filters in three groups: Routing (Account, Key, Model, Pool), Request (Client, Include
probes, User agent) and Subject (Session). Account, Key, Model and Pool list the values the range holds, Client and User
agent take a pattern in the Requests view's grammar, applied on Enter or Apply, Include probes applies at once, and
Session takes a session id, typed or pasted, which matches the session exactly. Each filter in force stands as a chip on
a line under the bar, as on the Requests view, such as "client: claude-code*" or "include probes". The session chip
prints the id's first eight characters, and its hover names the whole id with the key lifetime a link from the Sessions
view carries ("under the key laptop@3"), which has no chip of its own and leaves when the session changes. "Group by"
offers Account, Classifier, Client, Key, Effort, Kind, Model, Pool, Request class, Speed, User agent or "Session (Burn
only)", the last the Burn section's own grouping. While it reads User agent, "Groups (N)" opens the "User agent groups"
box with one pattern per line, N the patterns it holds, where each pattern names a group and every other agent groups
under `other` (`claude-cli/2.1.*` and `claude-cli/2.0.*` as two groups). The user agent grouping reads the agent's first
product token, `claude-cli/2.1.310` for `claude-cli/2.1.310 (external, cli)`. The classifier grouping lists the requests
with no classifier call under `none`, as the Requests view's Classifier filter names them.

Auto reload is at 5 s when the view opens, set on the split Refresh button's dropdown as on the Requests view and kept
out of the page's address. At each interval it reads the account list again and, once a new bucket of the range has
begun, moves the window's end to now and reads every chart again, so a page left open shows the hour it is in. It waits
while a menu or popover under the bar or the Cost reference dialog is open, or the tab is hidden. Off, the window's end
holds where it was read and the dropdown's title adds "the window ends {time}" until a filter, the range or Refresh
moves it. The split Refresh button's body moves the end to now and reads every chart again. A custom range holds its own
end either way, and one whose end lay more than a minute in the past when the page took it reads Off in the dropdown,
disabled, while a link whose range ends at the moment it was made reads on.

Filtered to one session the view sums the request log instead of the hourly rollups and says so, over the first 10,000
requests of the range, and a session with more reads "Partial: the first 10,000 requests of the range" on a line under
the bar. On a custom range the view walks the session's pages once, and each auto reload re-reads the first page alone,
with the later pages standing as loaded. Under that filter the Cost split reads "Cost split is not available for a
session view.", the Write cost vs read cost legend reads a dash for each cost, and Cache warming reads "Cache warming
figures are not available for a session view." with "not in the request log" under its probe tiles, since a request row
carries one cost total and no probe or kept-warm fact. Grouped by key, a deleted key's group reads
`<id> (deleted, created <time>)`, its creation instant in the viewer's zone. The range, the grouping, every filter, the
client groups and the probes switch live in the page's address. A link that carries `unit` sets the top bar's Unit
switch to that position and leaves the address, so a shared link opens on its unit. The section bar under it shows one
section at a time, Traffic, Burn, Latency, Cache, Cost, Cache warming or Accounts, and a range with nothing reads "No
traffic in this range." What each figure means is on [Statistics](statistics.md).

Traffic reads `{n} requests, {pct} errors` and draws Requests by outcome (OK, Rate limited, Failed, Incomplete and
Exhausted), Errors by code ("Failed and incomplete responses in range, the refused requests left out"), Refusals by code
("Requests the router refused before any account was asked, not in the counts", or "No request was refused in this
range." where none was), Refusals by category ("Answers the upstream refused in range, by category, each a request that
succeeded", one bar per category linking the Requests view on the stop reason `refusal` and the category over the range
to the millisecond, `none` for a refusal with no category, or "No answer was refused in this range." where none was, and
under a session, client or user agent filter every session's, client's and user agent's, which its subtitle then names,
its links carrying none of the three), Failovers, Migration rewrites with its moves (one bar per move reason, stacked by
the destination's fit, `fits`, `rewrite only`, `least bad` or `unknown`, each segment titled `{fit} {n}` and the bar
labeled with the reason's total, and under them "Held by the slow-down", a bar per bucket of the time the slow-down held
requests in the warning tone, its hover `12 requests held 14 min in all`, `no request held` on a bucket that held none,
and its legend line the range's sum, or `none in range` with no chart), Requests by speed, Effort levels, then at full
width the card "Guardrails" ("Matches by guardrail in range, every key") with one row per guardrail that matched in the
range as Guardrail, Observed, Blocked and "Blocked bytes", reading the range alone and no other filter, a guardrail the
registry no longer holds as `{id} (deleted)` and "No guardrail matched in this range." where none did
([Guardrails](guardrails.md)), and the table Traffic by the chosen group with Requests, OK, Rate limited, Failed,
Incomplete, Exhausted, Rejected, Refused, Error rate, Failovers, Search and Fetch, the Requests and Error rate headers
reading "The refused requests are left out, see the Refused column" on hover and the Refused header the Dashboard's
Refused tile hover. Under the model grouping a request refused before it resolved a model groups under "no model", which
the Cache by model table leaves out, and a group with no request, as that one, reads a muted dash in its Error rate
cell, as the head's `{pct}` reads a dash where the range rated no request. A request the router refused before any
account was asked is counted in its group's Refused column and in the "Refusals by code" card, a guardrail's block on
the Guardrails card's Blocked and "Blocked bytes" as well, and never in the request count, the outcome columns, the
error rate, the cache figures or the latency.

Burn follows the page's grouping, and "Session (Burn only)" groups it by session while every other section keeps the key
grouping, since no rollup carries a session. Of the filters, the Accounts filter and the user agent groups alone apply
to it, which its head says while another filter is set. Plan per hour draws three cards, "5-hour per hour", "7-day per
hour" and "Model week per hour", the last while a group's traffic ran on a model with a week of its own, each with one
line per group, named as the breakdown names it, the requests with no classifier call listed as `none` under the
classifier grouping. Under Window shares a line is the share of a subscription account's own window, which reads under
the account grouping or with one account named in the Accounts filter, and under every other position it is the group's
subscription traffic per hour in 20x equivalents of the window, the legend's figure naming the window in full on hover.
Under those positions a bucket whose traffic ran on no model with a week of its own reads zero on the Model week card,
as an idle bucket reads zero on the other two, and one the cost model could not price stays a gap. Under Window shares
such a bucket stays a gap on the Model week card, since the shares cannot tell it from one the cost model could not
price. The 5-hour card shades the peak band's hours behind its lines, and each card's note carries a "Cost reference"
link that opens the dialog on Tiers and peak band. A hollow dot marks a bucket whose model has no measured weight and
reads the latest weighed model of its model tier, with an "estimated" entry in the legend and a `~` on the legend's and
the tooltip's figures. A bucket whose traffic ran on a model of no known tier, or on a tier the cost model has no
capacity for on that window, prices no share there and draws no point, under a "model unlisted" entry, and under Window
shares an account whose tier the cost model does not size reads "tier unknown". Spend per minute charts the dollars
alone, a subscription account's among them for its fast-mode and overage answers. With the Model week card drawn, the
four cards sit two by two in the window widths where three would otherwise share a row with the fourth alone (1344 px to
1701 px), and without it the three cards share one row from 1344 px, an odd last card spanning its row in the narrower
widths alone. The section closes on Member budgets, Pool, Account, Spent, Budget and Period ends, one row per budget a
member carries, a member at its budget marked and one the ledger has not reported on reading "not reported".

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
Cache by model dashes under the same note while no account is named. A null diagnostics member on a request that named a
message is recorded as "No divergence", and the per-group table lists Pending after Not requested. Pending: the
upstream's comparison was still running when the answer went out. Every count in Misses by reason but the expired column
links to the Requests view on those requests with the page's filters carried over, and a count the Requests view cannot
list exactly stays plain with the reason on hover.

Cost reads in the top bar's unit. Under Native the tiles read Cost, the subscription requests' 20x 7-day equivalent
over the billed requests' dollars, a side with no requests left out, and Cost per request and Cost per 1k output
tokens the same two ways. Cost by the chosen group stacks the equivalent per bucket with "Billed by {group}" beside it
in dollars, the Cost split draws a ring per side by token kind, the subscription requests' with its 7-day equivalent in
the center (each token kind's figure names the window in full on hover) and the billed requests' in dollars, and the
table reads Requests, Cost, 20x 5-hour, the model week's 20x column, Per request and Per 1k output, its Cost the 20x
7-day equivalent, and Cost, Per request and Per 1k output each with the billed dollars on a line under the equivalent. A
subscription side the route could not price on the 7-day window, as when a request ran on a model of no known tier,
reads a dash, its bucket on the Cost by chart a gap, and the Cost split draws no ring for it. The Cost tile's dash
carries the Dashboard's hover, and the Cost by chart names the groups it draws that read unpriced on the 7-day window on
a line under its subtitle, `{group} unpriced in {m} buckets, drawn as gaps` for one group (`research unpriced in 1
bucket, drawn as a gap`), the groups' labels joined by middle dots for up to three, and `{n} groups unpriced in {m}
buckets, drawn as gaps` past three. Under All in dollars, Billed dollars and Dollar equivalent Cost draws the tiles
Estimated cost, Billed dollars or Dollar equivalent, Cost per request and Cost per 1k output tokens, then Cost by the
chosen group as a chart and a table (Requests, Cost, Input, Cache read, Cache write, Output, Per request, Per 1k output)
and the Cost split as one ring in dollars (Input, Cache read, Cache write, Output). Under Window shares, which reads
under the account grouping or with one account named in the Accounts filter, the tiles read Window share, Per request
and Per 1k output, the chart one line per group, the table Plan requests, 5-hour, 7-day, the model week, Per request and
Per 1k output, and the Cost split the whole in dollars. Every position draws Fast mode spend, and an estimated figure
carries the `~` and the estimate's hover. The prices behind the dollars are on [Pricing](statistics.md#pricing), and the
cost model behind the shares and the equivalents on [Plan meter](statistics.md#plan-meter).

![The Analytics view: the Cache warming tab](assets/analytics-cache-warming-light.png#only-light)
![The Analytics view: the Cache warming tab](assets/analytics-cache-warming-dark.png#only-dark)

Cache warming draws the tiles Probe cost, Saved by probes, Net saving and Kept warm, whose note splits the tokens read
warm between five minutes and an hour, then Probes by outcome (OK, Migrated, Held, Skipped, Failed), Wasted probes
(Wasted probes, Wasted cost, Share of probe cost), Saved by probes, Kept-warm requests, Kept-warm tokens and Cache
warming by pool (Probes OK, Held, Skipped, Failed, Kept-warm requests, Kept-warm tokens, Probe cost, Saved). Saved by
probes is what the kept-warm requests saved, the cache write each avoided at its TTL's rate less the read it paid. Net
saving is that saving less the probe cost, in the critical ink while the probes cost more than they saved, which under
Native the plan side alone decides. A held or skipped probe never reached the upstream and costs nothing, so a range
whose probes were all held or skipped reads a zero Probe cost under Native and the three dollar positions, a dash under
Window shares, and its whole saving as its Net saving. A Saved by probes chart draws the gross saving and the probe cost
per bucket with the net on hover. The by-pool table's Saved column reads each pool's saving beside its Probe cost. Probe
cost, Saved by probes, Net saving, Wasted cost, Share of probe cost and the by-pool Probe cost and Saved read in the top
bar's unit: under Native the plan side's 20x 7-day equivalent over the billed side's dollars, under the three dollar
positions the position's figures in dollars, and under Window shares the named account's share of its 7-day window, a
dash under "Shares of one account's windows alone: name one account in the Accounts filter." while none is named. The
chart draws dollars under the three dollar positions, the 20x 7-day equivalent under Window shares and under Native,
with the billed dollars on hover, and the billed dollars alone under Native where the range holds no plan side. Under
Native the `~` on the Probe cost tile, the Wasted probes tiles and the by-pool Probe cost marks a plan probe priced
through its model tier's fallback weight, which a request priced so beside it never marks. What a probe is and when it
counts as wasted is on [Keepalive](keepalive.md).

Accounts draws Peak 5-hour utilization, then Window usage and Pace under a "Window" switch whose positions are the
windows the range sampled, 5-hour, 7-day, 7-day Fable and "7-day, extra usage". Pace reads "The lead over pace per
bucket: above zero ahead, below behind, from the quota samples, before the router's burn projection", each account's
hover adding its Pace score, the remaining share over the hours from the sample's instant to the window's reset. The
table Accounts in range lists Peak 5h, Peak 7d, "Peak 7d, extra usage", Time in overage, Time in cooldown, Pins
created, Migrated in and Migrated out. The section keeps reading the rollups under a session filter, since they carry
no session, and its subtitle says so. The card "Unused allowance by account" follows Accounts in range, one row per
subscription account with a weekly reset in the last four weeks under the Accounts filter whatever the range, with
Resets, each weekly window's last and four weeks' figures in the top bar's unit, sorted most left first on the four
weeks' 7-day figure, a row whose latest reset is a term's end reading "term ended {date}" under its account, and two
fleet figures over the rows: under Window shares the mean share per reset with the resets and accounts counted, under
every other position the priced accounts' equivalents added with the unpriced accounts named.

## Sessions

![The Sessions view: the tiles and the table with a fork badge](assets/sessions-light.png#only-light)
![The Sessions view: the tiles and the table with a fork badge](assets/sessions-dark.png#only-dark)

The view opens on its bar: Add filter, the Columns control, and at the right edge the split Refresh button and the count
of the sessions the filters keep, `{n} sessions`. Add filter opens a "Filter by" field over the filters of one group,
Sessions: Companion only, Finished sessions, Non-companion age and Released agents. Companion only keeps the rows whose
companion reports or went silent. Finished sessions lists a session the engine is done with, one whose main agent ended
with the session or was released or whose every agent the mod released, which the view hides by default. Non-companion
age offers 1 h (the default), 15 min, 6 h, 24 h or No limit, and hides a row no companion reports once its last use lies
further back, with the session expiry as the outer bound. A companion row stays whatever its age. Released agents draws
a live session's released agents after its others, each with its released badge. Companion only, Finished sessions and
Released agents apply at once. Each filter off its default stands as a chip on a line under the bar, "companion only",
"finished sessions", "age: 6 h" or "released agents": a click on the chip opens its values again, its x removes it, and
from two chips on "Clear all" returns the four to their defaults. The filters live in the page's address, so a reload or
a shared link keeps them, and a bare `/sessions` reads the defaults. The head under the bar reads "Every session within
the session expiry that the filters keep, with the account each model is pinned to and each agent's cache-warming
recipe." with the disclosure "How pins and recipes work", whose paragraph says what a pin and a recipe are, and a "Cost
reference" link that opens the dialog on Plan rates. The tiles read Sessions (`{n} of {max}`, the rows the filters keep
against the engine's cap), Agents (`{n} of {max}` with `{n} busy, {n} idle, {n} parked`, both over the agents of the
rows the filters keep: the figure leaves out the subagents the mod released while their session lives, which draw no
line and keep their slot for one cache TTL after the release, then leave the engine, named on hover, and counts them
while Released agents draws them), Retained (`{n} MiB of {max} MiB`, "prompts kept for probes"), "Cache probes, 24h" (a
bar by outcome and `{n} ok ({pct}), {n} migrated, {n} held, {n} skipped, {n} failed`), "Expired between turns, 24h"
("misses cache warming exists to prevent") and "Bus gaps" ("times the keepalive engine missed bus events"). While the
filters hide rows, the Sessions and Agents tiles name them on hover, `{k} sessions hidden by the filters`. A tile whose
read failed draws a dash with the reason on hover, and Try again under the tiles reads them again. Auto reload starts at
5 s and reads the sessions, the recipes and the tiles at that one interval, and it waits while the Cost reference
dialog, the Release question or a row's menu is open. The probes and the recipe are explained on
[Keepalive](keepalive.md), the mod's words under [The mod](keepalive.md#the-mod) and the recipe's states under [The five
states](keepalive.md#the-five-states).

The card "Sessions" ("One row per session. Open a row for the cache-warming recipe each agent carries") offers "Expand
all" or "Collapse all". The Session column is always drawn, and the twelve others, which the bar's Columns control shows
or hides, are Key, Model, Context, Account, Runway, Last used, Requests, Last request, Agents, Companion, Cache warming
and Next probe, with Agents, Last request, Next probe and Requests hidden by default. Requests counts the session's
requests, the ones the router refused before any account was asked left out, and "View in requests" lists those with the
rest, so a session whose every request was refused still opens its analytics and its request log. Session takes the
card's free width and keeps at least 74 px, and a set of columns wider than the card scrolls inside it. Context reads
the main agent's context in tokens, `184K`, and under it its share of the model's window, `92% of 200K`, which its hover
repeats, and its cached prefix, the deepest prompt-cache entry of its pin, `prefix 171K`, with a dash for a session with
no pin or no figure. Runway reads the time until the projection of the pin's account reaches the ceiling on the window
that binds, among those the pin's model draws: a span such as `6 h 55 min`, `none` where the runway is gone, `unbounded`
where nothing burns or where the runway would pass the binding window's next reset, and a dash for an unpinned row or a
pinned row with no projection. On a pinned row whose pin carries a cycle its hover reads `cycle 20 s`, the gap between
the session's requests the slow-down's hold is sized from. Account draws the speed chip after the account's name on a
slowed session, with the Quota view's hover. The session cell cuts a long id with an ellipsis, whole on hover. A session
that branched from another conversation at depth 2 or deeper carries a fork badge under its id,
`fork, on another account` or `fork, same account` where the session it branched from is listed with a pinned account,
and `fork at 3` otherwise, with the base, the depth and the base's account on hover, and a click on the badge scrolls to
the base's row where the list holds it. Companion reads the mod's state, `reporting` with how long ago the person was
last seen at the terminal, `silent` or `none`. Cache warming reads the pin state badge, `busy`, `idle`, `parked`,
`released` or `unknown`, with the recipe summary, or "no recipe". Next probe reads the instant, "pending" or "not
scheduled". Before the first session the card reads "No live sessions" with "A session appears here on its first
request, or as soon as the keepalive mod registers it, and drops off once it passes the session expiry." While the
filters hide every row the router lists, the card reads "No sessions kept by the filters" with
`{k} sessions hidden by the filters.`

A row's menu offers "View in analytics" and "View in requests" once the session has ledgered traffic, "View in routing
log", which opens the log on that session's decisions an hour either side of its last request, and "Release". Release
asks "Release session": `Session {id} loses its pin to {account} and every recipe on it is dropped. The next request
picks an account again and the prompt cache on {account} goes cold.`, and answers "Session released".

Opening a row reads `Created {time}, {ago}` and one line per agent that has sent a real request, as Agent, Model,
Account, TTL, Last request, Last probe, Next probe ("on first stream event" or "not scheduled"), Probes / credited and
State, main first, then the busy, idle and parked agents. On a session with a companion the lines read Context and
Prefix after Model, each agent's context in tokens with its share of the model's window under it and its cached prefix,
the line of the row's own agent on the row's model as its row reads them, any other line as the router answers them for
its agent and model, and a dash where the router answers none, a released agent's and main's on another model among
them. A session no companion reports draws neither column. State reads the recipe's state word (`ended`, `held`, `not
retained`, `paused` or `retained`) with the cause beside it and the sentence of each on hover, and under them the mod's
lifecycle word for the agent with a parked agent's park kind and its deadline or wake time (`parked · background ·
deadline in 1h 12m`). A finished session, listed under Finished sessions, reads "The mod released every agent of this
session. Their records keep their slots of the cap for one cache TTL after the release and then leave the engine." above
its lines, which keep their ended state with the TTL, Last request, Last probe, Next probe and Probes / credited the
engine last read, and its Agents cell counts the lines drawn. A session the mod released whole keeps its released badge.
A session without a recipe reads "no recipe" and `Nothing is scheduled because {reason}.`

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
week row under a Fable model's 7-day group, and a warning tint on a cell past a full window. Under a regime of the cost
model ([Plan meter](statistics.md#plan-meter)), Windows by tier adds a group row naming the regime and its entrypoints
and a row per tier the regime names, the multiples against the base regime's smallest tier, the Peak band card a factor
row per regime that holds a window differently inside the band, and Plan rates a Regime control beside Tier offering
each regime and `Other` for the base rows, opening on `interactive` where the model names it, with the regime's
entrypoints in the caption. A model the cost model does not weigh, on a model tier with a weighed model, reads that
tier's latest weighed model, each of its cells marked with a `~` and its weight cell reading `estimated, as {model}`,
and the caption adds "A ~ marks an estimate: the model reads the latest weighed model of its model tier." A model of no
model tier, or of one with no weighed model, keeps its dashes and reads "not weighed". API prices draws the dollar list
prices per million tokens, input, cache read, cache write 5m, cache write 1h and output, with a fast-mode row under each
model that serves fast mode and a row under each model priced by prompt length, under the caption "USD per million
tokens at list price" and its note "An API-key account pays these. A subscription account pays them in overage and for
fast-mode answers." Both tables show one row per model tier, its newest model, in the cost model's `tier_order` with
`model_tier` naming each model's tier and a model of no tier last, and fold the older models under one row at the
table's foot that names their count, "1 older model" or `{n} older models`, and expands to them in the same order. The
fold is shared by both tables and starts closed at each opening. The price rows, the weights and how a probe is priced
are on [Pricing](statistics.md#pricing), which lists the shipped revision in force, and the dialog shows an earlier
entry under the Revision select.

## Settings

![The Settings view: Appearance, About and the client block](assets/settings-light.png#only-light)
![The Settings view: Appearance, About and the client block](assets/settings-dark.png#only-dark)

"Appearance" ("Stored in this browser only.") sets the Theme, Light, Dark or System ("System follows the operating
system preference."), the Time format, 12h or 24h ("Applies to every timestamp and axis tick."), and the Unit, which
mirrors the top bar's switch with a legend button of its own ("The unit every view prices a request in. Native reads a
subscription request as 20x window equivalents and a billed request in dollars."), the three browser preferences the
shell reads, and at a phone's width each row's control takes a line of its own under its label. "About" lists Version,
Profile, Metrics listener, Ledger path, API prices path and Subscription costs path (each as the path or "not
configured", then its table's entry in force, its revision count and the next date while one is pending, as
`effective {date}, {n} revisions, next {date}`, the last reading `{problem}` in their place while the cost model reports
a problem), Storage path, "Requests, samples and the routing log" and "Rollups" (each as `kept for {n} days`, or "kept
forever" where its key is 0), "Body capture" (as `at most {n} hours`, the longest a pool or a key keeps captured
bodies), and Admin secret as "configured" or "not configured", each path's and each retention line's hover naming the
bootstrap key that sets it, and closes on "The values the configuration file sets are on the installation page, and
`phoebis check-config` prints this router's."

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

The Routing log sits under the sidebar's Diagnostics heading, shown while the expert view is on or while either
Diagnostics view is open, and every link into the log opens it whatever the mode. It is one stream of routing decisions
and the events that explain them, newest first, 25 rows a page with First, Previous and Next. The rules a decision
follows are on [Pools, keys and routing](routing.md).

The filter bar holds one row: the range, Add filter, and at the right edge the split Refresh button and the count of
rows shown ("25 shown, more available"). The range offers the pills 1h (the default), 6h, 24h, 7d, 30d and Custom, with
From and To in a popover as on the Requests view. Add filter opens a "Filter by" field over the filters in three groups:
Kinds, Routing (Account, Key, Model, Pool) and Subject (Request, and Session, which a prefix matches). Every decision
and event kind lists by default. The Kinds filter opens with eight presets above the kinds: placements and pins, moves
and holds, exhaustion and refusals, account state and windows, cooldowns and backoff, keepalive, registry changes,
router life cycle. While every kind is shown, a preset's click lists its kinds alone, so keepalive reads the
cache-warming traffic in one click. From there a preset checks or unchecks its members, two presets combine, a hand edit
after a preset is one click apart, and unchecking the last kind returns to every kind. Checking Select all checks every
kind and unchecking it clears every check while every kind still lists, so a kind's click from there lists that kind
alone and a preset's click its members alone. Each filter in force stands as a chip on a line under the bar, as on the
Requests view: a set of whole presets reads their names ("kinds: moves and holds"), any other set of kinds reads their
words ("kinds: moved, held, placed +5"), and the request chip's hover counts the request's decisions and events ("1
decision, 0 events"). The widenings stand on each decision's card, after "Widen to": "this minute", "this session" (an
hour either side) and "this account" (a quarter hour either side). Every filter lives in the page's address (a preset
range by its name as in `range=6h`, a custom range as `from` and `to`, two UTC instants, the lists as comma lists), so a
filtered view is a link and Back returns to the view before it. A `kinds` value names exactly the kinds listed, so
`kinds=probe` lists probe decisions alone. An empty range reads "No decisions or events in this range." with "Widen the
range or clear a filter."

Auto reload is off when the view opens, and its poll is the log's own: the split Refresh button's dropdown sets it to
Off, 5 s, 10 s, 30 s or 1 min, out of the page's address, and each read takes every row that arrived since the read
before, up to eight pages, a new row landing at the top in the accent tint. Past eight pages a line reads "More rows
arrived than auto reload could fetch. Reload to read them.", and Next reads on from the last row the poll fetched. The
poll waits while the tab is hidden, on an older page until you go back to the first, and while a menu or popover under
the bar is open, with the interval kept and the cause named in the dropdown's title. A custom range whose end lay more
than a minute in the past when the page took it has nothing new to read, so the dropdown reads Off and stays disabled,
while a link whose range ends at the moment it was made reads on, each read refreshing the rows inside it.

A row reads Time (dated past a day as on the Requests view, a decision's request beneath it as a link to the request's
drawer), Kind (a decision in a filled badge with `attempt 2/3` beneath a failover attempt, an event in an outlined one),
Session (the first eight characters with the agent beneath), Account (a move names the account it left and the one it
landed on, and a registry change the pool, the key or the account it edited), Model, By, Fact, and Details, which opens
the request's drawer. By names what decided the row in one chip: the rule that fired (`spent rule`), the rung that
placed it (`static rung`), the strategy and the tier's priority (`Pace P0`, whose hover is the Pace hint), where a hold
waited (`cohort`, `session pin`), `slow-down` for a request the slow-down held and for the renewal after its hold,
`rewrite in flight` for a request that waited for a moved pin's first head and for the renewal after it, what exhausted
the pool (`budget`, `cooldown`, `fast lane`), the refusal (`client contract`, `client drift`, `key revoked`), `no rule
fired` for a renewal, or an event's reason (`window spent`, `429 on 5h`, `slow-down` for a slow-down's start and end,
`5h cleared` for a window read under its limit again, with the whole `5h cleared to 40% under its reset` on hover). Fact
gives the value that made it so as a muted label with its value, `gap`, `idle`, `prefix`, `until`, a held request's
`until 18:20` and `speed 0.5x`, the renewal's `held 42 s`, a slow-down start's `speed 0.5x` and an end's `slowest
0.35x`, a move's `runway` on its destination (a span such as `2 h 10 min`, `none` or `unbounded`) followed by `least
bad` or `rewrite only` where the destination did not fit, or a renewal's window utilization, such as `5h 96%`, where
that window's projection reached the ceiling. A verdict reads in one color across the rows, the card and the strip: good
when it served the request, warning when it changed its course, critical for a refusal or an exhaustion, and neutral for
none and no. Every account, key, pool, model, session and request a row names is a link. Each column but Time keeps a
set width and Time takes the rest of the card, so on a wide screen the request id under the time reads whole, and a
window narrower than the columns' total scrolls the table inside its card.

Every row opens beneath itself. A decision row loads its record and draws the decision card. Its verdict line names the
decision with its accounts and its pool as links ("Moved secondary to primary", "Attempt 2 of 2: placed on primary",
"Exhausted in jobs", "Held on nightly for the rewrite in flight of req_5b2e8d47-c19 until 11:03") and chips for the
tier, the rule or the rung, `held for the reset at 18:20` on a request that waited for a reset,
`held for the slow-down until 18:20` and `0.5x` on one the slow-down held, `held for the rewrite in flight` on one that
waited for a moved pin's first head, `reserved 10.4% of 5h` on a move or a placement that booked a reservation, the
pin's warmth and the cache verdict (`cache pending` before the attempt reports usage). "How the router decided" draws
the walk as a strip of stations in the engine's order, each with its verdict, the path the decision took in the accent,
every branch it did not take as a stub, a step the record does not carry dashed and reading "not run", and the ways out
as termini, Chosen, Held for a slot and Refused, the one reached filled in its tone. Three more ways out, Held for the
reset, Held for the slow-down and Held for the rewrite in flight, are drawn only when the decision took them, a spent
move that waited for a reset, a request the slow-down held or one that waited for a moved pin's first head, each in Held
for a slot's place, so the strip names six termini and draws three. A move's track carries the Fit station after Move,
the destination gate's table: a row per candidate the gate read with its projection, rewrite and reserved share on each
window the gate read, its runway, the window that binds, its end and its verdict, the station's own verdict naming the
order the gate took. Reserve stands after Pick, or after the last step where no pick ran, and lists the booking (the
account, the share reserved and the burn on each window, the expiry and the attempt), or reads "Nothing was booked: the
pin renewed in place." on a renewal and "Nothing was booked." on any other decision that booked nothing. A fresh
placement's partitions draw as the stations Spent, At cap, No fit and Winding down. A click or a hover on a station, a
candidate or a terminus opens its facts in a popover, and Tab, Enter, Space and Escape walk them from the keyboard. The
Shed station's Windows group reads the slow-down's `speed`, the share of it the holds apply as `ramp`, and the speed the
holds `realized`, in the order the engine applies them.

"Candidates, by rank" lists every pool member with the chosen or held-on member tinted: Account with its plan beneath
(the plan the router resolved from the organization type and the tier, "Free" among its words, or the tier word of a
record from before), Tier (`plan P0`, `overage P0`, `keys P1`), 5h, 7d and Fable as three lines each (the utilization,
the reset, the reading's source and age), or on a usage-based seat whose record carries its spend reading one cell
across the three, `spend 41%` over its reset with `estimated` under it where the router estimated the reset, Projection
and Runway, In flight against the cap, the Pace score, Prefix, and Verdict. A usage-based seat with no reading, its
share lapsed or not yet read, draws the three "not reported" cells as an account with no window does. The rows list the
chosen, renewed or held-on member first, then a move's source and its other destinations, then the ranked members by
rank, then the eligible ones the strategy did not rank, then the ones a partition or a check dropped with their
obstacle. Projection reads one line per window the candidate reports, the projection at the window's horizon, `5h 99% at
18:20` where the horizon is the reset and `7d 41% in 5 h` where it falls earlier, and Runway a span (`2 h 10 min`),
`none` or `unbounded`, each a dash for a candidate the gate did not read. The Projection cell's hover names the demand
where it differs from the burn the window sees, `demand 12.86 %/h over burn 6.43 %/h at 0.5x`, with the candidate's
slow-down factor. The Pace score stands under a header naming the window it read, `Spare rate, 7-day per hour` or `Spare
rate, 7-day Fable per hour` (`Pace` where no subscription candidate names one, and `Pace, 7-day left per hour` on a
version 1 record), each cell's hover reading `{pct} of the 7-day window left over {n} h to its reset at {time}, less
{burn} of burn: {rate}.` (or `to its subscription's end at {time}`) followed by `Inside its tier, the highest spare rate
wins.` The pick line names the accounts whose scores tied, the runway that narrowed them where it did, and whether the
draw chose among more than one. Verdict carries the one chip that decided it (`429 on 5h until 14:55`, `budget $1.10 of
$5.00`), whose hover says why the member was passed over, and beside it a badge for a destination the gate read that did
not fit, `rewrite only` or `least bad` in the warning tone and `ending` neutral. Three folds, "Request facts", "Pin and
warmth" (a record that pins nothing reads why: not looked up or pinning off, and a pin that carries them reads Cycle and
Slowed until) and `Settings in force since {time}` (which a guardrail's attach to or detach from the pool moves too,
since the fence re-reads the pool's settings), stay closed until opened. In Pin and warmth, a stamped pin's Shed mark
names the account it left, `shed from` after a shedding round and `failed over from` after a failover's landing, with
the instant. The settings fold lists the pool's twenty-four settings in the card's order with Load shedding and
Slow-down reading `on` or `off`, Stretch and Hold as spans, Return to plan as its word and Return dwell as a span or `at
once` at 0, or "The key and its pool had left the registry". A refused record, which no pool setting entered, draws no
settings fold. A record written before a setting existed reads `not recorded` for it, Load shedding and Window ceiling
on a version 1 record and the five slow-down and return-to-plan settings on an older version 2 record, and a version 1
record lists after the twenty-four, each marked `(then)`, the three settings it carried that a later record does not.
"Outcome" lists the pin written, the reservation (the account, the share per window and the expiry), the cache-map
entries the answer wrote (`pending` until it ends), the cache read with the TTL tier of its deepest entry, the hold,
reading `for the reset at {time}` where the request waited for a reset, or `for the slow-down until {time}` with the
chips `speed 0.5x`, `stretch 42 min of 30 min`, `bound 2 min` and `cycle 20 s` where the slow-down held it, or `for the
rewrite in flight of {request} until {time}` with `bound 2 min` where it waited for a moved pin's first head, a Cause on
a failover's landing or a move with a cause (`429 on 5h` or `fast lane 429` for a cooldown, `re-auth required by
credential refused` for a mark), the answer an exhaustion or a refusal gave, and the spend against a budget. With four
entries or more, the cache-map row draws one chip per tier with its count and depth range, such as `message ×11 depth 1
to 11`, then its three deepest message depths, and its control "All {n} entries" lists every entry until "Latest 3"
folds them again. The foot offers the three widenings after "Widen to", this minute, this session and this account,
names the record's version as `Record v{n}`, and "Copy as JSON" copies the record route's answer whole. A record the
retention purged reads "The record left with the retention", and one still loading "Loading the record".

An event row opens to a panel of pairs: When to the second, the account and the pool as links, then the kind's own
pairs. An event that names a request ends with "Source request", which opens the log on that request a quarter hour
either side, and "Details", which opens the request's drawer. An account state change names who moved the state:
`credential refused`, `credit balance`, `funding retried`, `refresh failed`, `by hand` or `paired`, and a deleted
account's last row reads `deleted`. A pin moved panel names the account the pin left, the one it landed on, the reason
and what moved it, where a rule's gate read the destination, Destination fit (`fits`, `rewrite only` or `least bad`) and
Runway (a span, `none` or `unbounded`), and on a failover's move its Cause, `429 on 5h` or `fast lane 429` for a
cooldown and `re-auth required by credential refused` for a mark, in the account state panel's words. A shedding round's
panel reads Started by, Trigger (`5h projection 99% at 18:20 over the ceiling 95%`,
`7d projection 96% in 5 h over the ceiling 95%`, or `5h stretch 42 min over the threshold 30 min` for a round the
stretch fired, followed by Demand, `12.0 %/h to the reset at 18:20`), Fired by (`the projection` or `the slow-down`),
Excess (`9.5 %/h`), Cap with the target and the burn removed, Unit, Burn average, Warm sessions, the Pins table
(Session, Agent, Warmth, Burn, Last prompt, Rewrite as the Max 20x equivalent of the pin's weekly window, labeled
`of 20x 7-day` or `of 20x 7-day Fable`, Destination, Mark), Skipped (one chip per pin, its session and its reason,
`off window` for a pin whose model does not draw the window that fired), Planned destinations, Outcome (`marked 2`,
`nothing to mark: no runway`, or `nothing to mark: off window` for a round that walked no pin and left one out off the
window that fired), Next round after, Dwell and Survive a restart. A mark cleared panel reads the pin, Reason
(`healed: the 5h projection read 91% under the ceiling`, `healed: the stretch read 24 min under the threshold 30 min`
for a mark the stretch made, `moved` or `released`), Marked (when, on which account) and Standing for, and a reservation
expired panel reads Request, Attempt, Cause, Reserved (`5h 10.4%`, `7d 1.2%`), Held for and the Effect
`the booked share left the account's prediction`. A slow-down started panel reads Speed (`0.5x`), Stretch
(`42 min of 30 min`, the threshold after `of`), Demand (`12.0 %/h`), Reset and the Effect
`requests of the account's sessions wait their cycle times the slow-down up to the hold`, and a slow-down ended panel
Lasted, Slowest (`0.35x`), Held (the time the span held requests) and the Effect
`requests of the account's sessions pass unheld again`. The kinds and what each row records:

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
| mark cleared | A shedding mark left a pin: it moved, was released, or its account's window read under the ceiling again, or its stretch under the threshold. |
| model permitted | An account served a model another account had rejected. |
| model rejected | An account answered 404 for a model, which cools that model on the account. |
| overage ended | An account left overage at its 7-day reset. |
| overage started | An account went into overage, so its requests draw on extra usage. |
| pin created | A placement pinned a session to an account. |
| pin expired | A pin was released, idle past the pin window or for the reasons it lists. |
| pin moved | A rule or a failover moved a session's pin to another account, with the destination's fit and runway where a rule's gate read it and the failover's cause. |
| pin released | An operator released a session's pin by hand. |
| plan ended | A profile read found the account's organization holding no plan, so the account is flagged plan ended, with the read's source (the profile read, or the usage 403 that made it due) and the profile's organization type, rate-limit tier and subscription status. |
| plan restored | The account's plan-ended flag cleared, with its source (a profile read that found a plan, or a pairing confirm) and the profile's organization type, rate-limit tier and subscription status. |
| poll backoff ended | The usage poll answered cleanly again after backing off, or the account's plan ended while a run was open. |
| poll backoff started | The usage poll was rate limited, so decisions read the account's windows from response heads. |
| poll failed | The usage or the profile endpoint answered a passing fault, with the endpoint, the status or the transport failure and the text. |
| refresh failed | An access-token refresh failed. |
| refresh succeeded | An access-token refresh succeeded. |
| refresh uncertain | An access-token refresh ended without a clear answer. |
| registry changed | An operator changed an account, a guardrail, a key or a pool: a creation records the row's snapshot, an account's, a key's or a pool's update each changed field with its value before and after (a list, such as a pool's guardrails, as its items joined by commas, or `none` when empty), a guardrail's update each changed field with its value before and after, its pools and keys among them, a deletion or a revocation nothing more, a change of a pool's members the new member list, and a key's rotation the new secret's prefix with the instant the retired secret stops being accepted. |
| reservation expired | A request ended without usage or its booking turned an hour old, so the share it reserved left the destination's prediction. |
| router started | The router started and loaded its pins, shedding marks, window readings and affinity entries. |
| shedding round | A round ran on an account whose projection on a plan window read over the ceiling at the window's horizon, or whose stretch stood over the slow-down's threshold for the safety wait, with the pins it marked or why it marked none, and kept its marks across a restart. |
| slow-down ended | An account's slow-down span closed: its factor read 1 again for the safety wait, the window it slowed for passed its reset, or its entry left at the purge, with the span's length, its lowest factor and the time it held requests. |
| slow-down started | An account's slow-down began: its factor read under 1, so requests of its sessions wait their cycle times the slow-down up to the hold. |
| stream interrupted | A stream ended short after its head, with the cut's code and the error's words. |
| warm prefix expired | A warm prefix's cache-map entry lapsed past its TTL. |
| wind-down started | An account's subscription end entered the pool's wind-down, so no fresh session starts on it. |
| window cleared | A quota window read under its limit again after a reading at the limit, with the readings before and after and the reset unchanged. |
| window reset | A quota window reset, with the reading before it. |
| window spent | A response head, the poll or the prediction read a quota window at its limit, once per reset cycle: a newer head or poll reading of the same cycle within two points of 1.0 after it neither clears nor repeats it, a head naming no reset, taken before the announced reset, is of the same cycle, and the cycle runs from the instant the wall was announced or, for a wall restored at a start, from the stored reading's instant. |

Other views open the log on what they show: a Requests row and its drawer on the request a quarter hour either side, the
Sessions row menu's "View in routing log" on the session's decisions an hour either side of its last request, a pool
card's "Moves in the last hour" figure on the pool's moved decisions over the last hour, the card's pool exhausted row
and the dashboard's pool exhausted item on the pool's exhausted decisions from the moment the condition began, the
routing conditions' dashboard items and pool card rows on the pool's shedding rounds, the account's moves and spent
windows, its spent windows, or its slow-down starts and ends, from the moment the condition began, the "Routing log"
action on an account's Requests card on the account's rows over the last hour, and a model a decision card, an event's
detail or a log row names on the model's rows over the last 24 hours.

## Meter

![The Meter view: the groups per account and window kind](assets/meter-light.png#only-light)
![The Meter view: the groups per account and window kind](assets/meter-dark.png#only-dark)

The Meter view sits under the sidebar's Diagnostics heading after the Routing log, shown while the expert view is on or
while either Diagnostics view is open, and every link into it opens it whatever the mode. It lists each subscription
account's completed windows of the last 30 days, the rise the upstream's readings showed against the share the router
priced for its own requests in each, grouped by account and window kind.

The filter bar holds one row: Add filter over Account, and at the right edge the split Refresh button and the count of
groups and windows the filter keeps (`{n} groups, {m} windows`). The view offers no range, since it reads the trailing
30 days. The Account filter stands as a chip ("account: nightly") on a line under the bar and lives in the page's
address (`accounts=nightly`), so a narrowed view is a link and Back returns to the view before it. Auto reload is off
when the view opens, and the split Refresh button's dropdown sets it as on the Routing log, out of the page's address.
Under the bar a line reads "Each subscription account's completed windows of the last 30 days, grouped by account and
window kind: the rise the upstream's readings showed against the share the router priced for its own requests in it. 1.1
means the meter charged 1.1 times the table. A ratio under 0.8 or over 1.25 after a rise of at least 5% is marked."

Two cards list the groups, "Weekly windows" (the 7-day window and each model week) above "5-hour windows", each with
"Expand all" in its head, which opens every group of its card. A group is one account's windows of one kind, the groups
by account id and, inside an account, the 7-day window before the model weeks, and a window kind an account never
reported draws no group. A collapsed group is one line: Account (a link to the account), Window ("7-day", "Fable",
"5-hour"), Latest ratio, Off (the count of off windows, or "none"), "Ratio, 30 days" (the spark), Latest end (its date
always written) and Windows (the count). A click on the line or Enter on it opens the group, its chevron turning down,
and a second closes it. An open group lists its windows newest first, the newest 10 of a 5-hour group and the newest 25
of a weekly one, under Ended, Rise, Predicted (with the charts' `~` and its estimate hover where an attempt in it was
priced at a model tier's estimate), Ratio and Requests, then, while windows stay unlisted, `{n} of {m} windows, newest
first` with "Show 25 more". A group's open state belongs to the page, so a reload or a link opens every group shut.

The spark charts the group's windows over the 30 days to the newest window the view lists, so every line reads one axis:
the ratio's base-2 log clipped to 0.5 and 2, the band 0.8 to 1.25 shaded with the 1.0 line in it, the windows the band
judges joined in a line, each off window a dot in the warning tone, and with eight judged windows or fewer each judged
window a dot too. A window whose rise or priced share is under 5% stays out, and the spark's hover counts the judged,
the off and the left-out windows (`{j} judged, {o} off, {l} unjudged`).

A ratio takes the warning badge where the rise and the priced share each reach 5% and it leaves 0.8 to 1.25, the test
the Meter drift condition reads, with the four decimals on hover. A ratio the band does not judge reads muted, its hover
saying "the rise or the priced share is under 5%, so the band does not judge this ratio".

With no completed window under the filter each card reads "No completed window yet", naming the accounts the bar narrows
to (`No completed window yet for {accounts}`), and a read that failed draws its error with Try again.

The [Dashboard](#dashboard)'s Meter drift against the table item, a [pool](#pools) card's row of that condition and the
"Meter" action of an [account](#accounts)'s Meter drift card open the view narrowed to the account.
