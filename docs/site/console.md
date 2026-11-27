# The console

This page is for the operator reading or using a view of the web console the admin listener serves. It covers the
shell first (the login screen, the sidebar, the theme switch and the expert view), then one section per view in the
sidebar's order, each with its screenshot, what the view shows, what each control does and the dialogs it opens. The
mechanism behind a view lives on its own page, which each section links. Every data table alternates its row tone, and
a row's hover or selection reads over it. A count of a thousand or more groups its digits, such as 1,204 requests.

Every list view (Accounts, Analytics, Guardrails, Keys, Pools, Quota, Requests, the Routing log and Sessions) opens on
one filter bar, which draws the slots the view has in one order: the range pills, the search, Add filter, the view's own
controls with its primary action last, among them on a view with an order the Sort control, a button reading the key and
its direction whose popover offers the view's keys and Ascending or Descending, the choice carried in the address, and
at the right edge the split Refresh button and the count of what the view lists. The control starts on Name on Accounts,
Analytics, Guardrails, Keys and Pools, on Default order on Quota, on Time newest first on Requests and the Routing log,
and on Created on Sessions. When the window is too narrow for the bar's controls on one line, the trailing ones wrap
onto a second row. Add filter opens a popover over the view's filters, and each filter in force stands as a chip on a
line under the bar. The Refresh button's body reads the view again now, and its dropdown sets auto reload to Off, 5 s,
10 s, 30 s or 1 min, 5 s when a view opens and off on Requests and the Routing log. Auto reload waits while the tab is
hidden, while a drawer, a dialog or a menu is open and on an older page, with a muted mark and the cause in the
dropdown's hover. A tick that arrives while the view's previous read is still in flight is skipped, and the first tick
after it lands reads again. On Requests and the Routing log a tick also waits while the list for the view's filters is
still loading, unless its last read failed. When a wait ends or the interval changes a full interval after the last
answer, the view reads at once, unless a read is still in flight, which leaves the read to the next tick. A read with no
answer after a minute fails as if the router never answered, and the next tick reads again. The filters, the search and
the range live in the view's address, so a reload or a shared link keeps them, and the interval stays with the open
page. An Accounts address that names no account opens the first account the router lists among the rows the list shows,
or the router's first account where the filters hide every row. An address that names an id the router does not hold
keeps its address: the list selects no row, and the detail's place reads "No account holds the id" with that id and "The
ledger keeps its requests.", with the link "Open its requests" to the Requests view filtered to that id.

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
a 401, the screen reads `The router did not answer ({status}).`, the status left out when no answer came at all or none
within a minute, beside a "Retry" button that reads the session again. A session that ends while a view is open returns
to the screen with `Your session ended, so log in again.`

The sidebar reads the brand "Phoebis Router" over the views in this order: Dashboard, Quota, Accounts, Pools, Keys,
Guardrails, Requests, Analytics, Sessions, Settings. Under them a group headed "Diagnostics" holds Routing log and
Meter, shown while the expert view is on or while the open view is a diagnostics view, so a link into either view opens
it whatever the mode and leaves the mode as you set it. The sidebar's foot carries the "Expert view" switch and the
router's version as `v{version}`. The top bar reads the open view's title (the document title is
`{title} - Phoebis Router`), the "Unit" switch, the "Week" control and the unit legend's button, "Log out", and the
theme switch "Theme" with Light, Dark and System, stored under `phoebis.theme` (`system` until you pick one). When the
router answers from a build other than the one the console's files were built from, a banner above the top bar reads
`The router runs a newer build than this console. Reload to pick it up.` beside a "Reload" button, and it stays in view
with the top bar until the page reloads. The time format lives under `phoebis.timeFormat` (`24h` by default) and the
palette, Default or GitHub, under `phoebis.palette` (`default` when unset), set on the Settings view's Palette row. The
theme, the palette, the time format, the unit and the week are browser preferences, which the Settings view's Appearance
card also sets, and nothing in the shell changes a router setting. On a narrow window a menu button opens the sidebar
over the view, and the top bar moves its controls onto a line of their own under the view title where the two do not fit
side by side.

The Accounts, Analytics and Pools views load their code on first open. A view that fails to draw tells a bundle that
changed under the tab ("This page could not load") from an error while drawing ("This page hit an error"), each with its
own words and a Reload button.

The "Unit" switch sets the unit the Dashboard, Pools, Keys, Requests, Analytics and Sessions views price a request in,
and the unit the Sessions view's burn lines and the Burn tab's "Plan per hour" and "Spend per minute" cards read their
rates in, stored under `phoebis.unit` (`native` until you pick one). Its button "What the units mean" opens the legend,
which takes the focus, closes on Escape, a click outside it or the focus leaving it, and hands the focus back to its
button on Escape, with one line per position in the switch's order:

- Native: "Each request in the unit its funding pays in: a subscription request as a share of the week the week control
  names, an API key, fast-mode, overage or usage-based request in dollars."
- All in dollars: "Every request in dollars at the API price list, a subscription request at its dollar equivalent."
- Billed dollars: "The API key, fast-mode, overage and usage-based requests alone, the dollars the account holder is
  charged, a billed keepalive probe's among them."
- Dollar equivalent: "The subscription requests alone at the API price list."

The legend ends on what a subscription figure is: "A subscription figure is the share of a Pro 7-day window the same
tokens would spend, the cost model's units over the Pro capacity, so figures of different account tiers compare and add.
The week control reads it as a share of a Max week, five Pro weeks, or an Ultra week, ten Pro weeks. On a request or one
account's row the figure's hover names the account's plan and its share of each window the account has." On a narrow
window every position but Native reads a short label (`All $`, `Billed $` and `Equivalent $`), and at a phone's width
the legend's button leaves the top bar and stays on the Settings view's Unit row. The cost model behind the figures is
on [Plan meter](statistics.md#plan-meter).

The "Week" control beside the switch sets the week a subscription figure reads in under Native, "Pro week", "Max week"
or "Ultra week", stored under `phoebis.week` (`ultra` until you pick one). Under All in dollars, Billed dollars or
Dollar equivalent it reads disabled with the title "The week applies under Native". On a narrow window it reads the
short labels `Pro`, `Max` and `Ultra`, and the Settings view's Unit row carries it too. Where the cost model sizes no
7-day capacity for the chosen week's plan, every cost figure in that week reads a dash titled "Unpriced: the cost model
holds no 7-day capacity for the {plan} week", a card draws its error state with that sentence and a tile's note under
the dash reads `unpriced`, until you pick a week the cost model sizes or a cost model that sizes it replaces the table
at the next start ([The cost reference](#the-cost-reference)).

The expert view, whose switch carries the title "Shows every field of the pool and key dialogs, the advanced ones
included.", is stored in the browser under `phoebis.expertView` and starts off. With it on, the pool dialog's rail gains
the groups Auto mode, Placement, Slow-down, Failover and Diagnostics, and its other groups show the fields Pin sessions,
Windows' Burn average (s), Load shedding's Preset, Shed cap (%), Shed dwell (s), Warm sessions, Fable reservation
shedding and Preview, Overage grace (min), Return to plan, Return dwell (s), Wind down (min), Add a cache marker to a
request without one and the cache TTL's per-model overrides. The key dialog gains Server-side auto mode, Pool rules and
List [1m] twins in the models listing, and the sidebar a Diagnostics group of its own. With the mode off, a group with
hidden fields closes on `{n} more settings in the expert view` (`1 more setting` for one), whose link turns the mode on,
and a hidden field that holds a refusal shows anyway. Closing a dialog with unsaved changes asks "Discard changes?" with
"This dialog has changes that are not saved.", "Keep editing" and "Discard". A dialog draws its fields afresh at each
opening, so a picker's search text starts empty.

## Dashboard

![The Dashboard view](assets/dashboard-light.png#only-light)
![The Dashboard view](assets/dashboard-dark.png#only-dark)

The Dashboard reads its figures again every 30 seconds. A line above the tiles names what needs your attention, worst
first, each item a badge that opens the account, pool or key where it is fixed, and reads "Nothing needs attention"
otherwise. The line keeps one row of items and folds the badges past it behind a "+N more" badge that opens the full
list, so the tiles hold still. An item with no such place (a drift refusal that named no key, a parked record without a
pool, a pool deleted since, one of the router's own writers, the unpriced model item, whose model the cost reference
dialog's Plan rates draws with dashes, a page unknown to the console or a condition the console does not know, which
reads "Unknown condition" with its kind on hover) stays a plain badge. The unpriced model item of a model that a catalog
listing added carries no requests until a plan-funded one lands, and its line then says that the model has no weight in
the cost model and that a plan-funded request on it will price no window share. The conditions are the ones the
`phoebis_attention` gauge exports ([Conditions and gauges](statistics.md#conditions-and-gauges)), and the pool exhausted
item opens the [routing log](#routing-log) on the pool's exhausted decisions from the moment the condition began. The
four routing conditions read as one sentence each, the subject first, then the count, the settings they name (with the
pool's values where the item carries them) and the start, such as `main: 4 rounds marked nothing, no destination had
runway, five_hour_ceiling 95 % and weekly_ceiling 100 %, since 14:45` for a pool whose rounds found no destination with
runway. An account's count reads `2 moves landed and the 5h window was spent within the hour` where moves onto it were
followed by a spent window, or `1 wall reached on the 5h window` where it reached its wall, followed by `the last round
projected 96 %` where a projection round on that window came before, or by `the wall came 25 min before the reset` where
a slow-down round came before (`the wall came at the reset` for a wall at or after the reset). An account whose
slow-down held its requests past twice its pool's stretch threshold within the day reads, under the label "Slowed past
twice the stretch", `studio: held 2 h 8 min of the last 24 h over 1 h, 72 requests held, slow_down_stretch_seconds 1800,
since 09:12`, the bar after `over`, the requests held in the day and the pool's threshold. Each opens the [routing
log](#routing-log) on the pool's shedding rounds, the account's moves and spent windows, its spent windows, or its
slow-down starts and ends, from the moment the condition began. An account whose meter drifted reads `acct-a: 5h charged
2.0x the table, 7d 1.3x, since 14:20`, each off window with the ratio the meter charged against the cost model, under
the label "Meter drift against the table", and opens the [Meter](#meter) view on the account. An account whose refresh
token nears its fixed end reads its days left as `expires in 2 days`, or `expired 3 days ago` past the end, in the
warning tone, and the item carries the account's Re-pair action. An account whose unused limit reset ends within the
warning's days reads, under the label "Reset ending", `nightly: a reset ends in 4 days` in the warning tone, `the first
of 2 resets ends in 4 days` where several end within those days, and `ends today` on the last day, and opens the
account's card on the [Quota](#quota) view. A wall on an account that holds a reset usable now for a window at its limit
adds `, a reset can clear it` after its own words and opens that card in place of the routing log. Both point at the
card's "Use reset", and the Dashboard uses no reset. Four items of the session engine and the cache warming open the
[Sessions](#sessions) view: `Keepalive bus gaps: {n} since the start` in the serious tone while the keepalive subscriber
missed bus events in the last 24 hours, `Session engine cap at {pct}: {cap}` in the warning tone while one of the
session engine's caps (sessions or agent records) stands at 90 percent of its max or more, `Keepalive cap at {pct}:
retained prompts` in the warning tone while the retained prompts stand at 90 percent of their max or more, and `Expired
between turns: {n} in 24h` in the warning tone while prompt caches of agents that the keepalive still held expired
between turns on a pool whose keepalive policy is on, under a key the keepalive may probe for. A parked ledger record
adds "Replay parked records", which answers `Landed {n}, {n} still parked`. Where the line is too narrow for its lead,
the "+N more" badge and that button with its answer together, the button and its answer take a second row. The line also
reads again when the window regains focus, and a tick or a focus that finds its previous read still out is skipped, so a
slow read lands and the next tick reads again.

Five tiles follow, each reading the 24 whole hours before the current hour and the current hour so far, which its
sparkline draws as the last point, and a note per hour under a tile reads over the hours covered: "Requests, 24h" with
`{n} per hour`, "Tokens, 24h" with `{pct} served from cache`, "Estimated cost, 24h" in the top bar's unit, headed
"Dollar equivalent, 24h" under Dollar equivalent and "Billed dollars, 24h" under Billed dollars, "Error rate, 24h" with
"No requests", `Most common: {code}` or "No errors", and "Refused, 24h" with `Most common: {code}` or "None", which
counts the requests the router refused before any account was asked. "Requests, 24h" and the error rate leave those out
and say so on hover, "Requests the router asked an account for. The refused requests are in the Refused tile." on the
first and "Errors among the requests the router asked an account for, the refused requests left out" on the second. The
error rate and its most common code also leave out the requests whose client left before its answer ended. The Refused
tile's hover names the refusals it counts apart from the router's own, "Requests refused for the client's or a policy's
cause before any account was asked. The router's own refusals, account_canceled, accounts_unavailable,
body_budget_exhausted, key_body_reads, ledger_behind and router_unavailable, count as requests instead." Under Native
the cost tile reads the subscription traffic's figure in the chosen week over the billed dollars, a side with no traffic
left out, with `{usd} billed per hour` under them, or the figure per hour while no request was billed. Where a
subscription request of the day priced nothing, its model without a weight in the cost model, the subscription side
reads a dash with the hover "No subscription request priced: its model has no weight in the cost model", the note under
it reads the dash per hour while no request was billed, and the spark leaves that hour a gap. Under the Max and Ultra
weeks the figure waits on the settings, so the tile draws its placeholder until they answer. Under All in dollars and
Billed dollars it reads those dollars with `{usd} per hour`, and under Dollar equivalent the subscription traffic's
dollars at the API price list with `{usd} per hour` as well. The tile's dollars count a billed keepalive probe's, one on
an API key account or a usage-based seat, in overage or in fast mode, while the other tiles leave the probes out. Under
the tiles a line says when the figures were read, `As of {ago}`, by the oldest of the page's reads, or names the read
whose last refresh failed and why: `Last refresh of the {read} failed: {error}. Showing data as of {ago}`, the read
being the 24-hour figures, the accounts, the keepalive overview, the recent requests, the recipes, the sessions or the
settings.

Two rings, "7d remaining" and "Fable week remaining", show how much of the pooled 7-day and Fable week capacity is left,
under the subtitles "Subscription accounts, 7-day window, weighed by tier" and "Subscription accounts, Fable week,
weighed by tier", the first with a "Cost reference" link that opens the dialog on Tiers and peak band. Each subscription
account is weighed by its tier's 7-day capacity from the subscription cost model over the capacity of the tier the week
control names, the Fable week being one factor of every tier's 7-day window, and listed with its own remaining share,
the most of its window left first. The ring's center reads the share "of capacity left", and the line under it reads
`{n} of {m} {plan} weeks left` in the chosen week, such as 0.39 of 1.30 Ultra weeks, or `{n} of {m} {plan} Fable weeks
left` under the Fable ring. Pro has no Fable week, so under the Pro week the Fable ring draws no line, and its ring, its
center and its rows still draw. A window that held a share and whose reset has passed with no reading since counts as a
full allowance. An account is named under the ring with the reason where its poll carries no such window ("no 7-day
window", or "no Fable week" on a Pro account), where no window has been read yet ("not read yet"), where the router
holds no share of the window ("share not reported"), where the router prices no tier for it (a free plan, a usage-based
seat, or a tier the cost model does not hold, a Team or Enterprise seat on a held tier weighing like any other), where
its tier has no capacity on the window (`{plan} 7-day capacity not measured`,
`{plan} Fable week capacity not measured`), or where it has no subscription profile. Without a subscription account the
card reads "No subscription accounts" and "A subscription account reports the window this card draws.". Where no listed
account carries the window the card reads "No account with a 7-day window." (or "No account with a Fable week.") over
the named accounts, and where they carry it with none sized "No listed account's tier is sized for the 7-day window."
(or "for the Fable week."). Where each listed account with a sized tier holds no share of the window, the card reads "No
listed account with a sized tier reports a share of the 7-day window." (or "of the Fable week."). Under a chosen week
whose tier has no 7-day capacity the card reads the week's unpriced sentence
(`Unpriced: the cost model holds no 7-day capacity for the {plan} week.`) in place of the tier sentence. Each account
keeps one color across the Dashboard, the Quota view and the Analytics, in the order of its id, the eight colors cycling
past eight accounts, and an account the registry does not list draws muted.

"Accounts" draws one small card per account, linking to its detail, with the status badge, the speed chip beside it on a
slowed account (`0.5x`, as [Quota](#quota) describes it), the email, the window bars (`No window since the plan ended
{ago}` in their place on an account whose plan ended, "Not paired, no window" or "Needs re-auth, no window" on a
subscription account that is not paired or needs re-auth, "Not read yet" on one whose poll has read no window yet, and
"none" muted in the slot of a window the account does not carry, such as an approved Ultra account's 5-hour slot, with
the window's words on hover) or the spend bar of a usage-based seat, "Overage active" while requests bill as extra
usage, the cooldown line, and "API key" for an API-key account. "Recent requests" ("Last 10 across every pool", with
"All requests" opening the Requests view) lists Time, Key, Account, Model, Status, TTFB, Tokens and Cost, the status
badge reading `×{n}` for a request of several attempts. Time is the column that gives way: the time reads whole, and
the request id under it is cut by the room past its first twelve characters, whole on hover, while Key and the other
columns read whole. Cost reads as the Requests view's column does: under All in dollars and Dollar equivalent a
subscription request's dollar equivalent over `equivalent`, `$0.00` for a client classifier call, under Billed dollars a
dash, and a billed request's dollars under each of the three but Dollar equivalent, where it reads a dash. Under Native
a subscription request reads its figure in the chosen week alone, with the hint the Requests view's column describes,
the dollar equivalent and the price list and cost model in force on hover, a client classifier call its zero figure with
"Free on the plan: a client classifier call takes no window share" leading its hover, and a billed request its dollars.
The column header's hover names what each kind of request reads under the position. Where the table runs wider than its
card it scrolls inside it.

"Sessions" ("Sessions, their agents and the engine that keeps their caches warm", with "Open") reads the active pins as
`active pins across {n} accounts`. Its grid counts the live sessions as the [Sessions](#sessions) view's summary line
does, the sessions the router lists that have not finished: Agents counts each session's agents less the released ones,
main among them where no line names it, and Busy, Idle and Parked count them by lifecycle word, a registered, unknown or
unreported agent in none of the three. Under the grid, "Engine caps" reads the three caps as `{n} of {max}`: Sessions,
Agent records, whose hover says that a released record that held a slot keeps it for one cache TTL after the release,
and Retained in MiB. "Cache probes, 24h" reads `{ok} ok ({pct}) of {total}` over a bar of the day's probes by outcome
(ok, held, skipped and failed), and a line reads `expired between turns {n} · bus gaps {n}`. The expired part takes the
warning ink while the attention item `expired_between_turns` holds, which counts the expiries of agents that the
keepalive still held on pools that warm caches under keys the keepalive may probe for, so the part's own count, the
keepalive overview's over every pool, can stand above zero in the plain ink. The bus gaps part takes the ink while
`keepalive_bus_gaps` holds, the last gap's instant on hover. Where the keepalive overview's first read fails, the caps,
the probes and the line read a dash each under `Could not load the keepalive overview: {error}` and a "Try again"
button. A failed recipes read draws in the card as a failed keepalive overview does, under a notice of its own. What
each figure means is on [Statistics](statistics.md).

## Quota

![The Quota view: the Accounts section](assets/quota-light.png#only-light)
![The Quota view: the Accounts section](assets/quota-dark.png#only-dark)
![The Quota view: the Quota windows section](assets/quota-windows-light.png#only-light)
![The Quota view: the Quota windows section](assets/quota-windows-dark.png#only-dark)

The Quota view covers the subscription accounts that are paired, not awaiting re-authentication and whose plan has not
ended, since an API-key account has no quota window, and with none of them to list its Accounts section reads "No
accounts to list" with "The view lists a subscription account that is paired, not awaiting re-authentication and whose
plan has not ended." Its bar offers the Sort control, which orders the Accounts section's cards, and "Refresh all",
which reads the endpoint and the limit resets of each of those accounts and whose hover reads "reads the endpoint now
unless the account is backing off, and records the reading as the scheduled poll does", and at the right edge the split
Refresh button, whose body reads the account list again ("Reads the list again now"), and the count of those accounts,
`{n} accounts`, followed in the critical ink by those needing attention, `, {n} need attention`. Auto reload starts at 5
s. A section bar shows one of two sections at a time: Accounts, where the view opens, and Quota windows, which a link or
a reload with `#quota-windows` lands on. A card's reading is the router's reported sample where the router holds one for
the window and the ledger's [quota sample](statistics.md#quota-samples) otherwise. The router answers no quota window
for an account in the `disconnected` or `reauth_required` state or whose plan ended (its `plan_ended_at` set), whatever
reading it held before, so no view draws one for it.

Accounts draws a card per account. The Accounts section's cards sort by the bar's Sort control: Default order (the
route's order, no direction), 5-hour pace, 7-day pace and 7-day Fable pace (each descending, an account without that
window last), or 5-hour recovery, 7-day recovery and 7-day Fable recovery (each ascending, the soonest reset first, an
account without that window's reset ahead and a usage-based seat among them by name under either direction). Every card
holds the same slots in the same order: the header with the label (the email on hover) and the status badge, one row per
window ("5h", "7d" and "Fable"), the Extra usage line, the chips and the poll reading. A usage-based seat's card draws
one Spend row in the windows' place and no Extra usage line. The pace orders read the lead the router answers for each
window, how far its used share runs past the share of the window elapsed, and file an account whose window expired or
carries no lead with the accounts without that window. Each live window's meter reads the share left and its pace,
`{window}: {pct} left, {n}% ahead of pace`, `{n}% behind pace` or `on pace`, with ` to its end at {time}` where the pace
runs to the subscription's end rather than the reset, and a mark at the share of the window still to come. On a screen
under 900 px wide the reset label takes a line of its own under the meter. Under the meter the window reads `resets
{ago}`, followed on a window the router projects by the share the projection leaves at the window's horizon: `resets in
2h 13m, 19% left at the reset` where the horizon is the reset (within a minute), `resets in 3d 5h, 56% left in 5 h`
where it comes earlier (five hours out on a weekly window, or the account's end), and `{pct} left now` once it has come.
A window the sessions spend before its horizon reads the time until it runs out, `spent in 2 h 10 min`, counted back
from the projection at the horizon along the burn, so the span follows the router's forecast with the charges and the
reservations the last reading does not hold yet, and `0% left now` once that time has passed. The reset and the
projection split over two lines where the column does not hold them on one, and the hover adds the burn behind the
projection, `the sessions on this account burn 8.5% an hour`. On a slowed account the 5-hour window's line ends in the
speed chip, `0.5x` in the warning tone (the speed the holds realized, to two decimals with a trailing zero dropped,
`0.35x`), whose hover reads `slowed since 17:40: the sessions on this account would pass the ceiling 42 min before the
reset, over the 30 min threshold` (`under` while no round can fire) followed by the factor the pool computed, how far
the span has ramped toward it where the entry is inside a span, and the speed the holds realized, one sentence each,
such as `The pool computed 0.5x. Ramped to 0.5x. The holds realized 0.5x`. The chip draws inside a slow-down span, while
the realized speed is under 1 and the 5-hour window the read came from has not reset since, and the Dashboard's account
cards, the Accounts view, the Pools cards and the Sessions view draw the same chip with the same hover
([Slow-down](routing.md#slow-down)). The window bars of the Dashboard's account cards and of the Accounts list draw the
projection alone in the reset's place, in a compact form (`19% left at reset`, `56% left in 5 h`) that splits between
the share and the horizon where the bar is narrow, with the reset's instant on hover, and keep the reset where the
window has no projection. An account whose plan ended reads `No window since the plan ended {ago}` in the bars' place on
both views, where a subscription account that is not paired or needs re-auth reads "Not paired, no window" or "Needs
re-auth, no window", any other subscription account without a window "Not read yet" and an API-key account "No quota
windows". A window the account's poll does not carry keeps its slot, so the bars line up across the cards, with "none"
muted in the share's place, since the slot is too narrow for the words beside its label, and the words on hover: "no
5-hour window" on an approved Ultra account, "no 7-day window", and "no Fable window" on a Pro account. The Accounts
detail's bars read the whole label and a missing window's words, split between its two parts where the bar is narrow,
and the 5-hour bar's label carries the chip on a slowed account. The Pools dialog's Preview, the Sessions Runway column
and the routing log keep the used share, the figure that each window's ceiling in a pool is measured against. A window
whose reset has passed with no reading since reads `expired {ago}` instead, with "no reading since the reset, {time}" on
hover, and draws no share and an empty meter until the next reading, and a window the account does not report keeps its
row with its words muted in the reset label's place and on the chip's hover ("no 5-hour window", "no 7-day window", "no
Fable window"). A window that has not opened, which the router reads at 0% used with no reset, reads "not open" in the
reset label's place with no projection beside it, on the card's row and on the window bars of the Dashboard's account
cards and the Accounts views, and its chip on the timeline reads "not open" in the share's place. A window with a used
share and no reset reads "not reported" there, followed by its projection where the window carries one, and the bars of
the Dashboard's account cards and of the Accounts list draw that projection in the word's place. Its chip on the
timeline keeps its share, with "no reset reported" on hover, and the timeline draws no bar for either window. A window
whose share the router does not hold reads "not reported" with an empty bar and stays out of the capacity ring, and an
account that holds no share on any of its windows stands last in a sort by share. A card with no window yet reads "No
quota windows reported yet.", and one whose read failed shows the error with `, next attempt {time}` and a "Retry"
button in the windows' place. A usage-based seat's Spend row reads the share of its monthly spend limit left,
`59% left`, over the meter, with the amount spent of the limit in the reading's currency on hover and `resets {ago}`
under it, and no pace, since the router projects no burn on the seat. With no limit set its chip reads the amount spent,
`$1,025.00 spent`, and no meter draws, and a seat with no reading yet reads "No spend reading yet." in the windows'
place. Until a reset header names the spend window's reset, the router estimates it as 00:00 UTC on the first of the
next month, and the line reads `resets {ago}, estimated`, the word in the muted ink, with
`resets {time} (estimated: no reset header has been seen for this window yet)` on hover. The reset label of every row
whose reset lies within 60 s of the card's soonest, the slack within which the router reads two resets as one, a seat's
Spend row among them, reads in the accent ink, its hover adding "the first of this account to recover". The "Extra
usage" chip reads `off`, `not in use`, `in use` or `not reported`, with the month's spend against its limit while extra
usage is enabled and the reading carries the amounts, in the serious tone while requests bill as extra usage, and the
provider's reason inside the line with the whole message on hover. The chips name the plan the router resolved from the
organization type and the tier ("Plan", reading "Free", "Pro", "Max", "Ultra" or "Usage-based", the display name
claude.ai sends where the account carries one, the tier on hover), when the plan of an account marked canceled ends (in
the serious tone inside a week), and when a trial ends ("Trial"). The poll reading says when the poll last asked the
endpoint and how it answered, with the whole message and the backoff on hover, and "Refresh" reads this account's
endpoint and its limit resets now. A card that shows an error, whose last poll failed, or that is backing off counts as
needing attention.

A card of an account that holds a one-time limit reset closes its chip row with a Resets chip. It reads the count of
resets left and "available" while one can be used now, as in `Resets 2 available`. Where none can, the reason of the
first reset left follows the count in the muted ink: "blocked by the 5-hour limit" (or the 7-day, the Opus, the Sonnet
or another limit), "not usable now", "nothing it can clear now", "on a cooldown until 15:10", "paused", "usable once at
a limit it clears" or "waits for the other reset". Inside the warning's days before the account's earliest reset ends,
the chip takes the tone of a plan about to end and, where a reset can be used now, adds "ends in 4 days", or "first ends
in 4 days" where the account holds several. The days are `usage.limit_reset_warning_days`
([installation](installation.md#usage)), and 0 turns the tone off. On a narrow card the chip shortens to `Resets 1 ends
in 4 days` or `cooldown until 15:10`. Its hover carries the whole words with the labels of the resets left and, inside
the warning's days, the instant the earliest one ends. A narrow card that also draws the plan's end and the trial leaves
the chip out. An account with no reset left draws no chip. A window that a reset usable now can clear carries a
counter-clockwise arrow in its chip unless it reads 100% left or has no live reading, and the chip's hover adds ", a
reset can clear it now".

A card with a reset left carries "Use reset" beside "Refresh". It is disabled while no reset can be used now, and its
hover then names the reason. A click reads the account's limit resets first, so the button reads "Reading…" for a
moment, and then opens a dialog, or reads the list again where no reset is left. Where that read fails, the dialog opens
on the stored resets under a line that says how old they are.

The dialog's first page, "Resets on primary", lists the account's resets, the usable ones first: the label, what the
reset clears, how many are left and the date to use it by, with the reason beside a row that cannot be used ("Used at
14:30", "Waits for the other reset"). An account whose only reset is usable opens on the second page. The second page,
the confirmation "Use reset on primary", names each window the reset clears with its reading now and what the use does
to it ("clears to 100% left", or "starts anew" for the 5-hour window), how many resets are left after, the date to use
it by, and that a used reset cannot be returned. A Fable reset also gives the 7-day window back its Fable share, and the
page prints the share left after as an estimate from the upstream's own figures. Where the windows read 100% left, the
page says the reset has nothing to clear yet. While the claim is on its way the button reads "Using…" and the dialog
cannot be closed.

![The reset dialog's first page: the account's resets](assets/quota-reset-pick-light.png#only-light)
![The reset dialog's first page: the account's resets](assets/quota-reset-pick-dark.png#only-dark)

![The reset dialog's confirmation](assets/quota-reset-confirm-light.png#only-light)
![The reset dialog's confirmation](assets/quota-reset-confirm-dark.png#only-dark)

After a use the dialog closes under the toast "Reset used", and the card reads the cleared windows. Any other answer
keeps the dialog open under one of two classes. "Refused" means the reset was not used, and the sentence says why. A
dialog that stayed open while its account was paired again, deleted and created again, or had a reset used from another
window reads "Refused", and a new click on "Use reset" reads the account as it is now. "Not confirmed" means the router
cannot tell: the upstream's answer leaves the use open, no usable answer came, or the router itself did not answer, so
the reset may have been used. The line after it reads the account's count once it is known. Read the card after a
refresh. Where the reset still reads as usable, "Use reset" is enabled again. After a "Not confirmed", a second try
inside ten minutes repeats the same claim under its request id, so it cannot spend a second reset where the first one
landed.

The address `/quota#<account id>` opens the Accounts section, scrolls to that account's card and focuses it, which is
where the Dashboard's Reset ending item and a wall that a reset can clear lead.

Quota windows lays the same windows across a calendar titled "Quota windows", with its span and length (`two weeks` in
the Weekly mode, `three days` in the 5-hour mode), "Previous period", "Today" and "Next period" buttons, the mode switch
Weekly or 5-hour, an "Account" axis and a now marker over the bars at the instant's place in the span. It draws one lane
per listed account with a window or a spend reading, and each lane carries the chips "5-hour limit", "7-day limit" and
"Fable 7-day limit", each with its share left and its reset on hover, and one bar per window that has a reset in the
account's color, the current bar filled from the window's start by its used share in a stronger tone, so a fill past the
now line is ahead of pace and one short of it behind, the upcoming window dashed and the elapsed ones muted. A
usage-based seat's lane carries one chip, "Spend limit", with the amount left of the limit (`$1,475.00 of $2,500.00
left`, or the amount spent where no limit is set) and the reset with the estimate's word on hover, and one bar per
monthly spend window, the current one from one calendar month before the reset to the reset, filled by the share spent
and labeled as a plan bar is. The current bar is labeled with the share left and its reset, `59% left · Sep 19 20:10`,
or with the share alone where the bar is too narrow for both, and a bar too narrow for its label keeps it on hover. On a
narrow screen each lane stacks its account and its chips above its bars, the now marker and the day grid cross the bars
alone, the marker's pill under the day labels, and the day axis prints short labels. A 5-hour cell too narrow for its
full labels prints the hour alone, `06` under the 24-hour time format and `6 AM` under the 12-hour one. Under the
12-hour format the 5-hour mode draws fewer, wider cells where twelve would be too narrow for `12 AM`. An account marked
canceled with a saved end shows the end on its lane: no window opens after it, and the window that spans it stops there.
The legend reads current window, upcoming, used, past window, now and plan ends, and the note "Each bar is one full
quota window, drawn from when it opened to when it resets, and labeled with the instant it resets. Lanes ending together
compete for the same days."

## Accounts

![The Accounts view: the list with the selected account's detail](assets/accounts-light.png#only-light)
![The Accounts view: the list with the selected account's detail](assets/accounts-dark.png#only-dark)

The bar above the list and the detail offers "Search accounts", Add filter with the Status filter ("All statuses" and
the six status labels, its chip reading such as "status: Paused"), the Sort control with Name (ascending by default),
5-hour reset, 7-day reset and 7-day Fable reset (each ascending, an account without that window's reset ahead last under
either direction), Most used (descending, an account with no plan window last under either direction) or Status
(ascending), and "Add account", and at the right edge the split Refresh button and the count of the rows the list shows,
`{n} accounts`. The search, the sort with its direction and the status live in the page's address, so a link or a reload
keeps them, and they stay while an account opens. Auto reload starts at 5 s and waits while a dialog or an editor of the
detail is open, or while its save runs, and while it is off the page reads the list every 2 s after a pairing's confirm,
with the same wait, until the account's first reading lands or 30 s pass. The six labels, with the state each stands
for, are Cooling down (`cooldown`), Needs funding (`funding_required`), Paused (`paused`), Needs re-auth
(`reauth_required`), Ready (`ready`) and Not paired (`unpaired`). A row reads the label with a badge that names the plan
the router resolved from the organization type and the tier ("Free", "Pro", "Max", "Ultra", "Max (tier unknown)",
"Team", "Enterprise" or "Usage-based", the display name claude.ai sends where the account carries one, none on an
API-key row), the status badge with the speed chip beside it on a slowed account, the email, `pool {id}` or "no pool",
"Overage" while requests bill as extra usage and "extra usage on" while it is enabled and unused. The pane reads "The
router holds no account yet" with "Add account" before the first account, "No accounts match" with "Clear the search or
the status filter." under a filter, and "No account to show" until a row is selected. What the states mean and how an
account moves between them is on [Pairing an account](pairing.md).

The detail opens on the account's label with "Edit label", its email and its badges: the status, `quarantined` while
the last refresh's outcome is uncertain, `cooldown ends {ago}`, "Overage active" and "Routing paused", with `Overage
since {ago}` under them while a sample reports the overage's start. An account whose refresh token was rejected
carries the callout "Routing is off for this account. The refresh token was rejected ({error}). Re-pair to restore
it." The actions, with the dialog each opens:

| Button | Dialog | What it does |
| --- | --- | --- |
| "Pair" or "Re-pair", on a subscription account | `Pair {label}` or `Re-pair {label}` | Opens the pairing dialog described below. |
| "Verify", on an API-key account | "Verify account" | "Sends a models request to the upstream with the stored key and reports the status.", and reads `Verify: upstream answered 200` on success. |
| "Pause" | "Pause account" | "Paused accounts stay in their pool but receive no new sessions. Pinned sessions migrate on their next request. The account's claude.ai features keep working where a pool or a key selects it." |
| "Resume", on a paused account | none | Resumes the account at once and reads "Resumed". |
| "Catalog" | `Catalog of {label}` | Opens the account's model list, described below. |
| "Clear funding flag", while funding is required | "Clear funding flag" | "Overage becomes eligible again on the next request. If the provider still refuses to fund it, the flag comes back with the new reason." |
| "Disconnect" | "Disconnect account" | `Deletes {label}'s credential. Pool memberships and request history stay, and re-pairing stores the credential again.` |
| "Delete account" | "Delete account" | `Removes {label} from its pool and deletes it. Request history stays in the ledger.`, below the account's uses where it has any. |

Delete account's dialog reads the pool and key lists as it opens and names, above its sentence, where the account is in
use: `{label} is in use:`, then one line per use, the pool it serves model requests in
(`It serves model requests in the pool {pool}.`), each pool that selects it for a [claude.ai
feature](claude-ai-features.md#whose-claudeai-a-key-acts-under) (`The pool {pool} selects it for {feature}.`) and each
key that names it for one (`The key {key} selects it for {feature}.`), the feature "Artifacts" or "Connectors". Where a
pool or a key selects it,
`Deleting it sets those selections to none, so the feature is refused there until an operator selects another account.`
follows the lines. A key that inherits its pool's account is no use of its own, and an account in no use shows the
sentence alone. Until both lists answered the dialog draws a placeholder in the uses' place and holds Delete. A failed
read draws "Could not load where this account is in use" with its error and Try again, which reads the failed list
again, and Delete stays held.

"Identity" lists, for a subscription account, Type ("OAuth login"), Plan, the plan the router resolved, with its start
(`Ultra since {date}`, and `Free` with no date on an account whose plan ended, the plan the router resolved once the
organization held none), Rate-limit tier (`{tier}, usage-based` on a usage-based seat, and on an account whose plan
ended the tier the profile read found), Ends where known, Plan ended where set, with its instant, Trial ends where
known, Organization and Billing type, and "Edit end" opens the end editor: the checkbox "Canceled", the field `Ends
({zone})`, whose label names the browser's time zone, with the hint "The date the plan's billing settings show as its
end. An annual plan is marked canceled in its final month only. The time is in this browser's zone.", Save and Cancel,
and "Reload" where the row changed under the editor. Beside the saved end the row answers `plan_ended_at`, the instant a
profile read found the account's organization holding no plan, which the router reads as an end that has passed until a
profile read or a pairing finds a plan again. A pairing whose profile still reports no plan keeps the flag, and the
Quota view leaves the account out while the flag stands ([Account states](pairing.md#account-states)). The card closes
on "Renewal date is not reported by the provider." For an API-key account it lists Type ("API key") and Windows ("None
reported, billed per token"). On either kind the rows end on Companion id, the pseudonym the companion mod's `allowance`
tool names the account by ([The companion routes](relay.md#the-companion-routes)). "Usage" ("Remaining share of each
window") draws the three windows as the Quota view does, or a usage-based seat's spend bar, "Spend limit" with
`{used} of {limit}` over its meter and its reset line, the speed chip beside the 5-hour bar's label on a slowed account,
"Extra usage this month" on every account but a usage-based seat, with an "in use" badge and its amounts and spend meter
where the reading carries them, the reason the provider gave for disabling extra usage, and a "Fast mode" line with the
share of the fast lane's input and output buckets used and a chip while the fast lane cools, or reads "Not paired, so no
quota window is read." or "Needs re-auth, so no quota window is read." on a subscription account in that state, where
the router withholds the windows until the account pairs again, "This account does not report quota windows." ("No spend
reading yet" on a usage-based seat, or "No quota window is read since the plan ended {ago}." on an account whose plan
ended), or "The first quota reading is in flight." from a pairing's confirm that started the account's first poll until
its first reading lands, where a usage-based seat reads "No spend reading yet". On an account whose plan ended the
plan-ended sentence stands first, and it stands too on a subscription account that is not paired or needs re-auth and
whose canceled end has passed. "7-day trend" ("Utilization of each window, sampled every 5 minutes") draws the 7-day
window of a subscription account, its Fable 7-day window where the account carries one or the week holds a Fable sample,
and its 5-hour window only where the week holds a 5-hour sample, each line ending at its own last sample. A week with no
sample reads "No samples yet" with "Quota samples appear after the first successful quota fetch.", or "No samples in the
last 7 days" over the Usage card's sentence on an account whose plan ended, that is not paired or that needs re-auth. A
usage-based seat's detail draws no trend card.

"Allowance left at reset" draws, for a subscription account that is not a usage-based seat, one column per weekly window
(the 7-day window, then each model week): the share left unused at the latest reset with "left at the reset of {date}"
("left at {date}" and the badge where that reset is the term's end or a plan change that cleared the window), the three
weeks before it with their figures, and "Total, {n} weeks", each week's hover naming its reset's instant and where its
reading came from. The block shows the rows of the card "Unused allowance by account", a used limit reset that cleared
its window among them with a "reset used" badge on its row, so its four weeks count the natural resets and add each use
newer than the oldest of them, including one after the latest, and the total's label names the uses, such as "Total, 4
weeks and 1 used reset". A plan change that cleared the window stands among the resets and counts as one of the four
weeks, dated by its drop, with a "plan changed" badge on its row and "a plan change" on its hover. The figures read in
the week the week control names under every position: the share of the account's own window times its tier's 7-day
capacity, a model week at its capacity factor, over the chosen week's tier's, so a Fable-week reset reads half its 7-day
figure. Each week prices at the tier the account ran under at that reset, so a week before an upgrade reads its figure
at the earlier tier's capacity, and a reset the router recorded no tier for at the account's current one, a dash where
the cost model holds no capacity for the tier a week prices at (a Free plan among them, the card then noting it), and
the total a dash where one of its weeks reads one. Each week's hover adds the reset's own share of the account's window
under the plan that week prices at, such as `Ultra: 39% of the 7-day window left`. The block waits on the cost model,
which the page reads from the settings once per visit whichever account opens, so it draws its placeholder until the
settings answer and the read's error where they fail. A week the account's term ended inside counts as a reset with the
badge "term ended", a clock-walk reset reads the last reading before it as the event carries it, and the card reads "No
weekly reset recorded yet. The first shows once this account's week ends." before the first. The card reads its weeks
once per account, again on the bar's Refresh, once a weekly window resets and once the account's term end moves (its
cancellation flag, saved end or plan-ended flag), and a failed read draws its error with Try again, which reads the
weeks and the settings again.

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
the last hour. Lifetime counts its rows older than the week from a read made once an hour, so a request older than a
week that lands late joins the figure within the hour.

"Catalog" opens `Catalog of {label}`: `As of {ago}` with "Refresh", and the models as Model, Name, Released, Input
tokens, Output tokens and Effort levels, or "The upstream lists no models." A failed read draws its failure, and a
failed Refresh draws it over the list it kept, until a read answers.

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

The view opens on its bar, the Sort control with Name (ascending by default), Status (ascending), Members, "Requests,
24h", "Cost, 24h" (each descending, Cost under Native ranking by the day's dollars at the API price list, the All in
dollars figure) and Active pins (descending), then "Create pool" with the split Refresh button and the count `{n} pools`
at the right edge, then "A pool groups accounts behind a set of keys. Requests pick a member by priority, stick to it
per session, and fail over when a member hits a rate limit." on a line of its own, then the tiles Pools, Accounts in
pools, Keys, "Requests, 24h" and "Cost, 24h". The cost tile's label names the position under Dollar equivalent and
Billed dollars, as the Dashboard's cost tile does, and so does each card's Cost 24h row. The sort and its direction live
in the page's address, and a card's links into the pool dialog keep them, so the dialog closes onto the same order. A
card's member rows keep the selection order whatever the sort. Auto reload starts at 5 s and waits while the pool dialog
or the Delete pool confirm is open. The sessions and the keepalive overview the pool dialog draws are read when it opens
on a saved pool and again after a key write from it, the overview also when the return rate's range is picked, never at
the interval. "Cost, 24h" and each card's Cost 24h read in the top bar's unit: under Native the day's subscription
traffic as its figure in the chosen week over its billed dollars, a side with no traffic left out and the subscription
side a dash with the Dashboard's hover where a request of the day priced nothing. The billed dollars count a billed
keepalive probe's, while the request counts leave the probes out. A pool sums its members' accounts, so its figure
carries no hint. Every field's meaning is on [Pools, keys and routing](routing.md#pools).

Each card carries the pool's name, its status badge (active, degraded, disabled or exhausted), its keys as a link ("No
keys yet", "1 key", `{n} keys`, or "Keys unavailable" when the keys read failed), an edit button and "Delete". The edit
button and "Delete" stay together at the row's right, and on a phone's width where the name, the badge and the keys link
leave them no room, both move to a line of their own at the right. A strip of badges names the features the pool has on
and the state of its load shedding, in the rail's order: `1 guardrail` or `{n} guardrails` with any attached,
"server-side auto mode off", `sheds above 95 % (5-hour) and 100 % (weekly)` (the pool's two ceilings) with the pool's
load shedding on or `shedding off` with it off, `slows above 2 h 30 min` (the pool's stretch threshold as a span, `slows
above 30 min` on a pool set to 1800) with its slow-down on, "cache TTL" and "keepalive", each opening the dialog on the
group that edits it, the last followed by the pool's resolved ceilings in whole hours,
`idle {h} · parked {h} · horizon {h}` (a range such as `44h to 71h` where the pool's combinations differ). One
sentence says what the status means, and attention rows under it list the dashboard's conditions filtered to this pool,
one row per condition with its badge, its subject linked where it is fixed and its detail, ending with its age, the
whole on hover. On a narrow card the detail and the age take a line of their own under the badge. The pool's own
exhausted row and its Rounds without a destination row open the [routing log](#routing-log) on its exhausted decisions
or its shedding rounds since the condition began, and an account's Destination spent within the hour row opens it on the
account's moves and spent windows, its Wall reached row on its spent windows, and its Slowed past twice the stretch row
on its slow-down starts and ends. An account's Meter drift against the table row opens the [Meter](#meter) view on the
account. An account's Reset ending row reads `a reset ends in 4 days (Sep 21, 2026, 02:30:00) and clears 5h and Fable:
Launch reset`, the end, the windows the reset clears and its label, and opens the account's card on the [Quota](#quota)
view, as does a Wall reached row whose words carry `, a reset can clear it`.

The serving table lists who serves next: Tier (`P{n}`, or `plan P{n}`, `overage P{n}` and `metered P{n}` where the pool
prefers subscription accounts, the metered tier holding the API-key accounts and the usage-based seats), Account,
Standing and Pins. A tier's eligible members stand in the order of the fit that a fresh session reads on each under the
regime the card's Regime control names, which starts at the interactive regime where the cost model in force names one
and at the base regime where it does not. That is the order the router lands such a session by: the members that fit,
then `rewrite only`, `least bad`, `winding down` and `window spent`, the longer runway first among the `rewrite only`
and among the `least bad` members, and the strategy's order after that. Standing reads `next` alone for the member a
fresh session lands on, `eligible`, `after plan quota` for a member in overage ranked after the plan-funded ones,
`winding down {left}` (on hover `Winding down to the plan's end at {time}`) for a member inside the pool's wind-down,
and for a member passed over `cooldown {left}` (on hover `In a cooldown that ends {time}`), `overage until {time}`
(`overage` where the reset is unknown), `spend cap until {time}` (`spend cap reached` where the reset is unknown) for a
usage-based seat at its spend cap, `at budget` or `at budget until {time}` with the spend against the budget, `budget
unknown` while the ledger is degraded, `subscription ended` for a member whose subscription ended, `unavailable` with
the account's status on hover, and `unknown` (on hover `The router has not loaded this member, or does not answer`) for
a member the router answers no reading of. After the standing word, a member on which a fresh session's burn would carry
a shared window past its ceiling before the window resets reads `rewrite only`, titled "A fresh session's burn would
carry a shared window past its ceiling before it resets", and a member whose shared window stands at or past its ceiling
reads `least bad`, titled "A shared window stands at or past its ceiling", both in the warning tone. A member whose
5-hour or 7-day window stands at 100 % or more reads `window spent` in the same place in the serious tone, titled "A
shared window has no allowance left", a ground of its own beside a seat's spend cap. The row's hover reads "{label}:
{word}, taken only once no account fits a fresh session" for `rewrite only` and `least bad`, "{label}: winding down,
taken only once no other account can take a fresh session" for a member inside its wind-down, and "{label}: window
spent, taken only once no other account has allowance left" for a spent one. `next` stands on the best of these rungs
that an eligible member reaches. On the rung of the members that fit, of `winding down` or of `window spent`, it is the
lead member of the first tier that holds one, so a pool whose eligible members all read `window spent` still names one.
On the rung of `rewrite only` or of `least bad`, it is the member of that rung with the longest runway across the pool,
whatever its tier, and the first tier's lead where several share it. On a card whose pool has the slow-down on, Standing
carries the speed chip of a slowed member, `0.5x` with the Quota view's hover where the account is loaded. The caption
names the strategy, `Pace`, `Priority, then random` or `Round robin`, with `subscription accounts first` appended where
the pool prefers subscription accounts, and its hover notes "A request for a Fable model is placed on the Fable window
instead, which this card does not draw." Pins reads the member's active pins, one per agent and model as the card's
Active pins fact counts them, a muted `0` for none. Then "Traffic, 24h" draws requests per hour with a 3-hour average,
or "No traffic in the last 24h", and the facts Members, Keys, Requests 24h, Cost 24h, Active pins, Warnings and Moves in
the last hour. The card's request figures, the count in the chart's head, the bars per hour and Requests 24h, leave out
the requests the router refused before any account was asked. Active pins counts every pin under the pool's keys, as the
Dashboard's Sessions card does. A pin outlives its account's membership, so where the figure exceeds the sum of the Pins
column, a line under it reads `{n} on a former member`, or `{n} on former members` for more than one. Warnings counts
the pool's warnings with their sentences on hover, and its figure opens the dialog on the group of the first warning's
first field. The figure of Moves in the last hour opens the routing log on the pool's moved decisions, and its hover
lists the moves by reason. "Delete" asks `Delete pool {name}`: "Deletes the pool and its memberships. Its keys must be
moved or revoked first."

The pool dialog, "Create pool" or `Edit pool {name}`, is grouped: a rail "Pool settings" on the left, one group open
at a time, a status line reading "Every group saves together." or `Save is held: {reason}` (a pool that changed since
you opened it holds the save until "Reload"), and Cancel beside "Create pool" or "Save pool". On create the rail marks
Identity "needed" and Keys and Guardrails "after saving", and the Keepalive group's "Keep prompt caches warm" switch
starts off. A pool can be saved with no member, and while it is enabled its card reads "Exhausted: the pool has no
members yet." The groups by rail parent, with what the expert view adds:

| Rail parent | Groups | In the expert view alone |
| --- | --- | --- |
| Pool | Identity, Members, Keys, Models, Guardrails, Auto mode, claude.ai features | Auto mode |
| Routing | Selection, Placement, Pinning | Placement, and Pin sessions under Pinning |
| Load | Windows, Load shedding, Slow-down, Failover, Overage and plan end | Slow-down, Failover, Burn average (s) under Windows, Preset, Shed cap (%), Shed dwell (s), Warm sessions, Fable reservation shedding and Preview under Load shedding, and Overage grace (min), Return to plan, Return dwell (s) and Wind down (min) under Overage and plan end |
| Caching | Cache TTL, Keepalive | Add a cache marker to a request without one and the per-model overrides, both under Cache TTL |
| Visibility | Body capture, Diagnostics | Diagnostics, with its two switches Cache diagnostics and Thinking diagnostics |

Selection's Strategy field explains the chosen strategy under it. Under Pace it reads "Pace scores each member by its
spare rate: the allowance left in the window it reads, the model's own 7-day Fable window where the model has one, else
the 7-day window, per hour until that window resets or the subscription ends, less what its sessions burn of that window
an hour, within the first priority tier that has an eligible account. An API-key member scores the room left in its
worst bucket. An unsampled account counts as untouched, and ties are random. On the 7-day window, Pace's allowance first
loses the member's Fable reservation, the share its expected Fable use is to take of it. Each account's windows are on
the Accounts page."

Windows opens on "The windows the router works from: the share of each it keeps every account under, and the span each
session's burn is averaged over." Its first row holds `5-hour ceiling (%)` and then `Weekly ceiling (%)` side by side in
every view, each from 50 to 100. The first's hint reads "A move or a new session lands on a member only where its 5-hour
window stays under this share until it resets. The slow-down paces a member's 5-hour window to it." and the second's
"The same line for the 7-day window and each model week, read over the next five hours. A round runs on a member whose
weekly window would pass it. At 100 a week is used up to its wall. Lower it for accounts with extra usage on or used
outside the router." The expert view adds `Burn average (s)` on the next row, from 600 to 7200, with the hint "The time
constant of each session's burn average. The projection, the slow-down and the round read it. A 5-hour window younger
than twice it fires no round. A slowed member's holds reach their full length over it. Editable under the Custom preset
in Load shedding." It closes on the "Member utilization" bar, whose hint reads "Each tick is a member's latest 5-hour or
7-day reading, as on the Quota page. Drag a ceiling's handle, or focus it and use the arrow keys." The bar holds a track
of used shares for each kind of window, the 5-hour readings under the "5-hour ceiling" handle and the 7-day readings
under the "Weekly ceiling" handle, a tick per member window with the hover `{account}: {window} {used}` and the legend
"5-hour reading" and "7-day reading". Each handle stands on its track in every view, dragged or moved with the arrow
keys, with its own field following it.

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
ceiling, up to this.", `Shed dwell (s)` and Warm sessions, then the switch `Fable reservation shedding`, which is off by
default and can be set under every strategy and with load shedding off, with the hint "On, under the Pace strategy, a
round also runs on a member whose 7-day room is smaller than its Fable reservation. It marks non-Fable sessions alone,
up to the shed cap.", and closes on a "Preview" line, the round's dry run as the fields stand. `Burn average (s)` sits
under Windows and `Migrate when spent` under Failover, which opens on "How long a request waits out a throttle on its
member, how many requests one member serves at once, and whether a spent window moves the session at once." The Preview
replays the trigger from the projections the accounts route serves for each member and window and from the burn of the
pool's pins, which the dialog reads from the router when it opens, an estimate of what the router reads. It walks each
pin under the pool's keys, as the router's round does, a pin that the Sessions view no longer lists among them, leaves
out a pin on the fast lane, a pin that reads ended and a pin that holds state on its account, and orders the rest and
picks among the pins that cover what the round's target still asks by the figures the router's own round reads for each
pin's move, its size and its cost, while the Sessions view keeps drawing an ended agent's line. The figures are those of
a round that the pin's own next request fires, so a round that another pin's request fires can order the pins another
way. The Preview reads each pin's destination with no planned booking, as a round's order and its first mark do, and a
round's second or later mark reads the destination again with the bookings its walk planned, so the Preview is exact for
a round's first mark and an estimate behind it. On a pool with the slow-down on, the Preview tests a member's 5-hour
window against the 5-hour ceiling by the figure of the router's stretch test, the projection at the window's horizon
less the stretch threshold. It reads no 5-hour round for a member whose account carries no such figure, and none once
the window has the stretch threshold or less left to its horizon, where the router's test fires nothing.

With the switch off the Preview opens on `Load shedding is off: no round runs.` and names the projection alone. It reads
`No member projects over the ceiling on any window: the closest is {label}, {window} {p} {horizon}.` while no member's
projection passes its window's ceiling, the closest being the member window with the least room under its own ceiling,
and otherwise `1 member projects over the ceiling: {label}.` or `{n} members project over the ceiling: {list}.`,
followed by `A round there would mark {k} sessions ({warm} warm), {excess} of the excess, to move to a sibling with
runway.` (`1 session (1 warm)` for one), by `No sibling has runway for them, so nothing would be marked.` where a
session the round's walk reached has no sibling with runway for it, or by `A round there would mark no session:` with
the first reason that holds, `each is too small to move.` where a session the walk reached is a sliver under the round's
mark share, `none is priced.` where one it reached has no priced burn, `each would leave its member too little burn.`
where the round's guard keeps one, `the sessions already marked cover it.` where a session stands under a mark already,
or `each is warm.` where Warm sessions is never and every session the walk holds that burns is warm. Where a round walks
no session, the reason says why each was left out: `none draws that window.`, `each agent has ended.` or `each holds
state on its account, which a move would lose.`, then `none burns.` where the sessions burn nothing on the window.
Otherwise the reason is `none is priced.`, as where the members over the ceiling hold no session or while the Shed cap
(%) field holds 0. With `Fable reservation shedding` on, under the Pace strategy and with load shedding on, the line
adds, for the members no window fired on whose 7-day room is smaller than their Fable reservation and that hold a
session of another model that a round can move, `1 member holds less 7-day room than its Fable reservation: {label}.` or
`{n} members hold less 7-day room than their Fable reservation: {list}.`, followed by `A round there would mark {k}
non-Fable sessions ({warm} warm), to move to a sibling with runway.` (`1 non-Fable session (1 warm)` for one) or by `A
round there would mark no non-Fable session.` A member under its reservation with no such session runs no round, and the
line reads `{label} is under its Fable reservation, and no session of another model is there to move.` Otherwise the
line says nothing of the reservation. A member whose 5-hour window is under the hold is read on its next window in the
order, and where its 5-hour projection reads over the ceiling, the line names the round the hold took away: `{label}'s
5-hour window is {age} old, under the hold of {hold}, so its round reads the next window.` `{p}` is a whole percent with
a space before the sign (`99 %`), `{horizon}` `at` a clock time on the 5-hour window and `in` a span on a weekly one
(`in 5h`), `{window}` the window's label (`5h`, `7d` or `7-day Fable`), `{excess}` a percent an hour with one decimal
(`9.5 %/h`), `{list}` the members in the Members group's order, and `{age}` and `{hold}` spans to the minute (`45 min`,
`1 h`).

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
it is saved. The card's Warnings fact counts them. The twelve checks, by code:

| Code | Holds when | Sentence |
| --- | --- | --- |
| `affinity_under_cache_ttl` | the cache TTL policy is on and adds a marker to a request without one, and `affinity_ttl_seconds` is under the longest TTL the policy sets | "The session affinity ends before the cache TTL the pool injects, so a pin can expire while its cache is warm and the next request can land cold elsewhere." |
| `cap_under_cohort` | `in_flight_cap` is under the most agents of one session among the pool's live pins | `The in-flight cap is under the largest live cohort of {n} agents, so subagents of one session wait for a slot they share.` |
| `cap_under_quarter` | `shed_cap` is under 0.25 | "A shed cap under 25 percent removes at most a quarter of the burn per round, so a window far over the ceiling needs several rounds and dwells to come under it." |
| `five_hour_ceiling_low` | `five_hour_ceiling` is under 0.8 | "A 5-hour ceiling under 80 percent moves sessions that would have finished the window warm." |
| `five_hour_ceiling_without_margin` | `five_hour_ceiling` is 1.0 | "A ceiling at 100 percent leaves no margin for a stale reading or a large turn, so a session can reach the wall before the round or the gate reads it." |
| `hold_over_window` | `slow_down_hold_seconds` is over 150 | "A hold over 150 seconds leaves under 30 seconds of the client's first-byte window for the upstream's head, so a slowed request can time out at the client and be sent again." |
| `reservation_shedding_idle` | `fable_reservation_shedding` is on, and `strategy` is not `pace` or `load_shedding` is off | "The option to shed on the Fable reservation does nothing here, since its rounds run on a Pace pool with load shedding on." |
| `return_at_once` | `return_to_plan` is `always` and `return_to_plan_dwell_seconds` is 0 | "A return dwell of zero returns every warm session on its first request at the metered member, so the member serves one turn per session and the move's rewrite is paid each time." |
| `shedding_off` | `load_shedding` is off and `slow_down` is off | "With load shedding off no round marks sessions on a member whose window would pass the ceiling, so a session stays until its window is spent or its account refuses it." |
| `spent_moves_off` | `migrate_when_spent` is off | "With spent moves off a session stays on a spent account until the upstream refuses it, so the destination gate never runs for that move." |
| `warm_never` | `warm_sessions` is `never` | "With warm sessions never a round stops after the cold sessions, so the excess is covered only while cold sessions carry it." |
| `weekly_ceiling_low` | `weekly_ceiling` is under 0.8 | "A weekly ceiling under 80 percent leaves a fifth of every week unused." |

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
age lists the pool's ten buckets over the last 7 days, or 30 under the choice beside its label, which opens on 7 days,
with the windows that returned out of those that reached it and a bar of the rate, or "No idle windows recorded yet". A
failed read draws `Could not load the return rate: {error}` above the buckets an earlier read of the same range drew, or
on its own when there are none. The ceilings are explained under [The four fields](keepalive.md#the-four-fields) and the
buckets under [The return rate by idle age](keepalive.md#the-return-rate-by-idle-age).

claude.ai features shows in every view and holds the selection that [Whose claude.ai a key acts
under](claude-ai-features.md#whose-claudeai-a-key-acts-under) explains. It opens on "Select the account under whose
claude.ai each key of this pool publishes artifacts or uses connectors. Any OAuth account of the router can be selected,
a member of this pool or not. A feature call runs under the selected account whatever its windows read and while it is
paused. It is refused while the account is signed out or past its plan's end." and holds two selects, Artifacts and
Connectors. Each lists "None (refused)", a new pool's choice, which refuses the feature's calls, then every OAuth
account of the router in the order of the labels, as `{label} ({pool})` or `{label} (no pool)`. An API-key account is
not listed, and an account whose stored sign-in lacks the feature's scope is listed disabled as
`{label} ({pool}), holds no sign-in with this feature's scope. A new pairing adds it.`
[Pairing an account](pairing.md#two-kinds-of-account) names the scope each feature needs. An account that cannot
serve a feature call is listed disabled with its reason as well, as
`{label} ({pool}), needs re-auth. Feature calls are refused until it is paired again.`,
`{label} ({pool}), is not paired. Feature calls are refused until it is paired.` or
`{label} ({pool}), is past its plan's end. Feature calls are refused.` The scope's reason wins where both hold. A
stored choice of a disabled account keeps its option, which the select draws as its value. The selects wait for the
account list, a failed read of it draws "The account list did not load" with "Try again", and a stored choice the list
does not hold reads `{id} (not listed)`. A change in Members leaves both choices as they are, so an account taken out of
the pool stays selected and reads `(no pool)` at once. Under a choice whose account cannot serve, a line says why in the
Accounts view's words for its state: `{label} needs re-auth. Feature calls are refused until it is paired again.`,
`{label} is not paired. Feature calls are refused until it is paired.`,
`{label}'s organization needs funding. Feature calls are refused until then.` or
`{label} is past its plan's end. Feature calls are refused.` Under a paused account the line reads
`{label} is paused. The pause stops its model requests, and its claude.ai features keep working.` Each save sends both
choices.

![The pool dialog's claude.ai features group with an account per feature](assets/pools-features-light.png#only-light)
![The pool dialog's claude.ai features group with an account per feature](assets/pools-features-dark.png#only-dark)

## Keys

![The Keys view](assets/keys-light.png#only-light)
![The Keys view](assets/keys-dark.png#only-dark)

The view opens on its bar: "Search keys", Add filter with the Pool and Status filters, the Sort control with Name
(ascending by default), Created, Last used, "Requests, 7d", "Tokens, 7d" and "Cost, 7d" (each descending), Expiry and
Status (each ascending), "Create key", and at the right edge the split Refresh button and the count of the keys the
table lists, `{n} keys`. Status offers All statuses, Active, Disabled, Expired or Revoked. Pool takes any number of
pools, each as a chip of its own, such as "pool: jobs" and "pool: all", and a pool the address names that the router
does not list keeps its chip until the pools load. The search, the filters and the sort with its direction live in the
page's address, so a reload keeps them and the pool card's key count link lands on its pool's keys. Under either
direction a key never used files last under Last used and one that never expires last under Expiry. Under Native "Cost,
7d" ranks by the week's dollars at the API price list, the All in dollars figure, so a key of billed traffic alone sorts
by its dollars. No column shows a key's creation date or its requests of the week, so while Created or "Requests, 7d" is
picked each row reads `created {date}` (the time on hover) or `{n} requests, 7d` under its name. The sort orders the
table alone, and the card's bars keep their order by cost. Auto reload starts at 5 s and waits while a dialog or a row's
menu is open. The tiles Keys, Active, Lifetime requests and Lifetime cost follow, and the card "Lifetime cost by key"
("Relative spend across all keys, in dollars at the API price list" under Native and "Relative spend across all keys"
under a dollar position), a bar per key. The cost tile's label names the position under Dollar equivalent and Billed
dollars, as the Dashboard's cost tile does. The lifetime tiles and the card's bars count their rows older than the week
from a read made once an hour, so a request older than a week that lands late joins them within the hour. Lifetime cost,
each bar's figure and the cost line under a key's "Usage, 7d" read in the top bar's unit: under Native the tile stacks
the subscription traffic's figure in the chosen week over the billed traffic's dollars, and a bar's figure and a cost
line read a key's figure beside its billed dollars, a side with no traffic left out, the tile, the bar's figure and the
cost line each reading the subscription side as a dash with the Dashboard's hover where a request priced nothing, while
its bar keeps the length and the place its dollars give it: under Native the card's bars rank by each key's dollars at
the API price list, the All in dollars figure, and the figure beside a bar keeps its two native sides. The billed
dollars count a billed keepalive probe's, while the requests and the tokens leave the probes out. A key spans its pool's
accounts, so its figures carry no hint. The columns are Name, Prefix, Pool, Models, "Usage, 7d", Expiry and Status, and
each row ends in its menu. Under the name a row notes the capture setting, `bodies captured, {h}h` or `bodies not
captured`, with "inherited" where the key follows its pool, and `allowance off` or `cache warming off` for each
companion permission the key lacks ([The key's permissions](companion-mod.md#the-keys-permissions)). Models reads `{n}
rules`, with "(replace)" where they replace the pool's, or "Pool rules" or "No rules". Expiry reads the date or "Never".
Status carries the key's state with `used {ago}` and, after a rotation, `{n} sessions still on the previous secret`. The
menu offers Edit, Rotate (`Rotate {name}`), Revoke (`Revoke {name}`: "Requests with this key are rejected immediately.
Its history stays in the ledger.") and Delete (`Delete {name}`: "Removes the key and detaches its request history from
the key name. This cannot be undone."). Before the first key the table reads "No keys yet" with "Create key". What a
key's settings mean is on [Keys](routing.md#keys).

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
"Manage guardrails", reading "Guardrails can be attached once the key is saved." on create), claude.ai features
(Artifacts and Connectors, below), Sessions (the key's two companion permissions as toggles, Allowance tool with the
hint "The model may read the pool's allowance through the companion." and Cache warming with "The router warms this
key's sessions' caches.", both on for a new key) and Capture (Capture request and response bodies as Inherit from pool,
an override in hours or off for this key). Server-side auto mode, Pool rules and List [1m] twins show in the expert
view. While the key captures, the Capture group warns what the bodies hold, the warning [Keys](routing.md#keys) quotes.

claude.ai features holds two selects, Artifacts and Connectors, the key's side of the selection that [Whose claude.ai a
key acts under](claude-ai-features.md#whose-claudeai-a-key-acts-under) explains. Each lists `Inherit ({label})`, a new
key's choice, which follows the pool's selection and names its account, or `Inherit (None (refused))` where the pool
selects none, then "None (refused)", which refuses the feature's calls for this key, then the accounts the pool dialog's
group lists, in its order and with its disabled options. The first option follows the pool the dialog shows, so a change
of Pool changes that text and no choice, and a key moved to another pool keeps an account it names and a "None
(refused)". The selects wait for the account list and for the key's pool, a failed read of the account list draws "The
account list did not load" with "Try again", a named account the list does not hold reads `{id} (not listed)`, and a
pool's account the list does not hold reads `Inherit ({id}, not listed)`, neither with a line under it. Under a named
account, and under Inherit for the pool's account, the line of the pool dialog's group names the account's state: why it
cannot serve, or that its features keep working while it is paused. Each save sends both choices.

A saved key's dialog shows its Effective list under the Models group: the list the key is judged by, top to bottom, with
the level each entry comes from and, where accounts cool for a model the entry names, a cooling count whose hover names
them.

## Guardrails

![The Guardrails view: a preset's guardrail observing beside an enforcing one](assets/guardrails-light.png#only-light)
![The Guardrails view: a preset's guardrail observing beside an enforcing one](assets/guardrails-dark.png#only-dark)

The view opens on its bar: the Sort control with Name (ascending by default), Mode (ascending), "Matches, 24h", Created
and Last updated (each descending), then "Create guardrail", whose menu offers "Blank guardrail" and, under the heading
"From a preset", one row per preset the build ships, three rows in the order of their ids (Away recap, Prompt
suggestion, Subagent status summary), then at the right edge the split Refresh button and the count `{n} guardrails` (`1
guardrail` for one). Under it one sentence: "A guardrail checks each request against its conditions before any account
is selected. Observe records the matches, enforce refuses them with a 403." "Matches, 24h" orders by the observed and
blocked counts together, a row whose Matches cell reads its dash last. No column shows a guardrail's dates, so while
Created or Last updated is picked each row reads `created {date}` or `updated {date}` under its name, the time on hover.
The sort and its direction live in the page's address, so a link or a reload keeps them. Auto reload starts at 5 s and
waits while a dialog or a row's menu is open. The pools and keys the dialog's pickers list are read once per visit and
again after a write, a dialog close and Refresh, never at the interval. The columns are Name (the description muted
beneath), Mode (a badge `observe` or `enforce`), Enabled (`on`, or `off` muted), "Applies to" (`{n} pools, {m} keys`, `1
pool, 0 keys` in the singular, the names on hover, `nowhere` muted when none), "Matches, 24h" (`{observed} observed`
over `{blocked} blocked` from the last day's counts, a dash while unread) and Preset (the preset's name with `v{n}`, a
`customized` badge where the conditions left the stamped version, an `update available` badge where the build ships a
newer one, a dash without a stamp), and each row ends in its menu: Edit, Duplicate, Enable or Disable, "Switch to
enforce" or "Switch to observe", and Delete. Duplicate opens the dialog in create mode with the source's fields, the
name `{name} copy`, no attachment and the preset stamp kept, so the copy does not run twice on the source's keys and
still says where it came from. Delete asks `Delete guardrail {name}`: "Removes the guardrail from every pool and key it
is attached to. Requests it matched keep its id in the log." Before the first guardrail the table reads "No guardrails
yet" with "Create guardrail". What a guardrail is, the presets and the conditions are on [Guardrails](guardrails.md).

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

A section bar shows one of two sections at a time: Model requests, where the view opens, and Feature calls, which a link
or a reload with `#feature-calls` lands on. The bar stays at the top as the page scrolls, and the request drawer docks
under it. Model requests is the request list this part describes up to [Feature calls](#feature-calls).

The filter bar holds the range, the search, Add filter, the Columns button, the Sort control, and at the right edge the
split Refresh button and the count of rows shown ("25 shown, more available"), the right edge on a second line at 1280
px whatever the count reads, since its slot keeps the width of its longest text. The range offers Last hour, Last 6
hours, Last 24 hours (the default), Last 7 days, Last 30 days or Custom range with From and To, drawn as the pills 1h,
6h, 24h, 7d, 30d and Custom, a pressed preset reading `{label}, from {from}` on hover. Custom opens From and To in a
popover on the window the table shows, where Apply stays disabled under "End must be after start." while the pair is
wrong, an end past now reads as now, and the pill prints an applied span ("13:00 to 14:30", the date before each time
across days). The search, "Search id, session, key, account, model, error, message id", matches the request id, the
session, the key, the model, the error code, and an attempt's account, upstream request id or upstream message id,
literally (`_` and `%` are characters), and applies 400 milliseconds after the last keystroke, on Enter, or when the
field loses focus. Add filter opens a "Filter by" field over the filters in four groups: Routing (Account, Key, Model,
Pool), Outcome (Cache miss reason, Dropped thinking, Fallback, Guardrail, Held only, Outcome, Refusal category, Status,
Stop reason), Request (Agent type, Classifier, Client, Effort, Kind, Request class, Speed, Turn origin, User agent) and
Subject (Agent, Request, Session). Choosing one opens its values: a list with a Select all row that applies on each
click (checking it checks every option and unchecking it clears every check), a pattern field for Client and User agent
(`claude-cli/2.1.*`, `*sdk-cli*`: `*` for any run of characters, `?` for one, a backslash quoting either, and a value
with no wildcard matching in full), an id field for Agent, Request and Session, which match the id exactly (an agent id
is `main` or a subagent's id, carried in the address as `agent_id`), the Kind choices (Any kind, Inference, Count
tokens, Probe) or the Fallback choices (Either, yes for the requests that presented a fallback credit token and no for
the rest, the chip reading "fallback: yes" or "fallback: no"), while Held only applies at once. Classifier offers client
classifier (`client`, the client's own auto-mode classifier call on the account its judged turn ended on), client
classifier off (`client_off`, one that ran off that account or with no turn to judge) and `none` for every other
request, its chip printing the word. Guardrail lists the registry's guardrails by name, then the ids of deleted
guardrails the loaded rows still carry as `{id} (deleted)`, then `none` for a request no guardrail matched
([Guardrails](guardrails.md)). Stop reason lists the stop reasons the console names (`end_turn`, `max_tokens`,
`model_context_window_exceeded`, `pause_turn`, `refusal`, `stop_sequence`, `tool_use`) and any other the loaded rows
carry, Refusal category the categories it names (`cyber`) and any other the loaded rows carry, each list ending on
`none` for a request whose serving attempt carries no value or that no attempt served. A pattern or an id applies on
Enter or Apply. Each filter in force stands as a chip on a line under the bar, such as "account: primary, nightly" or
"held only": a click on the chip opens its values again, its x removes it, the chips past five fold behind "+N more",
and from two chips on "Clear all" removes them and keeps the range, the search and the page size. The account filter
keeps the requests whose serving attempt ran on the account, so a request that failed over lists under the account that
answered it. The Dropped thinking filter keeps the requests whose serving answer listed a block under a picked kind.
Every filter, the range, the page size and the order live in the page's address, so a reload or a shared link keeps
them.

The table offers sixteen columns, Time, Key, Pool, Account, Model, Client, Kind, Transport, Status, Attempts, TTFB,
Duration, Tokens, Uncached input, Output tokens and Cost, with Duration, Kind, Output tokens, Pool, Transport and
Uncached input hidden until the Columns control turns them on, and shows Time, Model, Status and Attempts alone while
the drawer is open. Uncached input and Output tokens read the serving attempt's counts, and a row whose serving attempt
stored no count reads a dash in both, with the reason on hover. The Sort control orders the table by Time (descending at
rest), Attempts, Duration, TTFB, Uncached input or Output tokens (each descending when picked), a request without the
figure last under either direction, the read paging by the key, so each page holds the next rows in the order, and a
change of the key or the direction returns to the first page. Every column but Time is as wide as its widest value, and
Time takes the card's free width: the request id under the time is cut by the room with an ellipsis, whole on hover, and
reads whole where the card holds it, on a wide screen or beside the drawer. Time keeps room for the widest time the
clock draws, so a time never cuts, and under the 12-hour clock, whose times run wider, the default columns can run past
the card at 1280 px and scroll inside it, as any set of columns wider than the card does. Time reads the date and the
clock time with seconds, `Nov 5 13:11:05`, whatever the row's age, so a row of any range reads its date without the
hover. The model column reads the family and version with the whole id on hover, and a dash for a request the router
refused before it resolved a model. The client column reads the class the router derived from the user agent
(`claude-code/<entrypoint>` or `other`), whole on hover, with the agent type beneath it on a subagent's turn. The
drawer's Client row names a session that branched from another conversation, `fork of {base} at 3`, with the base's
whole id on hover. Status reads the code, a warning badge with the refusal's category where the upstream refused the
answer (`none` where it gave no category, its hover "The upstream refused this answer. Open the request for the stop
reason and the recommended model.", the request having succeeded), an info badge `fallback` where the request presented
a fallback credit token (its hover "A fallback token issued by {account}" naming the account of the refused attempt that
issued the token where the table holds the issuing request, and "A fallback token presented with this request"
otherwise), and the outcome word beside it where the request did not succeed (`canceled`, `exhausted`, `failed`,
`incomplete` or `rejected`). One row is one client request however many attempts it took: Attempts reads `2 attempts`
with the trail of account and status on hover, nothing for a request of one attempt, and a `waited 42 s` badge for a
request that waited through a hold, for a slot while every account was at its cap, for a reset, for the slow-down or for
the rewrite in flight, whose hover names each wait with its reason in the row's order and the account it waited on,
`waited for the slow-down 42 s, for a slot 3 s on {account}` or `waited for the rewrite in flight 2 s on {account}`, or
each with its own account where the waits span two,
`waited for the slow-down 42 s on {account}, for a slot 3 s on {other}` (a row from before the reasons reads
`waited 1 s on {account}`, and a wait under a second reads `1 s`). Cost reads in the top bar's unit, its header's hover
naming what each kind of request reads under the position. Under Native a request a subscription window paid reads one
figure in the chosen week, such as `0.0244% Ultra week`, with its hint and the dollar equivalent on hover. The hint
names the account's plan, its display name where the account carries one, and its share of each window the account has,
in the order 5-hour, 7-day and the model week, with "(in the band)" after a 5-hour share that completed inside the band,
read at its completion on the cost revision in force when the request was received: "Ultra: 0.11% of the 7-day window,
0.22% of the Fable week". It names no window the account lacks, so an approved Ultra account reads no 5-hour share, and
no Fable week on a Pro account. Under the Max and Ultra weeks the figure waits on the settings, so the cell draws its
placeholder until they answer. Where that read fails, the cell draws a dash titled with the read's message, the drawer's
Estimated cost row draws the same dash with the message leading its hover, and the split Refresh button's body reads the
settings again. A request whose model has no weight in the cost model reads its dollar equivalent over `equivalent` with
the hover "The cost model prices no share for this request". Under All in dollars and Dollar equivalent a subscription
request reads its dollar equivalent over `equivalent` with the hint and the figure in the chosen week on hover, and
under Billed dollars a dash with the hover "Nothing billed: a subscription request". An API-key, fast mode, overage or
usage-based request reads dollars under every position but Dollar equivalent, where it reads a dash with the hover
"Billed in dollars, no subscription equivalent". A client classifier call on a subscription account reads `0%` in the
chosen week under Native, with the hover "Free on the plan: a client classifier call takes no window share" ahead of the
hint, `$0.00` over `equivalent` under All in dollars and Dollar equivalent with the same hover ahead of its zero figure,
and a dash under Billed dollars, while on an overage or API-key account it reads its dollars as any billed request does.
The table pages by "Rows" (10, 25, 50 or 100, 25 by default) with First, Previous and Next, holds the requests up to the
moment you opened it so paging does not shift, and the split Refresh button's body brings it up to now. An empty table
reads "No requests match" with "Widen the range or clear a filter." and "Reset filters", which clears the filters, the
search and the range. What each figure means is on [Statistics](statistics.md).

Auto reload is off when the view opens. The split Refresh button's dropdown sets it to Off, 5 s, 10 s, 30 s or 1 min,
and the choice stays with the open page, out of its address. At each interval it reads the list again, a preset's range
ending at the router's clock and a custom range keeping its own end, where a request received inside it and written
after a read draws on the next read, takes every row that arrived since the read before, up to eight pages, and lands a
new row at the top in the accent tint until the next read. Under a key or a direction other than the default, a new row
lands where the order files it. A long request can finish below rows already shown: it lands between rows already shown,
in the same tint, on the next auto reload. On any range whose rows fit on the page shown, a request that lands below
them after a read draws on the next read in the accent tint, and where the range runs on past the page a late row below
it stays for Next. Past eight pages a line reads "More rows arrived than auto reload could fetch. Reload to read them.",
and Next reads on from the last row the poll fetched, under a preset to the router's clock. A poll whose newest page
stood in for a list read that failed gives way to the list once it answers. A filter change keeps the interval. The poll
waits while the tab is hidden, while the drawer is open, on an older page until you go back to the first, and while a
menu or popover under the bar is open, with the interval kept and the cause named in the dropdown's title. When the wait
ends a full interval or more after its last answer, it reads at once. When the tab comes back to a read at once, the
rows that read brought keep the tint through the read after it. A custom range whose end lay more than a minute in the
past when the page took it has nothing new to read, so the dropdown reads Off and stays disabled, while a link whose
range ends at the moment it was made reads on, each read refreshing the rows inside it.

Clicking a row, or pressing Enter or Space while the row has the focus, opens the "Request details" drawer, and doing
the same on the open row closes it. The drawer holds the request's id and time, a "Routing log" link that opens the
[routing log](#routing-log) on the request a quarter hour either side, and these sections:

- Outcome: Status, Outcome, Account, Model, Canonical model when it differs from the model the client spelled, TTFB,
  Duration, Error (the error code, which a stream cut after its head follows with the relay's words for the cut, such
  as `stream_undecodable: error decoding response body`), Client drift when set, Client contract when set (the item
  the refusal named), then the serving attempt's Answer (the stop reason, the speed, the geography, the service tier)
  and Cache (the miss reason in the Misses by reason card's words, "Not requested" and, for a request that asked and
  drew none, "No reason returned" among them, `{read} read, {written} written`, where the prompt diverged, "no response
  to read" on an attempt that drew no answer, and "no usage report" on an answer that held none, a token count's among
  them), and a Diagnostics row naming who put the cache diagnostics member on the body: "sent by the client", "added by
  the router" or none. A Dropped thinking row follows it for the thinking blocks the serving answer dropped or let pass:
  a line per kind with its count of blocks and the first message they reach, then, where the row names a sender, a line
  that names who sent the thinking-binding token, "token sent by the client" or "token added by the router". It says
  that none was dropped where the answer listed no block, that none was reported where a token went out and the answer
  carried no list, and that no report was asked for where no token went out.
- Guardrails: one row per guardrail that matched, in evaluation order, the guardrail's name (or `{id} (deleted)` for one
  the registry no longer holds) with a `blocked` badge in the critical tone or an `observed` badge muted, shown where
  the request carries a match ([Guardrails](guardrails.md)).
- Phases: a waterfall over Received, Selected, Written, Head, First content, Stream end and Finished, a phase the
  ledger holds no instant for reading "not measured".
- Usage: Input (uncached), Cache read, "Cache write, 5m", "Cache write, 1h", "Output, thinking", "Output, plain", Server
  tool calls and Estimated cost in the top bar's unit: a subscription request's figure in the chosen week under Native
  as one chip with the dollar equivalent after it and the hint's plan and its shares as chips on a row of their own
  under it, its dollar equivalent with `equivalent` under All in dollars and Dollar equivalent and a dash under Billed
  dollars, and a billed request's dollars, or a dash under Dollar equivalent. Its hover names why a dollar figure or a
  dash stands, as the Cost cell's hover does, then the hint, the serving account's tier (`Account tier {tier}`) and the
  price list and cost model in force when the request arrived (`Priced at the API prices of {date} and the cost model of
  {date}`, each date with its time where the entry carries one). A client classifier call's row reads its zero figure
  under Native and `$0.00` with `equivalent` under All in dollars and Dollar equivalent, each with the hover "Free on
  the plan: a client classifier call takes no window share", and a dash under Billed dollars with the hover "Nothing
  billed: a subscription request".
- Request: Session, Agent, Client with the request class badge and the fork line where the session branched from another
  conversation, User agent (such as `claude-cli/2.1.310 (external, cli)`), Key, Pool and Kind.
- Attempts: one row per attempt in its order, with its account, its outcome and status in the words and tones of the
  table's Attempts cell, its duration, the cooldown it set, the upstream's request id and, on the answer that served,
  its message id. A request with no attempt draws no such section.
- Request shape: Effort (the level the upstream applies), Thinking, Max tokens, "Tools, system blocks, messages",
  Context edits, Temperature, "Speed, service tier", Auto mode and Classifier (`client` for the client's own auto-mode
  classifier call, which takes no share of a plan window, `client_off` for one run off the account its judged turn
  ended on or with no turn to judge, which is charged as any call, and `none` otherwise), "not sent" where absent.
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

A request the ledger does not hold reads "Request not found" with "It may have aged out of the ledger." A failed read of
a request the list does not show draws "Could not load the request" with the error and Try again, which reads the
request again.

### Feature calls

The Feature calls section lists the calls the router sent to a claude.ai feature host under a feature account ([The
records](claude-ai-features.md#the-records)), one row per call, newest first. Time reads the call's arrival as the
request list reads a time, with the call's id under it. Key reads the key's name and Account the label of the account
whose sign-in the call carried, each with its id on hover, and the id itself where the key list or the account list
holds no row for it. A row takes the name the key list holds now, so where a key's id is issued again, an older call
reads the newer key's name. Feature reads Artifacts or Connectors, Method the client's method, Path the path without its
query in the monospace face, cut in the middle past 48 characters and whole on hover, and Duration the time from the
call's arrival to the answer's end. Status reads the status for a forwarded call and `canceled` for a call whose client
left before the answer's end. A failed call reads its code, `upstream_unreachable`, where no upstream head arrived and
the router answered with a 502, and its status with `ended early` behind it, such as `200 ended early`, where the answer
stopped after its head went back. A session's watch of an artifact reads as one row with the status 101, written when
the socket opens, and Duration is then the time to the `101`. A call the router refused leaves no row, and a row opens
no drawer, since the table shows all the row holds.

![The Requests view: the Feature calls section on its first page](assets/requests-feature-calls-light.png#only-light)
![The Requests view: the Feature calls section on its first page](assets/requests-feature-calls-dark.png#only-dark)

Above the table the section draws a bar of its own, and nothing of the request list's bar shows there: the range, Add
filter with Key, Feature, Account and Outcome (`canceled`, `failed` or `forwarded`), each filter in force a chip under
the bar, and the split Refresh button. Both sections read the one range. The section and the range live in the page's
address, so a reload or a shared link keeps them, while the four filters stay with the open page, through a look at
Model requests, and start empty on a reload. The table pages 50 calls at a time with First, Previous and Next, and a
change of the range or of a filter returns to the first page. Under a preset each read runs up to now, from the start
the range took when the page loaded or Refresh last ran, and a custom range reads its own window. Refresh, titled "Reads
the list again now" under a preset, takes the range again for both sections. Auto reload is off when the page opens and
keeps its interval through a look at Model requests. The dropdown offers the request list's intervals: each one reads
the first page again, and it waits on an older page and while a menu or popover under the bar is open. While Feature
calls shows, the request list reads nothing on its interval.

An empty table with no filter reads "No feature calls in this range" with "A row appears once a key's pool selects an
account for a feature and a client in a feature setup uses it." and the link "How the claude.ai features work", which
opens the documentation's page on the features in a new tab. With a filter it reads "No feature calls match" with "Widen
the range or clear a filter." A failed read draws "Could not load the feature calls" with the error and Try again, which
reads the calls again.

## Analytics

![The Analytics view: the Traffic tab](assets/analytics-light.png#only-light)
![The Analytics view: the Traffic tab](assets/analytics-dark.png#only-dark)

The sticky filter bar holds one row: the range, Add filter, "Group by", the Sort control and, while the page groups by
user agent, the "Groups (N)" button, then at the right edge the split Refresh button. It has no search and no count. The
range offers Last hour, Last 6 hours, Last 24 hours (the default), Last 7 days, Last 30 days or Custom range with From
and To ("End must be after start." while the pair is wrong), the pills and the popover as on the Requests view, and a
range reads the whole hours it touches on the rollup tabs, from the first bucket's start on the Burn tab, and the range
itself for a session's own figures in a session view. Add filter opens a "Filter by" field over the filters in three
groups: Routing (Account, Key, Model, Pool), Request (Client, Include probes, User agent) and Subject (Agent, Session).
Account, Key, Model and Pool list the values the range holds. Client and User agent list the values the range holds,
each a checkbox, and take a typed pattern in the Requests view's grammar through the field under the rows, added on
Enter, several picks matching any, a picked value matching itself in full. The User agent list holds the range's product
tokens. Include probes applies at once, and Session takes a session id, typed or pasted, which matches the session
exactly. Each filter in force stands as a chip on a line under the bar, as on the Requests view, such as "client:
claude-code*" or "include probes", each Client or User agent value a chip of its own, cut after 32 characters with the
whole on hover. The Requests links of the Misses by reason counts and the refusal card carry one client pattern, and
read plain under several. The session chip prints the id's first eight characters, and its hover names the whole id with
the key lifetime a link from the Sessions view carries ("under the key laptop@3"), which has no chip of its own and
leaves when the session changes. The session view narrows to one agent where the address carries `agent_id`, which the
Sessions view's agent menu writes, and the agent applies under a session alone and leaves with the session as the key
lifetime does. "Group by" offers Account, Classifier, Client, Key, Effort, Kind, Model, Pool, Request class, Speed, User
agent or "Session (Burn only)", the last the Burn section's own grouping. The grouping splits each tab's group table,
the Cost by chart and the Burn cards, and every other chart draws the groups' total. While it reads User agent, "Groups
(N)" opens the "User agent groups" box with one pattern per line, N the patterns it holds, where each pattern names a
group and every other agent groups under `other` (`claude-cli/2.1.*` and `claude-cli/2.0.*` as two groups). The user
agent grouping reads the agent's first product token, `claude-cli/2.1.310` for `claude-cli/2.1.310 (external, cli)`. The
classifier grouping lists the requests with no classifier call under `none`, as the Requests view's Classifier filter
names them.

The Sort control offers Name (ascending by default) and the open tab's group table columns in the table's order, each
descending, and Name ascending is the order at rest on every source, the session view's tables included. Under either
direction a group with no requests files last under Error rate and one with no sample last under a percentile. While the
top bar's unit is Native, the Cost, Per request and Per 1k output keys order by the subscription requests' figure, and
under either direction a group whose subscription requests priced nothing files last under all three and a group of
billed requests alone last under the two ratios. Under a dollar position they order by the dollars. A tab switch keeps a
key the new tab's table has and reads Name otherwise, and the address keeps the key for the next tab whose table has it.
On Burn, Cache warming and Accounts, and on the Cost tab of a session view, which draws no Cost table, the control
stands disabled in its slot, titled "This tab's tables keep their own order". On Accounts and on Cache warming "Group
by" stands disabled in its slot too, titled "The Accounts tab draws one line and one row per account" or "The Cache
warming tab draws the total and one row per pool", and so does the "Groups (N)" button beside it under the same title.
The tables over a fixed dimension (Cache by model, Cache warming by pool, Accounts in range) and every chart and card
keep their own order.

Auto reload is at 5 s when the view opens, set on the split Refresh button's dropdown as on the Requests view and kept
out of the page's address. At each interval, once a new bucket of the range has begun, it moves the window's end to now
and reads the account list and every chart again, so a page left open shows the hour it is in. It waits while a menu or
popover under the bar or the Cost reference dialog is open, or the tab is hidden. Off, the window's end holds where it
was read and the dropdown's title adds "the window ends {time}" until a filter, the range or Refresh moves it. The split
Refresh button's body moves the end to now and reads the account list and every chart again. A custom range holds its
own end either way, and one whose end lay more than a minute in the past when the page took it reads Off in the
dropdown, disabled, while a link whose range ends at the moment it was made reads on. A custom range whose end has not
passed reads the account list again at each interval, as it reads its charts.

Filtered to one session the view sums the request log instead of the hourly rollups and says so in four heads: Traffic,
Cache and Cost read "From the request log, since rollups carry no session" after their figures, and Latency reads it in
place of "Percentiles read from merged per-hour histograms". The sums run over the first 10,000 requests of the range,
and a session with more reads "Partial: the first 10,000 requests of the range" on a line under the bar. On a custom
range the view walks the session's pages once, and each auto reload re-reads the first page alone, with the later pages
standing as loaded. Under that filter the Cost split reads "Cost split is not available for a session view.", the Write
cost vs read cost legend reads a dash for each cost, and Cache warming reads "Cache warming figures are not available
for a session view." with "not in the request log" under its probe tiles, since a request row carries one cost total and
no probe or kept-warm fact. Grouped by key, a deleted key's group reads `<id> (deleted, created <time>)`, its creation
instant in the viewer's zone. The range, the grouping, every filter, the client groups, the probes switch and the sort
with its direction live in the page's address. A link that carries `unit` sets the top bar's Unit switch to that
position and leaves the address, so a shared link opens on its unit. The section bar under it shows one section at a
time, Traffic, Burn, Latency, Cache, Cost, Cache warming or Accounts, and a range with nothing reads "No traffic in this
range." What each figure means is on [Statistics](statistics.md).

Traffic reads `{n} requests, {pct} errors` and draws Requests by outcome (OK, Rate limited, Failed, Incomplete, Client
canceled, Exhausted and Rejected, where Rejected draws in the muted ink for the requests the router rejected itself),
Errors by code ("Failed and incomplete responses in range, the refused requests left out"), Refusals by code ("Requests
the router refused before any account was asked, not in the counts", or "No request was refused in this range." where
none was), Refusals by category ("Answers the upstream refused in range, by model and category, each a request that
succeeded", one bar per model and category linking the Requests view on the stop reason `refusal`, the category and the
model over the whole hours that the range touches, each bar labeled with the model's family and version and the
category, such as `sonnet-5-5 · cyber`, with the whole model id on hover, `none` for a refusal with no category, or "No
answer was refused in this range." where none was, with the requests that presented a fallback token counted in the
subtitle ("1 request presented a fallback token"), and under a client or user agent filter the matching requests' alone,
and under a session filter every session's, which its subtitle then names, its links carrying the client pattern and no
session, plain under a user agent filter as the Misses by reason counts are), Failovers, Migration rewrites with its
moves (one bar per move reason, stacked by the destination's fit, `fits`, `rewrite only`, `least bad` or `unknown`, each
segment titled `{fit} {n}` and the bar labeled with the reason's total, and under them "Held by the slow-down", a bar
per bucket of the time the slow-down held requests in the warning tone, its hover `12 requests held 14 min in all`, `no
request held` on a bucket that held none, and its legend line the range's sum, or `none in range` with no chart),
Requests by speed, Effort levels, then at full width the card "Guardrails" ("Matches by guardrail in range, every key")
with one row per guardrail that matched in the range as Guardrail, Observed, Blocked and "Blocked bytes", reading the
range alone and no other filter, a guardrail the registry no longer holds as `{id} (deleted)` and "No guardrail matched
in this range." where none did ([Guardrails](guardrails.md)), and the table Traffic by the chosen group with Requests,
OK, Rate limited, Failed, Incomplete, Client canceled, Exhausted, Rejected, Refused, Error rate, Failovers, Search and
Fetch, the Requests header reading "The refused requests are left out, see the Refused column" on hover, the Client
canceled header "Requests the client canceled. They are left out of the error rate", the Error rate header "The refused
requests and the client's cancels are left out, see the Refused and Client canceled columns" and the Refused header the
Dashboard's Refused tile hover. Under the model grouping a request refused before it resolved a model groups under "no
model", which the Cache by model table leaves out, and a group with no request, as that one, reads a muted dash in its
Error rate cell, as the head's `{pct}` reads a dash where the range rated no request. A group that counts no request
leaves the Latency and Cache group tables. The Cost table and the Cost by chart keep such a group only where it still
holds a cost, as a group of billed keepalive probes alone does, and Traffic keeps it for its Refused column. A request
the router refused before any account was asked is counted in its group's Refused column and in the "Refusals by code"
card, a guardrail's block on the Guardrails card's Blocked and "Blocked bytes" as well, and never in the request count,
the outcome columns, the error rate, the cache figures or the latency. A request whose client left before its answer
ended counts among its group's requests, and the head's errors, the Error rate column and Errors by code leave it out.
The bars of Errors by code carry the upstream's own error types beside the router's words. Three of Traffic's cards
follow fewer controls than the bar holds: Refusals by category follows every control but the Agent and Session filters,
Migration rewrites the range and the Account filter, its "Held by the slow-down" chart the Pool filter too, and
Guardrails the range alone. Each names the controls it ignores under its title while one of them is set, the chart under
its legend line.

Burn follows the page's grouping, and "Session (Burn only)" groups it by session while every other section keeps the key
grouping, since no rollup carries a session. Of the filters, the Account, Key and Pool filters and the user agent groups
apply to it, and its head names the Model, Client, User agent, Agent and Session filters as ignored while one of them is
set. Burn counts the probes whatever Include probes says, and Member budgets follows neither the range nor a filter. The
card "Plan per hour" ("The subscription traffic per hour as a share of the chosen week") draws one line per group, named
as the breakdown names it, the requests with no classifier call listed as `none` under the classifier grouping, each the
group's subscription traffic per hour in the chosen week, the legend's figure in full on hover and its highest hour on
the entry's hover, led by the whole session id under the session grouping. Beside it the card "5-hour per hour" ("The
subscription traffic per hour as a share of the chosen tier's 5-hour window, the peak band's factor applied by the
hour") draws the same lines from the burn route's 5-hour figure for the chosen tier under every grouping and bucket
size. The card "Fable week per hour" ("The subscription traffic per hour as a share of the chosen tier's Fable week")
draws the subscription traffic of the models that draw the Fable week, and of those alone, from the burn route's
Fable-week figure for the chosen tier. A busy bucket with no such traffic reads zero, and a group whose traffic over the
whole range ran on no such model draws no line and no legend entry. The two cards' tooltips carry no account's hint.
Under the Ultra week the 5-hour card's note reads "The Ultra tier's 5-hour capacity row is assumed, and an approved
account carries no 5-hour window." On each of the three cards a busy bucket with no figure under the chosen tier stays a
gap, and an idle bucket reads zero. The gap draws under a "model unlisted" entry, with the hover "A bucket whose traffic
ran on a model without a weight in the cost model prices no share and draws no point", or under a "tier unsized" entry,
with the hover "A bucket priced under a cost revision that holds no capacity for the chosen tier on this card's window
draws no point". "Plan per hour" files the gap under "model unlisted" where the router flags the bucket as holding a
plan row whose model has no weight on the 7-day window, and under "tier unsized" otherwise. The other two cards file it
under "tier unsized" where the bucket holds a figure for another tier, a zero among them, and under "model unlisted"
where it holds one for no tier. A card draws its unpriced sentence in place of its chart, such as `Unpriced: the cost
model holds no 5-hour capacity for the Ultra week`, where its range holds a "tier unsized" gap and no bucket with a
figure under the chosen tier. The three plan cards read the figure the router answers for the chosen tier, which it
scales row by row under each row's own cost revision and band before it sums, so a bucket that spans a revision's start
holds each side at its own revision. Where one account is named, under the account grouping or with one account in the
Accounts filter, a bucket's tooltip adds the hint the Requests view's Cost column describes under the account's line,
with no band word, since a bucket holds no one request's completion. Under that grouping or filter a subscription
account charts as a plan line where the router sizes its tier or the range holds a priced bucket of it, so an account
whose range ran on unpriced models alone keeps its line with its gaps, and an account the router prices no tier for (a
free plan, no tier on file or one the cost model does not hold) whose range holds no priced bucket reads "tier unknown",
while a usage-based seat draws no line on the card. An account the registry no longer holds charts by the funding of its
rows: its plan traffic draws under its id on the three plan cards and its billed traffic on Spend per minute, and a
group none of whose rows in the range holds a token draws on no card. The three plan cards' notes carry a "Cost
reference" link that opens the dialog on Tiers and peak band. Under the Unit setting "Plan per hour" charts the plan
side, the chosen week's share per hour under Native and each group's `plan_usd` per hour under All in dollars and Dollar
equivalent, where its subtitle reads "The subscription traffic per hour in dollars at the API price list", and "Spend
per minute" charts the billed side, `usd` per minute, under Native, All in dollars and Billed dollars, a usage-based
seat's whole spend and a subscription account's fast-mode and overage answers among it. A card whose side has no figure
in a position, "Plan per hour" under Billed dollars and "Spend per minute" under Dollar equivalent, keeps its place in
the grid and draws one sentence in place of its chart, so the two cards together read `usd` plus `plan_usd` under All in
dollars, `usd` under Billed dollars and `plan_usd` under Dollar equivalent. The four cards sit two to a row where the
grid fits two columns or more. The section closes on Member budgets, Pool, Account, Spent, Budget and Period ends, one
row per budget a member carries, a member at its budget marked and one the ledger has not reported on reading "not
reported".

![The Analytics view: the Burn tab's plan chart](assets/analytics-burn-light.png#only-light)
![The Analytics view: the Burn tab's plan chart](assets/analytics-burn-dark.png#only-dark)

Latency reads "Percentiles read from merged per-hour histograms. The slow-down's hold is left out" and draws one row per
metric, Time to first byte, Time to first content token, Total duration and Output tokens per second, each with a
percentile chart (p50, p90, p95, p99) and a Distribution card, then Latency by the chosen group with TTFB p50, TTFB p99,
TTFT p99, Duration p99 and tok/s p50. The histograms hold 84 buckets that reach about 61 minutes. A request counts
toward Output tokens per second only where its answer reports output tokens above zero and at least 500 ms pass from its
first content token to its end, and the three times leave out the time the slow-down held the request. Latency follows
every filter of the bar.

Cache reads `{pct} hit ratio, {n} expired between turns, {pct} of input read from cache` and draws Tokens (Uncached
input, Cache read, Cache write 5m, Cache write 1h, Thinking, Output), Hit ratio and cached share, Misses by reason,
Dropped thinking blocks, Write cost vs read cost, the per-group tables of Misses by reason and of Dropped thinking
blocks, Cache by model (Reads, Writes 5m, Writes 1h, Hit ratio, Cached share, Read cost, Write cost) and Placements by
rung (Rung, Placements, Hits, Hit share, Tokens read, Tokens promised). The hit ratio counts the requests that read the
cache over the requests that could read one: a token count, and a request whose body marked no cache breakpoint and that
read and wrote nothing, stay out of its base. Where no request could read one, the hit ratio reads a dash: in the head,
in the legend and the hover of Hit ratio and cached share, and in Cache by model. Write cost vs read cost and Cache by
model's Read cost and Write cost read in the top bar's unit. Under Native they read the subscription requests' figure in
the chosen week under every grouping, the card's subtitle naming the billed requests' dollars and each Cache by model
cell carrying them on a line under it, and the card draws the billed dollars while no subscription request is in range.
Under the three dollar positions they read the position's dollars. A null diagnostics member on a request that named a
message is recorded as "No divergence", and the per-group table lists Pending after Not requested. Pending: the
upstream's comparison was still running when the answer went out. An answered request whose answer named no reason
counts under Not requested where its body carried no diagnostics member and under No reason returned where it carried
one. In the hours the router stored before it filed these two words such a request counts under no reason, so those
hours' bars add up to less than their requests. A bar draws apart the requests under its reason that read 90 % or more
of their prompt from the cache, and the per-group table shows the same split. A bar's share is of the sum of the card's
reasons, the requests that the card counts, which the card's subtitle names. Every count in Misses by reason but the
expired column links to the Requests view on those requests with the page's filters carried over, and a count the
Requests view cannot list exactly stays plain with the reason on hover. The Dropped thinking blocks card counts the
requests whose answer listed a thinking block the upstream dropped or let pass, one bar per kind with its count as a
link to those requests in the Requests view, a request counted once under each kind its answer lists. A bar's share is
of the requests whose answer reported its dropped blocks, which the subtitle names. Over a range that reaches an hour
whose requests the ledger no longer held when the router first kept that count, the card draws its counts and no share.
Under the bars it reads two plain figures, the moved requests whose answer listed a block dropped for the account (the
reasons `end_user_binding_mismatch` and `organization_binding_mismatch`) and the answers that listed nothing, and it
counts no block. Cache follows every filter of the bar, and Placements by rung the Account, Key, Model, Pool, Client and
User agent filters.

Cost reads in the top bar's unit. Under Native the tiles read Cost, the subscription requests' figure in the chosen week
over the billed requests' dollars, a side with no requests and no dollars left out, and Cost per request and Cost per 1k
output tokens the same two ways. Where one account is named in the Accounts filter, the Cost tile's hover carries the
hint the Requests view's Cost column describes, and each row's Cost cell does under the account grouping, or under any
grouping while one account is named, since every row is then that account's, while a figure that sums several accounts
carries none. Cost by the chosen group stacks the figure per bucket with "Billed by {group}" beside it in dollars, the
Cost split draws a ring per side by token kind, the subscription requests' with its figure in the chosen week in the
center (each token kind's figure in full on hover) and the billed requests' in dollars, and the table reads Requests,
Cost, Per request and Per 1k output, each with the billed dollars on a line under the figure. A subscription side the
route could not price, as when a request ran on a model without a weight in the cost model, reads a dash, its bucket on
the Cost by chart a gap, and the Cost split draws no ring for it. The Cost tile's dash carries the Dashboard's hover,
and the Cost by chart names the groups it draws that read unpriced on a line under its subtitle,
`{group} unpriced in {m} buckets, drawn as gaps` for one group (`research unpriced in 1 bucket, drawn as a gap`), the
groups' labels joined by middle dots for up to three, and `{n} groups unpriced in {m} buckets, drawn as gaps` past
three. Under the Max and Ultra weeks the figures wait on the settings, so the section draws its placeholders until they
answer. Where that read fails, the tiles draw the read's error and the cards their error state. Under All in dollars,
Billed dollars and Dollar equivalent Cost draws the tiles Estimated cost, Billed dollars or Dollar equivalent, Cost per
request and Cost per 1k output tokens, then Cost by the chosen group as a chart and a table (Requests, Cost, Input,
Cache read, Cache write, Output, Per request, Per 1k output) and the Cost split as one ring in dollars (Input, Cache
read, Cache write, Output). Every position draws Fast mode spend. A billed keepalive probe's dollars count in the cost
and the billed dollars, while its request and its tokens follow the Include probes toggle, and under All in dollars and
Billed dollars with the toggle off the tab's head says so, in a session view as well: "A billed probe's dollars count
whatever the Include probes toggle says". The prices behind the dollars are on [Pricing](statistics.md#pricing), and the
cost model behind the subscription figures on [Plan meter](statistics.md#plan-meter). Cost follows every filter of the
bar.

![The Analytics view: the Cache warming tab](assets/analytics-cache-warming-light.png#only-light)
![The Analytics view: the Cache warming tab](assets/analytics-cache-warming-dark.png#only-dark)

Cache warming draws the tiles Probe cost, Saved by probes, Net saving and Kept warm, whose note splits the tokens read
warm between five minutes and an hour, then Probes by outcome (OK, Held, Skipped, Failed), Wasted probes (Wasted probes,
Wasted cost, Share of probe cost), Saved by probes, Kept-warm requests, Kept-warm tokens and Cache warming by pool
(Probes OK, Held, Skipped, Failed, Kept-warm requests, Kept-warm tokens, Probe cost, Saved). The probe counts on the
Probe cost tile and the Wasted probes card read the probes sent, and Probes by outcome names the held and the skipped
ones apart. Saved by probes is what the kept-warm requests saved, the cache write each avoided at its TTL's rate less
the read it paid. Net saving is that saving less the probe cost, in the critical ink while the probes cost more than
they saved, which under Native the plan side alone decides. A held or skipped probe never reached the upstream and costs
nothing, so a range whose probes were all held or skipped reads a zero Probe cost and its whole saving as its Net
saving. A Saved by probes chart draws the gross saving and the probe cost per bucket with the net on hover. The by-pool
table's Saved column reads each pool's saving beside its Probe cost. Probe cost, Saved by probes, Net saving, Wasted
cost, Share of probe cost and the by-pool Probe cost and Saved read in the top bar's unit: under Native the plan side's
figure in the chosen week over the billed side's dollars, with no hint, and under the three dollar positions the
position's figures in dollars. The chart draws dollars under the three dollar positions and the figure in the chosen
week under Native, with the billed dollars on hover, and the billed dollars alone under Native where the range holds no
plan side. The tab's keepalive figures follow the Client and User agent filters from the hours folded by this build on.
In an hour folded before it the kept-warm, saved, skipped and wasted figures stand under no user agent, and all but the
kept-warm and saved ones under no client, so there the User agent filter drops those figures and the Client filter drops
the skipped and wasted ones. Probe cost and the OK, Held and Failed counts read the probes' own request rows, which
follow both filters in every hour. So in an hour folded before this build, Net saving under the User agent filter counts
the probe cost against no saving and can read below zero. What a probe is and when it counts as wasted is on
[Keepalive](keepalive.md). Cache warming counts the probes whatever Include probes says, which its head states, on
Probes by outcome and the by-pool table's probe columns as on its tiles. That toggle is the one control its figures
ignore.

Accounts draws Peak 5-hour utilization, then Window usage and Pace under a "Window" switch whose positions are the three
windows the router reads, 5-hour, 7-day and 7-day Fable. Pace reads "The lead over pace per bucket: above zero ahead,
below behind, from the quota samples, before the router's burn projection", each account's hover adding its Pace score,
the remaining share over the hours from the sample's instant to the earlier of the window's reset and the account's end.
The table Accounts in range lists Peak 5h, Peak 7d, Peak Fable, Time in overage, Time in cooldown, Pins created,
Migrated in and Migrated out. Time in cooldown counts the time an account stood under at least one cooldown, a minute
once however many of its scopes were open, and a cleared cooldown up to its clear, and the clear leaves no time
behind. It counts an open cooldown up to the moment of the read. The section reads the account rollups, which carry the
account alone: it follows the range and the Account filter, and its cards name the Key, Pool, Model, Client, User agent,
Agent and Session filters and Include probes as ignored while one of them is set. The card "Unused allowance by account"
follows Accounts in range, one row per subscription account with a weekly reset in the range under the Accounts filter,
with Resets, the count of the range's resets, which reads the used limit resets apart ("4, 1 used") with a hover that
names each use (`{window} cleared by a used reset, seen {instant}, {share} left`), and each weekly window's last figure
and its sum over the range in the chosen week, "last" being the newest reset at or before the range's end (each week
priced at the tier the account ran under at that reset, as the Accounts detail's "Allowance left at reset" prices it), a
used reset that cleared its window counting among the resets as its `claim` row, sorted most left first on the range's
7-day figure, an unpriced account last, a row whose latest reset is a term's end reading "term ended {date}" under its
account, and two fleet figures over the rows, "7-day, fleet total" and "Fable, fleet total", the priced accounts'
figures added with the unpriced accounts named. A plan change that cleared a window counts among that window's resets in
the range that holds its drop, and never as a use. The card reads the resets from a week before the range's start, 400
days before the range's end at the most, so a range shorter than a week still shows an account's newest reset, with a
note that the range holds none, and the Accounts detail's "Allowance left at reset" keeps its four weeks. The range's
hover on this tab says so: "Reads whole hours: {from} to {to}. Unused allowance by account reads its resets from a week
before the range's start." The card waits on the cost model, so a failed settings read draws its error state in place of
the rows and the figures.

## Sessions

![The Sessions view: the session table with a fork badge](assets/sessions-light.png#only-light)
![The Sessions view: the session table with a fork badge](assets/sessions-dark.png#only-dark)

The view opens on its bar: the search field "Session id, title, project, repository or branch", Add filter, the Columns
control, the "Group by" control (None by default, Project or Repository, carried in the address as `by`), the Sort
control (Created, newest first by default, Last activity, Cost or Requests, each with Ascending or Descending, carried
in the address) and at the right edge the split Refresh button and the count, `{n} sessions · {k} agents`, its agents
counted as the Agents cell counts them over the rows the filters keep. While an Agents filter stands the count reads
`{k} matching agents`, and while the filters hide rows its hover reads `{h} sessions hidden by the filters`. At 1280 px
the right edge stands on a second line whatever the count and the Sort control read, as on Requests, since the count's
slot keeps the width of a count with three digits in each figure. The search field matches the start of a session id, or
a part of the title, the project path, the working directory, the repository, the repository's root or the branch its
companion reports, whatever the case. It reads no agent's fact: the Agent type filter reads an agent's type, and no
filter reads its description or its name. Add filter opens a "Filter by" field over two groups: Session, which reads the
row (Active within, Client, Companion, Finished sessions, Forked sessions, Key, Pool, Session and Slowed), and Agents,
which reads each agent line, main's among them (Account, Agent type, Cache, Effort, Model, Released agents and Status).
Filters are AND: a session is listed when it passes every Session filter and, while Agents filters stand, when one of
its lines passes every Agents filter together. A finished session, one whose main agent ended with the session or whose
every agent the mod released, stays out unless Finished sessions stands, and Released agents draws a live session's
released agents, which stay hidden without it. Status reads each line's word as the Agents table draws it, so main's
line of a session no mod reports matches `unreported`. Session keeps the rows of one session id, matched whole. The
router lists a companion session within the session expiry, which for such a session counts from the later of its last
request and its mod's last report, so its row stays while the mod reports, and a session no mod reports while the latest
of its last use, its last request and, where its companion ended the session, that end is within the lesser of the
non-companion timeout of the Settings page (`sessions.non_companion_idle_seconds`, 1h by default) and the session
expiry, and the view reads that list as it is. Each filter off its default stands as a chip on a line under the bar: a
click on the chip opens its values again, its x removes it, and from two chips on "Clear all" returns every filter to
its default. An Agents filter's chip reads `(any agent)` after its values in the secondary ink, its hover ending
"matched by any one agent". The filters live in the page's address, so a reload or a shared link keeps them, and a bare
`/sessions` reads the defaults. Where a read the rows wait on failed before it answered, the sessions, the recipes or
the keys, a line under the bar names it, `Could not load the {read}: {reason}`, with Try again, and a failed accounts
read draws a line of its own. The head under the bar reads "Every session the router lists: what it runs, where it is
pinned, what it has spent and whether its prompt cache is warm." with the disclosure "How pins and recipes work", whose
paragraph says what a pin and a recipe are, and a "Cost reference" link that opens the dialog on Plan rates. Auto reload
starts at 5 s and reads the accounts, the recipes and the sessions at that one interval, and it waits while the Cost
reference dialog, a row's menu or a popover of the bar is open. The probes and the recipe are explained on
[Keepalive](keepalive.md), the mod's words under [The mod](keepalive.md#the-mod) and the recipe's states under
[The five states](keepalive.md#the-five-states).

The card "Sessions", its title carrying "One row per session. Open a row for its agents, its routing and its cache
warming." on hover, offers "Expand all" or "Collapse all", and its summary line reads the rows the filters keep: `{k}
agents: {b} busy, {i} idle, {p} parked · {r} requests and {spend} in 24 h`, the agents counted as the Agents cell counts
them, the busy, idle and parked ones by their lifecycle word, the client requests of the 24 whole hours before the
current hour and the current hour so far, and their spend as a share of the chosen week, or a dash where a kept row's
day holds a request the cost model priced nothing for, since a partial sum would read as the whole, with `+ {dollars}`
after it where the day billed dollars, the probes left out. The Session column is always drawn, and the eighteen others,
which the bar's Columns control shows or hides, are Status, Agents, Model, Account, Requests, Cost, Last used and Cache,
shown by default, then Companion, Context, Created, Effort, Key, Last request, Next probe, Project, Repository and
Runway, hidden by default. Project reads the folder name of the session's project, or of its working directory where the
companion reports a working directory and no project, with the whole path and a working directory that differs on hover.
Repository reads the last segment of the repository, or of the repository's root where it has no remote, with the branch
under it and the whole `host/path`, the branch and the root on hover. A path at a root, `/` or a drive root such as
`C:\`, has no folder name or last segment, so either column reads it whole, and a session with such a project and no
title leads its Session cell with its id. A session whose companion reports a working directory and no project takes
that directory as its project in its label and under the Project grouping. Both columns cut a name or a branch past 24
characters with an ellipsis, and each reads a dash on a session whose companion reports neither. The rows follow the
sort control: by the session's creation time, its first request, by its last activity, its newest request that is
neither a keepalive probe nor a token count, or its last use where the ledger holds no such request yet, by the list
dollars of its requests, which add across units, or by its requests, newest or largest first by default, ties by session
id and a row without the value last, and the five-second re-read keeps a row's place under Created. Session takes the
card's free width and keeps at least 74 px, and a set of columns wider than the card scrolls inside it.

Group by files the rows the filters keep under their project or their repository. Grouped by Project, the rows stand
under their project's path. Grouped by Repository, they stand under the repository's `host/path`, or under its root
where it has no remote, so a checkout and its linked working trees share a group. Each group opens with a row across the
table that reads the last segment of that value, or the whole value at a root such as `/` or `C:\`, cut with an ellipsis
where it is long, and the count `{n} sessions`, with the whole value on hover. The groups stand in alphabetical order of
that segment or whole value, ignoring case, two groups that share it in the order of their whole values, and the
sessions that report neither a project nor a working directory, or no repository, stand last under "No project" or "No
repository". Inside a group the rows keep the Sort control's order. A group none of whose rows pass the filters is not
drawn, and a group's row opens nothing and does not fold.

The Session cell leads with the session's label, which is the custom title its companion reports, else the folder name
of its project or, where it reports no project, of its working directory, else its id. A label that is not the id stands
over the id, each cut with an ellipsis, and the hover reads both whole. A session whose companion reports none of the
three reads its id alone, cut with an ellipsis and whole on hover, as each session without the mod does. A line under
the id reads a `fork` badge on a session that branched from another conversation at depth 2 or deeper, then the client
version and the placements count from 2 on, `Claude Code 2.1.310 · 2 placements`, with the user agent as the client sent
it and the count's sentence on hover: how often the session's main was placed or moved. The fork badge's hover names the
base and the depth, and where the list holds the base with both pins live, whether the base is on the same account or
another and the base's account. A click on the badge scrolls to the base's row where the list holds it. Status reads
main's lifecycle word as a badge with its sentence on hover: `busy`, `idle`, `parked` with the park's kind and its
deadline or wake time on hover, `registered` for a session whose main the mod registered or started again with no real
request of it reaching the router since, `released` or `unknown`. A finished session reads `finished`, and a session no
mod reports, or one whose main the engine holds no record of, reads `unreported` in the muted ink. Under the word, a
companion session reads `seen 9s ago` while the mod reports, or `silent 2h 18m` in the warning ink once it went silent,
with the companion's sentence and the instant on hover. Agents counts the session's agents less the ones the mod
released, main counted where no line names it, over the agents by word (`1 busy, 2 idle, 1 parked`). Model reads main's
model, short with the id on hover, and a "+N" badge where N agents of the session are pinned on another model, its hover
naming each agent with its model and account in the lines' order. Account links to the pinned account's page with its
kind on hover, then the speed chip on a slowed session with the Quota view's hover, the warning badge `leaving` while a
shedding round's mark stands on the session's pin, its hover naming the account the mark was made on and its window, and
a "+N" for the agents pinned on another account. A session that a round's move or a failover's landing already took off
an account reads no badge. A session whose requests name state that its account holds, such as a file, a container or a
skill, carries the badge `account-bound` on its row and on each such line of its Agents table, and the hover says that
the router keeps its pin on that account and that no rule or round moves it. While a Model or an Account filter stands,
that column's "+N" takes the accent tone where one of the agents it names matches every Agents filter. A row with no
account reads a dash.

Requests counts the session's client requests the retention keeps under its key, the ones the router refused before any
account was asked left out, with its last request's age under it and the instant on hover, and "View in requests" lists
the refused ones with the rest, so a session whose every request was refused still opens its analytics and its request
log. "View in requests" opens the narrowest preset range that holds the session's first request, and where that request
is older than the widest preset a custom range from it to a minute past the session's last request. Cost reads the spend
of the same requests from the ledger's per-session totals, which hold the Requests count too: under Native the session's
share of the chosen week, and under the other positions its dollars as the request log's Cost cell reads a request, then
a `+$` badge where its requests billed dollars beside a plan figure and a "+N" naming every other account the requests
spent on, by a plan share or by billed dollars. The "+N" badge's hover leads with "Other accounts the requests spent
on:", and a billed account's line reads `{label}, {dollars} billed in dollars`. The Cost cell's hover counts the
requests, reads the figure in full with the account's shares of its windows where the requests spent on one account, the
dollars billed by funding and the probes' figure. Under the figure stands the pin's burn per hour as the Unit setting
reads it. A plan pin reads its share of the chosen week per hour under Native, `0.09% Ultra week/h`, its plan dollars
per hour at the API price list under All in dollars and Dollar equivalent, `$0.12/h`, and a dash under Billed dollars. A
billed pin reads its dollars per hour under Native, All in dollars and Billed dollars, and a dash under Dollar
equivalent. The hover keeps the pin's share of the account's own window, and no line stands where the figure its
position needs reads null. A session with no ledgered request reads a dash with "No ledgered request yet". Last used is
the session's last activity, the instant the sort, the Active within filter and "View in routing log" read: its newest
request that is neither a keepalive probe nor a token count, so a session kept warm reads the age of its last turn, or
its last use, the newest use across its conversation pins, main's and its agents', where the ledger holds no such
request yet. A session with no pin reads the receipt of its last request in place of that use, which can be a token
count's.
On a session no mod reports, `expires in {span}` stands under it and counts to the instant the router stops
listing the session, the lesser of the non-companion timeout and the session expiry after the latest of its last use,
its last request and, where its companion ended the session, that end, with "The router stops listing this session at
{time}" on hover. A companion session reads no such line. Cache reads main's pin `warm` within its cache TTL of the
pin's last refresh, the last turn that rewrote its prefix, or `cold` past it, a pin with no refresh on record reading
`cold` until the next turn or probe that refreshes it, with the recipe's state word under it (main's recipe, or the
newest other agent's where main holds none) or `no recipe`, and `no pin` on a row with no pin, the pin's, the state's
and the cause's sentences and the next probe on hover. Of the optional columns, Companion reads the mod's state,
`reporting` with `seen {ago}` under it, `silent` with how long it has been silent, or `none`. Context reads main's
context in tokens, `184K`, with a dash for a session with no pin or no figure, and Effort the effort of its pin's last
turn, `high`, with a dash for none, both from main's conversation pin (or, where main holds none, the newest
conversation pin of another agent). Created reads the session's first request as an age with the instant on hover, and
Key links to the key's row in the Keys view, with `deleted holder` under an earlier holder's. Last request reads the
last request's instant, or `none`. Next probe reads the instant, "pending" or "not scheduled". Runway reads the time
until the projection of the pin's account reaches the ceiling of the window that binds, among those the pin's model
draws: a span such as `6 h 55 min`, `none` where the runway is gone, `unbounded` where nothing burns or where the runway
would reach the window's next reset or the account's end, and a dash for an unpinned row or a pinned row with no
projection. On a pinned row whose pin carries a cycle its hover reads `cycle 20 s`, the gap between the session's
requests the slow-down's hold is sized from. Before the first session the card reads "No live sessions" with "A session
appears here on its first request, or as soon as the companion mod registers it, and leaves when the router stops
listing it." While the filters hide every row the router lists, the card reads "No sessions kept by the filters" with
`{h} sessions hidden by the filters.`

A row's menu offers "View in analytics" and "View in requests" once the session has a ledgered request, each disabled
with "No ledgered request yet" on hover before it, and "View in routing log", which opens the log on that session's
decisions an hour either side of its last activity.

Opening a row reads, on a finished session, "The mod released every agent of this session. Each record leaves the engine
one cache TTL after its release, and a record that held a slot of the agent cap keeps it until then." first, then two
grids of facts side by side, one above the other where the open row is narrower than 620 px. Session reads Created
(`{time}, {ago}`, the first request the ledger holds of the session, which moves when the retention removes it, or
`unknown` where it holds none), Key (`{key} in pool {pool}`, the key a link to its row in the Keys view, or `{key},
deleted holder` on an earlier holder's), Client (the client version, the user agent on hover), Project (the whole path,
with `working directory {path}` under it where the working directory differs, or that line alone where the companion
reports a working directory and no project, else `not reported`), Repository (the whole `host/path`, or the repository's
root where it has no remote, else `not reported`), Branch (the branch, else `not reported`), Fork (the base cut to 13
characters and whole on hover, a link to its row where the list holds it, the depth and the relation to the base's
account, or `none`), Placements and Companion, the Companion column's badge with `seen {ago}` or the time since it went
silent. Routing reads Account (the link and `subscription` or `API key`, or `none`), Pinned since (the instant main's
pin was written on the account it stands on, else the row's pin's, so a move starts it again), Shed mark (the `leaving`
badge with the account the mark stands on and its window, or `none`), Moved (the stamp, a move that happened: `shed from
{account}` after a shedding round or `failed over from {account}` after a failover's landing, with the instant, or
`none`), Runway, Speed (the speed chip on a slowed session, else `full speed`) and Cycle (or `not measured yet`), and
its title carries "View in routing log", the row menu's link.

The Agents table under the facts lists one line per agent, main first (from its conversation pin or its recipe, or from
the row where neither names main), then the lines with a lifecycle word grouped by that word (`busy` with `registered`,
`idle`, `parked` with `unknown`, `released`), then the pins whose agent the mod does not report, inside each by the
agent's first request or the last activity as the control orders the rows. Agent reads the agent's label, which is
`main` for main, else the name its companion reports, else its description, else its id, cut with an ellipsis at about
32 characters, with the id under a label that is not the id. The label links to the request log on the session and the
agent once the session has ledgered traffic, and the hover reads the name, the description and the id, each on a line of
its own, ahead of the agent's last request and its pin's creation. Type reads the type the companion reports where it
reports one and the ledger's otherwise, breaking at its hyphen (`general-purpose`) where room runs out, and a dash on
main. Model reads the line's model, short with the id on hover, and the effort of its pin's last turn under it. Account
links to the line's account, its pin's or, where no pin stands, its recipe's, with its kind on hover, then the row's
speed chip and `leaving` badge where the line sits on the row's account and the account's speed chip elsewhere while it
is slowed, and reads `none` only where no pin stands and the recipe names no account. Context reads the pin's tokens,
Burn the pin's burn per hour under the Unit setting as the Cost cell's line reads the row's, and Requests and Cost the
agent's requests over every model it ran and their spend, read as the row's Cost cell reads the session's, its "+N"
naming the accounts other than the line's. Status reads the line's lifecycle word with its sentence and a parked agent's
park on hover, or `unreported` where no mod reports the agent or the session, and Cache reads the pin's `warm` or `cold`
with the recipe's state word under it, or `no pin`. Each line's menu offers "View in analytics", "View in requests" and
"View in routing log", each narrowed to the session and the agent, the routing log an hour either side of the agent's
last request, and an open one holds auto reload as a row's does. While an Agents filter stands, the table lists the
matching lines alone, and the earlier agents line gives way to a line under the table, `{k} agents hidden by the
filters`, which counts the row's other lines and its earlier agents and opens them under the matching lines.

The earlier agents, the ones the ledger holds requests of that draw no line, such as a subagent whose pin and recipe are
gone, share one line under the table, `{n} earlier agents · {k} requests · {cost}`, with a `+$` badge where they billed
dollars beside a plan figure and, on hover, each one's label with its id, `reviewer (a1b2)`, or the id alone where its
companion reported no name and no description. The line opens to one row per earlier agent in the table: its label, its
type, the one its companion reported ahead of the ledger's, the model and account of its newest request, its requests
and its cost, `no pin` in Status and dashes for a pin's figures, with the same menu. The Cache warming details end the
open row, shut by default with the recipes' states on their line (`1 retained, 1 paused, 1 ended`) or `no recipe`. Open,
they read one line per agent as Agent, TTL, Last request, Last probe, Next probe ("on first stream event" or "not
scheduled"), Probes / credited and State, Agent reading the agent's label as the Agents table names it, with its id on
hover. A pin no recipe stands on reads its TTL as the time left on its cache, counted from the pin's last refresh, its
last use as Last request, dashes in the probe cells and a State of `warm` or `cold` by its cache TTL from that refresh,
the word carrying its own sentence on hover. On a recipe's line State reads the recipe's state word (`ended`, `held`,
`not retained`, `paused` or `retained`) with the cause beside it, the state word and the cause each carrying the cause's
sentence on hover, and the line under them reads the mod's lifecycle word for the agent with a parked agent's park kind
and its deadline or wake time (`parked · background · deadline in 1h 12m`), then the state's sentence with the cause's.
A finished session, listed under Finished sessions, keeps its lines in their ended state with the TTL, Last request,
Last probe, Next probe and Probes / credited the engine last read, and its Status reads `finished`. A session without a
recipe reads "no recipe" and `Nothing is scheduled because {reason}.` inside the details, and so does a session whose
companion mod holds cache warming off, whatever its lines read.

At a phone's width, under 600 px, the session table scrolls sideways inside its card. An open row's detail stays as wide
as the card's visible part, its content 12 px in from each side, and keeps to the card's left edge while the table
scrolls under it. The Session facts stand above the Routing facts, the Agents table scrolls on its own at its full
width, and the lines under it wrap their hint under their label where the two do not fit side by side.

### The cost reference

The "Cost reference" link opens the dialog "Cost reference" under "What a million tokens costs: in dollars at list
price, and as a share of a subscription window." on three tabs, Tiers and peak band, Plan rates and API prices. The
Sessions view's head and the pool dialog's Warm sessions hint open it on Plan rates, the pool dialog's Write cost hint
on API prices, and the subtitle of the Dashboard's 7-day ring and the notes of the Burn section's three plan cards on
Tiers and peak band. It opens on the entries in force now. Its footer carries a "Revision" select over both files'
revision dates, newest first, each with its time where the entry carries one, a date not yet in force marked `(next)`,
and choosing a date redraws every tab at the entry each file had in force on that date, or at a file's first entry when
the date comes before it. A single date across the two files shows no select. The footer names each table's entry as a
chip, `API prices, effective {date}, shipped` (or `file`) and `Subscription cost model, effective {date}, shipped` (or
`file`), each date with its time where the entry carries one, the hover naming the key and the path that set it or
would replace it at the next start (`stats.api_prices_path`, `stats.subscription_costs_path`). The console has no
import control: a table is replaced by a file at that path, read at the next start.

![The cost reference dialog on Plan rates](assets/cost-reference-plan-rates-light.png#only-light)
![The cost reference dialog on Plan rates](assets/cost-reference-plan-rates-dark.png#only-dark)

Tiers and peak band draws Windows by tier, a row per tier the cost model sizes, named by its plan (Pro, Max, Ultra),
with its 5-hour and 7-day window as bars labeled as a multiple of the smallest tier's window, a Peak band switch that
redraws them for a request completing inside the band, and the Peak band card: a week, Monday to Sunday in the browser's
time zone, with the band's hours on each day it covers, a marker at now, a caption naming this week's hours here and in
the zone the cost model defines them in, whether the band runs now and until when, the next change of the local hours,
and each window's capacity factor. Plan rates draws the share of a tier's 5-hour and 7-day window that a token count of
each type spends, per model, for a request completing outside or inside the band, with a tier chooser (the anchor tier
Pro first, then the cost model's order), the band control and a token field in millions with quick picks for 100k, 1M
and 10M, a weight column per model, a Fable week row under a Fable model's 7-day group on a tier with Fable access and
none under Pro, and a warning tint on a cell past a full window. Under a regime of the cost model ([Plan
meter](statistics.md#plan-meter)), Windows by tier draws the regime's part first, a headline row naming the regime and
the clients it prices, "Interactive (Claude Code CLI, Claude Desktop, Claude VSCode)" for the shipped model, then a row
per tier the regime names, and the base rows under their own headline, "Headless (Claude Code CLI headless mode, the
Agent SDK, Claude in Slack, etc.)", the multiples against the base's smallest tier. The Peak band card's subtitle names
the traffic the band applies to and the sessions that hold the full window at every hour ("The band applies to Headless
traffic only: a request completing inside it spends more of its 5-hour window. Interactive sessions hold the full window
at every hour."), and its factor rows add one per regime that holds a window differently inside the band, named by its
headline word. Plan rates adds a Regime control beside Tier offering each regime's headline word and Headless for the
base rows, opening on Interactive where the model names it, the caption naming the shown headline and its clients. A
model the cost model does not weigh prices no window share, so its cells read dashes and its weight cell "not weighed".
API prices draws the dollar list prices per million tokens, input, cache read, cache write 5m, cache write 1h and
output, with a fast-mode row under each model that serves fast mode and a row under each model priced by prompt length,
under the caption "USD per million tokens at list price" and its note "An API-key account pays these. A subscription
account pays them in overage and for fast-mode answers." Both tables show one row per model tier, its newest model, in
the cost model's `tier_order` with `model_tier` naming each model's tier and a model of no tier last, and fold the older
models under one row at the table's foot that names their count, "1 older model" or `{n} older models`, and expands to
them in the same order. The fold is shared by both tables and starts closed at each opening. The price rows, the weights
and how a probe is priced are on [Pricing](statistics.md#pricing), which lists the shipped revision in force, and the
dialog shows an earlier entry under the Revision select.

## Settings

![The Settings view: Appearance, About and the client block](assets/settings-light.png#only-light)
![The Settings view: Appearance, About and the client block](assets/settings-dark.png#only-dark)

"Appearance" ("Stored in this browser only.") sets the Theme, Light, Dark or System ("System follows the operating
system preference."), the Palette, Default or GitHub ("GitHub follows the colors of github.com."), Default until you
pick GitHub, the Time format, 12h or 24h ("Applies to every timestamp and axis tick."), and the Unit, which mirrors the
top bar's switch with the week select beside it, disabled under the dollar positions as the top bar's is, and a legend
button of its own ("The unit every view prices a request in. Native reads a subscription request as a share of the
chosen week and a billed request in dollars."), the five browser preferences the shell reads, and at a phone's width
each row's control wraps under its label, the Unit row's position select, week select and legend button onto a second
line. "About" lists Version, Profile, Metrics listener, Ledger path, API prices path and Subscription costs path (each
as the source the router read, the file's path, `shipped table (no file at {path})` where a path is set and no file
stands there, or `shipped table (not configured)` where the key is unset, then its table's entry in force, its revision
count and the next date while one is pending, as `effective {date}, {n} revisions, next {date}`, each date with its time
where the entry carries one, the last reading `{problem}` in their place while the cost model reports a problem),
Storage path, "Requests, samples and the routing log" and "Rollups" (each as `kept for {n} days`, or "kept forever"
where its key is 0), "Body capture" (as `at most {n} hours`, the longest a pool or a key keeps captured bodies),
"Session expiry" (as `1d (86400 s)`, how long a companion session stays listed after the later of its last request and
its mod's last report), "Non-companion timeout" (as `1h (3600 s)`, how long a session no mod reports stays listed after
the latest of its last use, its last request and its end, or the session expiry where that is the lesser of the two,
which also bounds the lines its pins draw), "Reset warning" (as `5 days before a reset ends`, or "off" at 0, how long
before an unused limit reset ends the Dashboard raises its item and the Quota card's Resets chip takes the tone of a
plan about to end), and Admin secret as "configured" or "not configured", each path's, each retention line's, each
session line's and the reset warning's hover naming the bootstrap key that sets it, and closes on "The values the
configuration file sets are on the installation page, and `phoebis check-config` prints this router's."

"Downstream client configuration" opens on its Setup switch, which shows one block at a time: "Base URL", the default,
"Base URL and proxy" and "Base URL and DNS redirect". On "Base URL" the subtitle reads "Put it in the env block of the
client's settings file or export it in the shell that launches the client, and replace the token with a key from the
Keys page." and the block shows the seven lines that point Claude Code at the router, with "Copy snippet". The base URL
carries the proxy listener's scheme (`https` where the listener terminates TLS) and the port it binds, on the
placeholder host `router.example.com`, and the hint under the block reads `Inference is served on {proxy}, the address
the proxy listener binds, so the base URL carries its scheme and port. Replace the placeholder host with the name that
reaches the listener, and the scheme and the port too where a TLS proxy or a port mapping stands between Claude Code and
it.`

The two other blocks are the setups of a client that uses Artifacts and the claude.ai connectors through the router
([claude.ai features](claude-ai-features.md)). Each holds the eight input lines of the launch script on that page, in
the script's order: `BASE_URL`, `ROUTER_KEY`, `PHOEBIS_FEATURE_CA_SHA256`, `SETUP`, `PROXY_URL`, `PRIVATE_HOSTS`, `PLAN`
and `BASE_CA`. The router fills in the three values it knows: the base URL as the "Base URL" block builds it, the
feature CA's fingerprint, and the tab's setup, `proxy` under "Base URL and proxy" and `redirect` under "Base URL and DNS
redirect". The other five read as the script's head reads them: `ROUTER_KEY='<key>'`,
`PROXY_URL='http://127.0.0.1:8318'`, `PRIVATE_HOSTS='no'`, `PLAN='max'` and `BASE_CA=''`. Both blocks print
`PRIVATE_HOSTS='no'`, which the script reads under `SETUP=redirect` alone. A reader pastes the block over the eight
input lines at the script's head, which replaces them with their comments, and a paste that keeps `<key>` stops the
start ahead of the fetch. The subtitle there reads "Paste these lines over the inputs at the head of the launch script,
and replace the key with one from the Keys page." "Copy snippet" copies the block shown. Under the listener hint a
second hint reads "The launch script of the claude.ai features page reads these inputs. It fetches the CA, compares the
fingerprint, checks the road and writes the env file before it starts the client.", its first words a link to the
script, and under the redirect's block it adds "The machine's name resolution must lead api.anthropic.com and
mcp-proxy.anthropic.com to the router's feature listener on port 443. On Linux, PRIVATE_HOSTS set to yes leads both
names to loopback for one start, where a relay passes port 443 on to the router, so live updates work where the router
has a private address." The line "Feature CA fingerprint" follows with the fingerprint, "Copy fingerprint" and "Download
feature-ca.pem", which saves the CA's certificate from the settings answer as the file `feature-ca.pem`. On a router
whose configuration names no `features.listen` a feature block shows only "This router serves no feature call until its
configuration names features.listen." with a link to Installation. Before the settings arrive it shows a placeholder,
and after a failed read the error with "Try again", each with no subtitle and no copy control.

One definition per variable follows the block, and the list follows the shown block: its lines in the block's order,
then `CLAUDE_CODE_PROMPT_CACHE_TTL=5m`, which is defined under each setup although no block holds it. [The environment
block](client.md#the-environment-block) on The client explains each of the base block's seven lines, and [The
inputs](claude-ai-features.md#the-inputs) on claude.ai features the lines of a feature block. The view writes browser
preferences alone and changes no router setting.

## Routing log

![The Routing log view: a decision card open](assets/routing-log-light.png#only-light)
![The Routing log view: a decision card open](assets/routing-log-dark.png#only-dark)

The Routing log sits under the sidebar's Diagnostics heading, shown while the expert view is on or while either
Diagnostics view is open, and every link into the log opens it whatever the mode. It is one stream of routing decisions
and the events that explain them, newest first, 25 rows a page with First, Previous and Next. The rules a decision
follows are on [Pools, keys and routing](routing.md).

The filter bar holds one row: the range, Add filter, the Sort control, and at the right edge the split Refresh button
and the count of rows shown ("25 shown, more available"). The Sort control orders the log by Time, newest first at rest,
and Ascending reads the stream reversed whole, the read paging in that direction and a change of it returning to the
first page. The range offers the pills 1h (the default), 6h, 24h, 7d, 30d and Custom, with From and To in a popover as
on the Requests view. Add filter opens a "Filter by" field over the filters in three groups: Kinds, Routing (Account,
Key, Model, and Pool, which a decision matches by its request's pool and an event by its own, so an event that names no
pool, an account's own among them, is left out) and Subject (Agent, which the id matches exactly: a decision by its
request's agent, an event by its own, so an event that names no agent is left out, Request, and Session, which a prefix
matches). Every decision and event kind lists by default. The Kinds filter opens with eight presets above the kinds:
placements and pins, moves and holds, exhaustion and refusals, account state and windows, cooldowns and backoff,
keepalive, registry changes, router life cycle. While every kind is shown, a preset's click lists its kinds alone, so
keepalive reads the cache-warming traffic in one click. From there a preset checks or unchecks its members, two presets
combine, a hand edit after a preset is one click apart, and unchecking the last kind returns to every kind. Checking
Select all checks every kind and unchecking it clears every check while every kind still lists, so a kind's click from
there lists that kind alone and a preset's click its members alone. Each filter in force stands as a chip on a line
under the bar, as on the Requests view: a set of whole presets reads their names ("kinds: moves and holds"), any other
set of kinds reads their words ("kinds: moved, held, placed +5"), and the request chip's hover counts the request's
decisions and events ("1 decision, 0 events"). The widenings stand on each decision's card, after "Widen to": "this
minute", "this session" (an hour either side) and "this account" (a quarter hour either side). Every filter and the
direction live in the page's address (a preset range by its name as in `range=6h`, a custom range as `from` and `to`,
two UTC instants, the lists as comma lists), so a filtered view is a link and Back returns to the view before it. A
`kinds` value names exactly the kinds listed, so `kinds=probe` lists probe decisions alone. An empty range reads "No
decisions or events in this range." with "Widen the range or clear a filter."

Auto reload is off when the view opens, and its poll is the log's own: the split Refresh button's dropdown sets it to
Off, 5 s, 10 s, 30 s or 1 min, out of the page's address, and each read takes every row that arrived since the read
before, up to eight pages, a new row landing at the top in the accent tint. On any range whose rows fit on the page
shown, a row that lands below them draws on the next read, and where the range runs on past the page it stays for Next.
Under Ascending a new row lands where the order files it. Past eight pages a line reads "More rows arrived than auto
reload could fetch. Reload to read them.", and Next reads on from the last row the poll fetched while the log reads
newest first. Under Time ascending, Next after an overrun reads rows the poll already brought, and the rows the poll
left need the reload the line asks for. The poll waits while the tab is hidden, on an older page until you go back to
the first, and while a menu or popover under the bar is open, with the interval kept and the cause named in the
dropdown's title. A custom range whose end lay more than a minute in the past when the page took it has nothing new to
read, so the dropdown reads Off and stays disabled, while a link whose range ends at the moment it was made reads on,
each read refreshing the rows inside it.

A row reads Time (dated as on the Requests view, a decision's request beneath it as a link to the request's drawer),
Kind (a decision in a filled badge with `attempt 2/3` beneath a failover attempt, an event in an outlined one), Session
(the first eight characters, a link that opens the Sessions view on that session alone, under a chip `session` and with
Finished sessions on, with the agent beneath), Account (a move names the account it left and the one it landed on, and a
registry change the pool, the key or the account it edited), Model, By, Fact, and Details, which opens the request's
drawer. By names what decided the row in one chip: the rule that fired (`spent rule`), the rung that placed it
(`static rung`), the strategy and the tier's priority (`Pace P0`, whose hover is the Pace hint), where a hold waited
(`cohort`, `session pin`), `slow-down` for a request the slow-down held and for the renewal after its hold,
`rewrite in flight` for a request that waited for a moved pin's first head and for the renewal after it, what exhausted
the pool (`budget`, `cooldown`, `fast lane`, `spend cap`), the refusal (`client contract`, `client drift`,
`guardrail blocked`, `key revoked`), `no rule fired` for a renewal and `keepalive` for a probe's renewal, or an event's
reason (`poll on Fable` or `prediction on 5h` for a spent window, by the source that read it, `poll`, `head` or
`prediction`, and `on 5h` where the row names none, a cooldown's `429 on 5h` by the window it names, `429 retry-after`,
`fast lane 429` or a bare `429`, `404 on {model}` for a model's rejection and `403 on {model}` for its permission error,
a cleared mark's reason, `healed`, `moved` or `released`, a failed poll's status or `transport failure`, a cut stream's
code without its `stream_` stem, `cut`, `incomplete`, `stalled` or `undecodable`, `slow-down` for a slow-down's start
and end, `5h cleared` for a window read under its limit again, with the whole `5h cleared to 40% under its reset` on
hover). Fact gives the value that made it so as a muted label with its value, `gap`, `idle`, `prefix`, `until`, a held
request's `until 18:20` and `speed 0.5x`, the renewal's `held 42 s`, a slow-down start's `speed 0.5x` and an end's
`slowest 0.35x`, a move's `runway` on its destination (a span such as `2 h 10 min`, `none` or `unbounded`) followed by
`least bad` or `rewrite only` where the destination did not fit, or a renewal's window utilization, such as `5h 96%`,
where that window's projection reached its ceiling, or the wall where the round tested it. A verdict reads in one color
across the rows, the card and the strip: good when it served the request, warning when it changed its course, critical
for a refusal or an exhaustion, and neutral for none and no. Every account, key, pool, model, session and request a row
names is a link. Each column but Time keeps a set width and Time takes the rest of the card, so on a wide screen the
request id under the time reads whole, and a window narrower than the columns' total scrolls the table inside its card.

Every row opens beneath itself. A decision row loads its record and draws the decision card. Its verdict line names the
decision with its accounts and its pool as links ("Moved secondary to primary", "Attempt 2 of 2: placed on primary",
"Exhausted in jobs", "Held on nightly for the rewrite in flight of req_5b2e8d47-c19 until 11:03") and chips for the
tier, the rule or the rung, `held for the reset at 18:20` on a request that waited for a reset,
`held for the slow-down until 18:20` and `0.5x` on one the slow-down held, `held for the rewrite in flight` on one that
waited for a moved pin's first head, `reserved 10.4% of 5h` on a move or a placement that booked a reservation, the
pin's warmth and the cache verdict (`cache pending` before the attempt reports usage, and where no rung promised a
prefix the cache read over the read, the write and the input, `19.3K of 46.6K`, with the three counts on hover). On a
move whose answer listed a thinking block dropped for the account (the reasons `end_user_binding_mismatch` and
`organization_binding_mismatch`), the chip `account-bound thinking` follows the cache verdict with the count of those
blocks, `154 blocks`, and its hover lists each of those kinds with its blocks. "How the router decided" draws the walk
as a strip of stations in the engine's order, each with its verdict, the path the decision took in the accent, every
branch it did not take as a stub, a step the record does not carry dashed and reading "not run", and the ways out as
termini, Chosen, Held for a slot and Refused, the one reached filled in its tone. Three more ways out, Held for the
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
holds `realized`, in the order the engine applies them. Its Round group reads, beside the trigger's facts, each window's
wall (`wall 5h`, `wall 7d`, `wall Fable`), the instant the burn the projection carries reaches the window's whole share,
absent while the window burns nothing. On a member whose 7-day room stands under its Fable reservation the station also
reads `Fable reservation`, the reserved share, and while the reservation's round still waits, `under since Fable
reservation` and `wait Fable reservation`. A pin step's `credit issuer` chip names the account that issued the fallback
credit token the body carries, on which the request is placed ahead of the pin's renewal and the moves, under the credit
rule of [Routing](routing.md#selection). A classifier call's pin step draws no pin and names the turn the call judges,
that turn's account and where the call went, under the classifier rule there. A keepalive probe's walk carries a Probe
station. It reads the pinned member's `obstacle` where one stands, in the words `already tried`, `budget`, `budget
unknown`, `credits unavailable`, `in cooldown`, `in overage`, `not available`, `spend cap reached` or `subscription
ended`, or `no pin` where the probe found no live pin, and `tried yes` or `tried no` for whether the probe had tried
that member. Where the probe is answered on that standing, the Outcome's Answer row reads `status 429` for an obstacle a
wait clears, a budget, a cooldown, missing credits, overage or a spend cap, and `status 503` for any other.

"Candidates, by rank" lists every pool member with the chosen or held-on member tinted: Account with its plan beneath
(the plan the router resolved from the organization type and the tier, "Free" among its words, or the tier word of a
record from before), Tier (`plan P0`, `overage P0`, `paid P1`, with `paid` for an API key and for a usage-based seat),
5h, 7d and Fable as three lines each (the utilization, the reset, the reading's source and age), or on a usage-based
seat whose record carries its spend reading one cell across the three, `spend 41%` over its reset with `estimated` under
it where the router estimated the reset, Projection and Runway, In flight against the cap, the Pace score, Prefix, and
Verdict. A usage-based seat with no reading, its share lapsed or not yet read, draws the three "not reported" cells as
an account with no window does. The rows list the chosen, renewed or held-on member first, then the account that a move
or a failover's landing left and a move's other destinations, then the ranked members by rank, then the eligible ones
the strategy did not rank, then the ones a partition or a check dropped with their obstacle. Projection reads one line
per window the candidate reports, the projection at the window's horizon, `5h 99% at 18:20` where the horizon is the
reset and `7d 41% in 5 h` where it falls earlier, and Runway a span (`2 h 10 min`), `none` or `unbounded`, each a dash
for a candidate the gate did not read, and Runway a dash for a member that has no allowance left. The Projection cell's
hover names the demand where it differs from the burn the window sees, `demand 12.86 %/h over burn 6.43 %/h at 0.5x`,
with the candidate's slow-down factor. The Pace score stands under a header naming the window it read, `Spare rate,
7-day per hour` or `Spare rate, 7-day Fable per hour` (`Pace` where no subscription candidate names one, and `Pace,
7-day left per hour` on a version 1 record), each cell's hover reading `{pct} of the 7-day window left over {n} h to its
reset at {time}, less {burn} of burn: {rate}.` (or `to its subscription's end at {time}`), or `{pct} of the 7-day window
left, less {share} reserved for Fable, over {n} h to its reset at {time}, less {burn} of burn: {rate}.` where the
candidate carries a reservation, followed by `Inside its tier, the highest spare rate wins.` The Pick station names the
accounts whose scores tied, the runway that narrowed them where it did, and whether the draw chose among more than one,
over a table with a row per scored account. On a request of a model without a week of its own, the table carries a
"Fable reservation" column, a whole percent such as `12%` on each first-tier member that holds a reservation. Verdict
carries the one chip that decided it (`429 on 5h until 14:55`, `model 404 until 15:24 for claude-opus-5`, `budget $1.10
of $5.00`, `5h spent until 15:42`, `spend cap reached until 00:00`), whose hover says why the member was passed over, on
a spent member with the reset it waits for (`its 5h window spent until 15:42`, `at its spend cap until 00:00`), and
beside it a badge for a destination the gate read that did not fit, `rewrite only` or `least bad` in the warning tone
and `ending` neutral, or `window spent` in the Pools card's tone for a placement's member that has no allowance left. A
cooldown chip names the scope its words leave out after the reset, the model a cooldown holds. On the walk's exhausted
line a chip reads its member's row words and hover where the member's own obstacle carries the chip's name, such as `429
on 5h until 15:42`, a chip whose member's obstacle is another reads the partition's word where its name is a
partition's, such as `window spent` where a plan window or a model's week passed the member over, and the obstacle's
label otherwise, and an operator chip reads its member's state words whatever the row's obstacle is. Three folds,
"Request facts", "Pin and warmth" (a record that pins nothing reads why: classifier, not looked up or pinning off, and a
pin that carries them reads Cycle and Slowed until) and `Settings in force since {time}` (which a guardrail's attach to
or detach from the pool moves too, since the fence re-reads the pool's settings), stay closed until opened. In Pin and
warmth, a stamped pin's Shed mark names the account it left, `shed from` after a shedding round and `failed over from`
after a failover's landing, with the instant. The settings fold lists the pool's twenty-six settings in the card's order
with Load shedding and Slow-down reading `on` or `off`, Stretch and Hold as spans, Return to plan as its word and Return
dwell as a span or `at once` at 0, or "The key and its pool had left the registry". A refused record, which no pool
setting entered, draws no settings fold, a guardrail block's among them, whose Admission line and Answer row carry
`guardrail blocked` and the guardrail's name as a second chip. A record written before a setting existed reads `not
recorded` for it, Load shedding, 5-hour ceiling and Weekly ceiling on a version 1 record and the five slow-down and
return-to-plan settings on an older version 2 record. A record written before the router recorded Shed on the Fable
reservation draws no row for that setting, so it lists twenty-five. A version 1 record lists after its twenty-five, each
marked `(then)`, the three settings it carried that a later record does not. A record that carries the one ceiling a
pool held before its 5-hour and weekly ceilings split reads that fraction on both the 5-hour ceiling and the Weekly
ceiling rows. "Outcome" lists the pin written, the reservation (the account, the share per window and the expiry), the
cache-map entries the answer wrote (`pending` until it ends), the cache read with the TTL tier of its deepest entry,
over the promised prefix where the rung promised one and over the whole prompt, the read, the write and the input, where
it promised none, the hold, reading `for the reset at {time}` where the request waited for a reset, or `for the
slow-down until {time}` with the chips `speed 0.5x`, `stretch 42 min of 30 min`, `bound 2 min` and `cycle 20 s` where
the slow-down held it, or `for the rewrite in flight of {request} until {time}` with `bound 2 min` where it waited for a
moved pin's first head, a Cause on a failover's landing or a move with a cause (`429 on 5h` or `fast lane 429` for a
cooldown, `re-auth required by credential refused` for a mark), the answer an exhaustion or a refusal gave, and the
spend against a budget. With four entries or more, the cache-map row draws one chip per tier with its count and depth
range, such as `message ×11 depth 1 to 11`, then its three deepest message depths, and its control "All {n} entries"
lists every entry until "Latest 3" folds them again. The foot offers the three widenings after "Widen to", this minute,
this session and this account, names the record's version as `Record v{n}`, and "Copy as JSON" copies the record route's
answer whole. A record the retention purged reads "The record left with the retention", and one still loading "Loading
the record".

An event row opens to a panel of pairs: When to the second, the account and the pool as links, then the kind's own
pairs. An event that names a request ends with "Source request", which opens the log on that request a quarter hour
either side, and "Details", which opens the request's drawer. An account state change names who moved the state:
`credential refused`, `credit balance`, `funding retried`, `refresh failed`, `by hand` or `paired`, and a deleted
account's last row reads `deleted`. A pin moved panel names the account the pin left, the one it landed on, the reason
and what moved it, where a rule's gate or the landing's placement read the destination, Destination fit (`fits`,
`rewrite only`, `least bad`, or `window spent` for a landing on a member that has no allowance left, which draws no
Runway) and Runway (a span, `none` or `unbounded`), and on a failover's move its Cause, `429 on 5h` or `fast lane 429`
for a cooldown and `re-auth required by credential refused` for a mark, in the account state panel's words. A shedding
round's panel reads Started by, Trigger (`5h projection 104% at 18:20 over the wall` on the 5-hour window, which the
round tests against the wall at its reset, `7d projection 102% in 5 h over the wall` on a weekly window whose reset is
more than five hours away, which it tests against the pool's weekly ceiling, the wall at the default, `7d projection 96%
in 5 h over the ceiling 95%` on the same window of a pool that holds a weekly reserve, `5h stretch 42 min over the
threshold 30 min` for a round the stretch fired, followed by Demand, `12.0 %/h to the reset at 18:20`, or `7d room 30%
under the Fable reservation 32%` for a round the reservation fired), Fired by (`the Fable reservation`, `the projection`
or `the slow-down`), "Fable reservation" (the reserved share as a whole percent, on a round the reservation fired),
Excess (`9.5 %/h`, which a round the reservation fired reads under "Walked burn", the burn of the pins the round
walked), Cap with the target and the burn removed, Mark share (`10%`, the share of the burn left to shed that a pin's
own burn must cover before the round marks it), Unit, Burn average, Warm sessions, the Pins table (Session, Agent,
Warmth, Burn, Last prompt, Thinking dropped as the thinking the pin's move would drop, in tokens, which the round
weighed at the output's weight beside the rewrite, with a dash for a pin that states none and no column on a round
stored before the figure, Rewrite as the share of the chosen week the pin's rewrite takes, `0.111% Ultra week`, with
`compared on the 7-day window` on hover naming the window the pick compared on, or on an earlier build's round, which
names no unit, the figure as stored under its old label, Destination, Mark), Skipped (one chip per pin, its session and
its reason, `off window` for a pin whose model does not draw the window that fired, `Fable model` for a Fable pin of a
round the reservation fired, which the round leaves where it is, `ended` for a pin whose agent ended before the round
read it, `no burn` for a pin with no priced burn on the fired window, and `sliver` for a pin the round left warm under
its Mark share), Planned destinations, Outcome (`marked 2`, `nothing to mark: no runway`, `nothing to mark: sliver`, or,
for a round that walked no pin, the word of the pin it left out, such as `nothing to mark: off window`), Next round
after, Dwell and Survive a restart. A mark cleared panel reads the pin, Reason (`healed: the 5h projection read 91%
under the wall` where the heal's horizon was the window's reset or the account's end and at the five-hour bound on a
window whose ceiling is the wall, and `under the ceiling` at the five-hour bound on a window whose ceiling stands under
it, `healed: the stretch read 24 min under the threshold 30 min` for a mark the stretch made, `moved` or `released`),
Marked (when, on which account) and Standing for, and a reservation expired panel reads Request, Attempt, Cause,
Reserved (`5h 10.4%`, `7d 1.2%`), Held for and the Effect `the booked share left the account's prediction`. A slow-down
started panel reads Speed (`0.5x`), Stretch (`42 min of 30 min`, the threshold after `of`), Demand (`12.0 %/h`), Reset
and the Effect `requests of the account's sessions wait their cycle times the slow-down up to the hold`, and a slow-down
ended panel Lasted, Slowest (`0.35x`), Held (the waits of the span's holds summed, with the count of the holds beside
it, labeled "holds") and the Effect `requests of the account's sessions pass unheld again`. Held can pass Lasted, since
the span's holds can overlap and one hold can outlast the factor's return. The kinds and what each row records:

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
| limit reset claimed | An operator's use of a limit reset reached the upstream. The row names the grant, the request id, the upstream's result and reason as sent, the router's outcome (refused, unsettled or used), the windows it cleared, the count left and the HTTP status, or the transport error where no answer arrived. |
| mark cleared | A shedding mark left a pin: it moved, was released, or its account's window read under its line again, the wall or the ceiling, or its stretch under the threshold. |
| model permitted | An account served a model another account had rejected. |
| model rejected | An account answered 404 for a model, which cools that model on the account. |
| overage ended | An account left overage at its 7-day reset. |
| overage started | An account went into overage, so its requests draw on extra usage. |
| pin created | A placement pinned a session to an account. |
| pin expired | A pin was released, idle past the pin window or for the reasons it lists. |
| pin moved | A rule, a failover or the state of the account a live pin sat on moved a session's pin to another account, with the destination's fit and runway where a gate read it, the failover's cause, and for a replaced pin the reasons it left for. |
| pin released | A stored event of a session an operator released by hand. No build writes it any more. |
| plan ended | A profile read found the account's organization holding no plan, so the account is flagged plan ended, with the read's source (the profile read, or the usage 403 that made it due) and the profile's organization type, rate-limit tier and subscription status. |
| plan restored | The account's plan-ended flag cleared, with its source (a profile read that found a plan, or a pairing confirm) and the profile's organization type, rate-limit tier and subscription status. |
| poll backoff ended | The usage poll answered cleanly again after backing off, or the account's plan ended while a run was open. The panel reads Resumed under `clean_poll` and Ended by the plan's end, with the pause's instant, under `plan_ended`. |
| poll backoff started | The usage poll was rate limited, so decisions read the account's windows from response heads. |
| poll failed | The usage or the profile endpoint answered a passing fault, with the endpoint, the status or the transport failure and the text. |
| refresh failed | An access-token refresh failed. |
| refresh succeeded | An access-token refresh succeeded. |
| refresh uncertain | An access-token refresh ended without a clear answer. |
| registry changed | An operator changed an account, a guardrail, a key or a pool: a creation records the row's snapshot, an account's, a key's or a pool's update each changed field with its value before and after (a list, such as a pool's guardrails, as its items joined by commas, or `none` when empty), a pool's keepalive policy as its dotted fields, `keepalive.enabled`, `keepalive.max_idle_seconds`, `keepalive.max_parked_seconds` and `keepalive.scheduled_horizon_seconds`, each with its value before and after, a guardrail's update each changed field with its value before and after, its pools and keys among them, a deletion or a revocation nothing more, a change of a pool's members the new member list, a key's rotation the new secret's prefix with the instant the retired secret stops being accepted, and a pause or a resume the account's `enabled` with its value before and after. A change names the account or the key it changed, as it names a pool, so the Account and Key filters list it. |
| reservation expired | A request ended without usage or its booking turned an hour old, so the share it reserved left the destination's prediction. |
| router started | The router started and loaded its pins, shedding marks, window readings and affinity entries. |
| shedding round | A round ran on an account whose projection on a plan window read over the ceiling, or over the wall where the round tested it, at the window's horizon, or whose stretch stood over the slow-down's threshold for the safety wait, with the pins it marked or why it marked none, and kept its marks across a restart. |
| slow-down ended | An account's slow-down span closed: its factor read 1 again for the safety wait, the window it slowed for passed its reset, or its entry left at the purge, with the span's length, its lowest factor, the waits of its holds summed, which can pass the span's length, and the count of those holds. |
| slow-down started | An account's slow-down began: its factor read under 1, so requests of its sessions wait their cycle times the slow-down up to the hold. |
| stream interrupted | A stream ended short after its head, with the cut's code and the error's words. |
| warm prefix expired | A warm prefix's cache-map entry lapsed past its TTL. |
| wind-down started | An account's subscription end entered the pool's wind-down, so no fresh session starts on it. |
| window cleared | A quota window read under its limit again after a reading at the limit, with the readings before and after and the reset unchanged. |
| window dropped | A weekly quota window fell by more than 5 points under its unchanged reset, the trace a used limit reset or a plan change leaves, with the reading before, the reading after as the poll that wrote the event read it, the reset, and Plan changed where the account's plan or tier changed with the drop. |
| window reset | A quota window reset, with the reading before it and Read from, the usage sample or the clock walk that announced it. |
| window spent | A response head, the poll or the prediction read a quota window at its limit, once per reset cycle: a newer head or poll reading of the same cycle within two points of 1.0 after it neither clears nor repeats it, a head naming no reset, taken before the announced reset, is of the same cycle, and the cycle runs from the instant the wall was announced or, for a wall restored at a start, from the stored reading's instant. |

Other views open the log on what they show: a Requests row and its drawer on the request a quarter hour either side, the
Sessions row menu's "View in routing log" on the session's decisions an hour either side of its last activity, a pool
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
of a weekly one, under Ended, Rise, Predicted, Ratio and Requests, then, while windows stay unlisted, `{n} of {m}
windows, newest first` with "Show 25 more". A group's open state belongs to the page, so a reload or a link opens every
group shut.

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
