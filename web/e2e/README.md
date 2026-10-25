# Browser pass

`views.mjs` drives a headless Chromium over every view of the console in the light and the dark theme. It logs in
through the login screen, then for each theme visits the dashboard, quota, accounts (which lands on the first account
through a client-side redirect, since `/accounts` carries no account id), pools, keys, guardrails, request logs,
analytics (on its Traffic tab), sessions, settings, and the routing log, and in the mock pass the Analytics page once
more on its Cache warming tab and the Quota page once more on its Quota windows section. Before the dark theme it logs
out through the top bar's Log out control and logs in again on the screen drawn in that theme. Per view it waits for one
element, asserts the document title, records every console error and page error, and writes a full-page screenshot to
`$PHOEBIS_SCREENSHOT_DIR/<theme>/<view>.png`. Further layout assertions ride along, one function each in `views.mjs`, in
the order the file defines them (the rail's two levels (the indent, the gaps, the hairlines, a parent's color at rest
and under the pointer), the create marks with and without the refusal dots (a mark's size, weight and ink, 6 px after
its label on the label's line and short of the dot, at the pass's viewport and in the 720 px strip), the expert view's
discovery line (below the group's last field, in the hint's size and a rail parent's ink), the cell grid through an
expert field's wrapper (a half cell beside the plain one, a lone field the grid's width), the sidebar's brand row (the
mark's box 36 px square with the logo drawn 26 px square in it, the name 17 px at weight 600 on one line and inside the
sidebar's content box), the dashboard's attention line (one 22 px row in every state, each badge and the "+N more" badge
the pool card's strip badge on the row's one top edge, in the content box and clear of the tiles, with the story's
seventeen items folded behind the dashed "+N more" badge in the mock pass, the seeded exhaustion or the quiet line in
the router pass), the list the "+N more" badge opens (every item, one badge per row inside the box, 8 px under the badge
and 8 px inside the window, closed by Escape with the focus back on the badge), the attention line at the phone widths
from 360 to 414 px (the first row one 22 px row of the lead, the badges that fit and the fold badge, the Replay button
on a second row where they need more than the line, every part inside the line's box, the same draw on a second read),
the dashboard's stat grid at the pass's viewport, at 1024 by 768 and at 400 by 900 (all five tiles drawn, wrapping as
the grid's columns let them and none past its right edge, no tile's figure past its content box, the fifth tile across
the row's width, the tiles, their rows, the fifth tile's width and any figure past its tile recorded at each width), the
Dashboard's tiles at 1280 by 800 and 1440 by 900 (four to a row, the fourth a quarter of the row on the first tile's top
edge and the fifth across the row's width at the first tile's left edge, both recorded), the Dashboard's tile words (the
Estimated cost, 24h tile's subscription line a share of the 20x 7-day window, or `0%` on a day without subscription
cost, and its note a figure per hour, and the Refused, 24h tile's hover naming the router's own refusals beside the
Requests, 24h tile's hover, the two labels in one type), the top bar's controls (Log out and the theme control 32 px at
one top), the sidebar's Expert view row above the version line, the routing log's columns at 1280 by 800, 1440 by 900,
1920 by 1080 and 375 by 800 (the chevron 24 px, Kind 166, Session 70, By 152, Fact 189 and Link 64 px at every width,
Account 178 px under 1420 px and 146 px from it, Model at 192 px from 1420 px alone with the model beneath the account
under it, and Time the fill: on the three wide viewports the table its card's width with nothing to scroll sideways and
Time the card less the other columns, 147 px at 1280 and 1440, with no request link under a time cut at 1920, and at 375
the table at its 990 px minimum scrolling inside its wrap with the page's width the window's and its table card's, the
top bar's title on at most two lines inside the bar and its last control inside its padding, each column as wide as its
longest cell with no slack, the morning dated time under the 12-hour setting among Time's, a line cut on purpose behind
an ellipsis, and again under the 7-day range on the state changes with each dated Time cell on one line), an account
edit's Account cell under the 24-hour range on the registry and state changes (the link the cell reads off the change's
target, against the account link of a state change in type, ink, height, left edge and top, and against the pool's and
the key's links of the other registry rows in all but their left edge, which their muted word moves), a cleared window's
By chip under the 7-day range on the cleared and spent windows (against a spent window's chip in type, ink, height,
padding and top, the width its text needs recorded, and the By column as wide as its longest cell), the Shed mark of the
two pins the story stamps in the mock pass alone (on the next request of the shed session and of the failed-over
session, the origin chip opening on `shed from` or `failed over from`, against the chip of the instant beside it in box,
type, ink and top, its label against that chip's label in type and ink, and its text whole), the Diagnostics group
(neither its heading nor the Routing log link on the dashboard with the expert view off, both with the log open and the
link active), pool chips, the caching controls, the Load shedding switch against Migrate when spent, the Burn average
field's hint in the expert view (ending on the preset that unlocks the field, against the Window ceiling field's hint
beside it in type, ink, line height, margin, left edge and top, and as wide as its field), the Wind down and Retention
hints of the pool dialog (the Wind down hint naming the canceled subscription against the Return dwell hint beside it,
and the Retention hint naming the limit and the Settings page's About card against the capture switch's hint beside it,
each in type, ink and left edge), the load groups (the overage grace field against the window ceiling field, the
utilization bar's ceiling handle and the shedding preview line), the pool dialog's ceiling handle in the plain view (on
the track at the field's value, centered on the track and taller than it), the Active pins tile's line under its figure
(in the mock pass on the work card alone, "1 on a former member" inside its tile below the figure, in the labels' ink
and the type of the serving table's sub line at a normal weight), the contract member's Tier cell (mock pass: `metered
P1` in the text, face, size, weight and ink of the all pool's API-key member's), the pool card's serving table and its
seven tiles, its attention rows ending with their age (the row's last cell, on the detail's line in the detail's ink and
size, uncut and inside the card, at the pass's viewport and at 400 px), the serving table's columns (Account the fill),
the chevron of the pool dialog's Identity select (12 px square at a 1.5 stroke), the pool dialog's members table
(Account the fill, each Priority and budget input at least its column's 96, 140 or 150 px less the cell's padding) and
keys table (Key the fill), each read while its group shows, and in the mock pass's expert view its model rules table
(Model the fill, each select holding its widest option's text inside its padding), which the router's seed draws no
override for, the Create key dialog's Key group (the Name input, the Pool select and the footer's buttons 32 px, the
Notes field 72 px, each label above its control), the settings cards' order, the listener hint's gaps and size and
About's lines (their order, one type and one gap), About's three retention lines (the label's type and ink, the value's
ink and the row's height of the Storage path line above them, the label's and the value's type and ink of the Admin
secret line below them, and in the 720 px strip and at 400 px those types and inks again with the label column at most
half the list's width), the Settings page's Unit row at 1280 and 400 px (the legend's button on the select's line, and
its popover inside the viewport's width, at or below its button's bottom, uncovered, in the legend's 12 px type and
closed by Escape), the Appearance card's Theme, Time format and Unit rows at 400 px (each control under its label), the
cooling-down block's columns (Reason the fill), the Delete account button, the window bars on the dashboard and the
Accounts page (each bar's line inside its bar, a compact bar's projection in its reset's place as two parts, its share
and its horizon, and a whole bar's reset and projection as two parts, each part whole on its line), the usage-based
seat's spend bar there at 1280 and 400 by 800 (mock pass: its row as wide as a window triple's beside it, from its left
edge where the two share a column, the amount spent of the limit in its head, and its reset on one line inside the bar
with the estimate's word muted and its sentence on hover), the account lines, the Quota cards' poll lines (short of the
Refresh pill and inside the card, the backoff line in the serious ink, none wrapping, a failed poll's reason cut inside
its line and whole in its title where a listed card draws one), the Quota cards' chips and reset cells (each chip and
each reset cell without a projection on one line inside its column, a live label with its projection split between its
reset and its projection with each part on one line inside the cell, the window grid inside its card, an expired cell's
reset on its title where a listed card draws one) and each live window's pace mark (inside its meter and taller than the
track), the timeline's chips on the Quota windows section (each inside its column, a chip with a share in the background
and ink of the card chip of the same tone, primary's used fill at the share the page computed, the now line at the
chart's now share of primary's track within a pixel, each live bar's body and used fill its lane's color at two
strengths, and the label's ink at 4.5:1 or better over both for every color a lane can take, and in the mock pass the
contract lane's one chip, its amount left of the limit with the estimate's sentence on hover, from primary's chips' left
edge, of their height and at least their column's width), the timeline at 340 and 400 by 900 (each lane's head above its
track, the now line at the chart's now share on every track and its pill in the axis row, the day grid crossing no
lane's head and starting at the tracks' left, the chips wrapping inside the lane with none cut, a usage-based seat's one
chip on one row, every day cell's short labels whole, no bar's label cut and nothing scrolling sideways), the timeline
under the 12-hour setting in the 5-hour zoom at 340 and 400 by 900 and 1280 by 800 (mock pass, on loads of its own: as
many cells as the largest divisor of twelve whose cell holds "12 AM" in the sub's face, every cell's labels whole and a
short hour's full time on its title), the Quota cards' one height (the head's and the foot's rules at one offset on
every card), the cards leaving out the plan-ended account the list shows (mock pass: no card naming it, their count on
the bar, the first card against its neighbor on its row), every Quota card's meters at least 120 px wide, its window
cells inside its padding, its chip column as
wide as its widest chip and its reset column as wide as its widest label within its 150 px bound, a split label read by
its wider part, at 1280 by 800 with two cards in a row in the mock pass and in a window that sets two cards on the card
grid's floor (an expired line among its cells where a listed card draws one, and one card in a row 1 px narrower), and
at 375 by 800 and 390 by 844 with no sideways scroll on the page and each card inside the window (the reset label on a
row of its own there, so its column goes unread), the Quota cards' window rows at seven widths from 340 to 1280 px (from
900 px up each reset label beside a meter of 120 px or more, under 900 px on a row of its own under its meter, no label
cut and nothing scrolling sideways), the remaining cards, the error state, the lines under the quota rings, the recent
requests' model cells and their table's fit, the recent requests' Cost cell under All in dollars and Billed dollars
(mock pass: a subscription row's equivalent line in the size, line height and ink of the Tokens cell's second line, and
its dash under the nothing billed title), the Estimated cost, 24h tile under the mock's unpriced switch (mock pass, on
loads of its own: the subscription line's dash under the unpriced title, in the size, weight and ink of the billed
figure beside it and in the weight and ink of the Tokens, 24h tile's lone figure), the recent requests' columns at 1280
by 800 and 1440 by 900 (Time the fill, its scroll past the card recorded in the mock pass), the cost-cap rows, the
wasted-probe rows, the Cache warming tab's saving tiles under each unit position (Probe cost, Saved by probes, Net
saving and Kept warm on one row at one top, the two saving tiles in the Probe cost tile's box and value type, and the
Saved by probes chart in the box and title type of the card beside it), the Cost tab under the mock's unpriced switch
(mock pass, on loads of its own: the Cost tile's dash under the unpriced title in the type, ink and top of the Cost per
request tile's first figure, and the Cost by card's note on a line of its own under the subtitle's text, at its left
edge, in its type and ink, its text recorded), the burn cards (an odd last card spanning its row, and in the mock pass
four cards two by two at 1440 by 900 under the key grouping), the Errors by code card's subtitle (its text, in the type
and ink of the Refusals by code card's subtitle), the plan legend's estimated entry (a hollow swatch) and model unlisted
entry, the placement and budget tables, the Analytics tables' columns, each read on the tab that draws it (the Traffic
breakdown's group column the fill, Key on the misses-by-reason breakdown under its wrapped headers, Cache by model's
width recorded under native with every column inside its wrap at 1280 by 800 and in the mock pass a cost cell's billed
line drawn, Rung on the placements table, Pool and Account on the member budgets, whose Spent column is as wide as its
meter's 140 px floor, and the Accounts in range card's account column at least 132 px at 375 by 800 with the table
scrolling inside its wrap and the card as wide as the Pace card), the request table's fit (in both passes the default
columns inside the card at 1280 by 800 with the Cost column whole, Time the one fill taking the card less the other
columns, its floor the widest dated time the clock setting draws with the cell's padding, every id line the whole id cut
by the room alone with at least twelve characters in view, and no Client text cut, again under the 7-day range read a
day and an hour on, where every Time cell prints its date on one line inside its cell, and there under the 12-hour
setting, the table carrying its 12-hour class and, where the set runs past the card, scrolling inside its wrap with Time
at its floor), its sizes (at 1440 by 900 inside the card with every id whole in the mock pass, and at 375 by 800 at its
narrowest with Time at its floor, scrolling inside its wrap with the page as wide as the window), its columns (Time the
fill, the Status, Attempts and links cells wrapping), its text at 1920 by 1080 in the mock pass (no client class or
agent type cut, each read on its own span, and every id whole), the usage-based Cost cell (mock pass: `usage-based`
under its dollars in the tag, size, weight and ink of a billed row's funding word, its hover `Billed in dollars:
usage-based`), the table with Time turned off (`table--no-fill` at its content width at 1280 by 800 and 1920 by 1080),
the Cache warming grid (an odd last card spanning its row, its title in the type of the card before it), the Analytics
tiles rows and the Cache warming grid at 1280 by 800 and 1440 by 900 (no figure on the Cost and Cache warming tiles
rows, the Wasted probes card's among them, wider than its tile, a Cache warming card alone on its row spanning the grid
and a last card that shares its row on its neighbor's top edge, each figure and card recorded), the Traffic table
grouped by model (its Error rate column, and the no model row's Error rate a dash in the muted ink and in the type of
the rate cell beside it, which reads a percent, the row required in the mock pass alone since the router's seed holds no
request without a model), the request pages' widths, walked on a load of their own once every view's shot is taken, the
pagers of both logs (three buttons 32 px square on each, the request log's page-size select 32 px, each footer one
control tall with its padding and its border), the request log's range control (the group 32 px with its six pills 28 px
at one top, the group against the split Refresh button and the search field on height and top, the Custom pill's width
recorded), the speed chip, and the cache miss reason, effort and request class chips (each against the turn origin chip
at 26 px, on an address that holds them), the Cost split's rings under native (each center figure inside its ring, two
side by side in the mock pass), the native Cost by table (its width recorded, every column inside its wrap at 1280 by
800), the filter bar of every list view at 1280 by 800 and 1440 by 900, at rest, with the view's chips and, on a ranged
view, with a span across midnight (the row 32 px on one line with every control 32 px at one top and a segmented's pills
28 px, the bar 32 px at rest and 66 px with a chip line, 49 and 83 px as the Analytics sticky bar, each chip 26 px with
its word in `--ink-2` and its values in `--ink`, the chip line on one line, the first card's head below the bar, on a
card view the next element and the tiles below it, on Analytics the Group by select against Add filter, and on the
routing log and Analytics the width of the row's slots and gaps at rest under the view's bound, and at 950 by 800 on
Analytics and Requests the bar's scroll width at most its client width with the count inside the window, the trailing
slots wrapped, and at 400 by 800 on every view with a bar, at rest, the bar and the row no wider than their boxes with
the count where the view draws one and the Refresh button inside the window), the Kinds list through Add filter on the
routing log (Select all, the eight presets and the 44 kinds, its popover 8 px inside the window at 1280 by 800), the
split Refresh button on the request log (one width at Off, at 5 s and paused under the drawer, so the count beside it
never moves), a late row on the request log (in the mock pass, after the view's shot, the story's late request landing
below the first of the rows a page of 50 shows on the first auto reload at 5 s, every cell of it in the fill a top row's
cells take under the same tint, apart from the fill of an untinted row of its own stripe and in the box of the untinted
row above it, recorded as skipped in the router pass), the fresh tint on both logs (in the mock pass, after the view's
shot, the row the story lands a few seconds after the load, the request log's late request on a page of 50 and the
routing log's late event, drawn on the first auto reload at 5 s with every cell in `--accent-soft` over the surface,
apart from the row's resting fill and in its box, the one row in the tint, recorded as skipped in the router pass), the
traffic cards (each Analytics check opens the tab it reads and returns to Traffic), the tab strip docked under the
filter bar at the end of the Latency tab (the bar 49 px at rest and 83 px with a chip line, read on an address of its
own), the unit legend's lines in the top bar's popover at 1280 by 800 (Native's and Billed dollars' sentences naming the
four billed fundings, each in the face, size and ink of the line after it, its label at that label's weight), the Unit
select and the legend's button in the top bar (inside the bar, clear of Log out and the theme switch, inside the
viewport at 1280 and 400 px with nothing scrolling the bar sideways, every short label whole and no legend button at 400
px, the long labels whole at 1280 px, and the legend's popover over the filter bar uncovered at 1280 px), the top bar on
every view at 1280 and 400 px (nothing scrolling it sideways, its last control inside its padding, the title whole on
one line, and Log out a text button at 1280 px and its icon alone at 400 px), the reload banner under the mock's build
switch `phoebis.mockBuildMismatch` (mock pass, on loads of its own at 1280 by 800 and 400 by 800, scrolled 600 px down
the Analytics view: the banner's top edge at the window's top, where the bar's sits with the switch off, the bar's top
edge on the banner's bottom, the banner's paint opaque under its tint, the block's paint transparent with the switch off
and opaque with it on, the sticky filter bar on the block's bottom and the section bar on the filter bar's bottom, and
at 1280 by 800 the Accounts list 20 px under the block, each edge within 1 px), the anchored pool card and key row under
the reload banner (mock pass, on loads of its own under the mock's build switch, the Pools view at 1280 by 800 and the
Keys view at 1280 by 400: the first card's or row's top edge 16 px under the sticky block's bottom once the view moves
to its hash, within 1 px), the fast lane, the request drawer, which
opens once the request list's shot is taken (its compact table with Time the fill as wide as its card at 1280 by 800 and
1440 by 900 with every id whole in the mock pass, and at 375 by 800 under the list scrolling inside its card, its chips
in the status badge's type, and the Estimated cost row's 20x equivalent chips and muted equivalent on one line each
inside the cell at 1280 by 800 and 1440 by 900, the story's Fable request reading a 20x 5-hour, a 20x 7-day and a 20x
7-day Fable equivalent in that order), a reasons panel link landing on a list whose first row's drawer reads the reason
(mock pass, on a load of its own once every view's shot is taken), the find field of a captured body (after the wide
dialog's Escape, the trimmed text of a line the copied body first holds past a third of the box typed into the Request
block's field, the find row at the 400 px drawer inside its width with the field at least 8rem wide and the count and
its buttons on one line, and Enter opening the wide dialog with that row the one drawn active inside the rows in view,
the box scrolled off its top, the mark's fill apart from the row's tint and the focus in the dialog's field, in both
passes and recorded as skipped where the body holds no such line), the wide body view's width floor (mock pass, on a
load of its own once every view's shot is taken: the page an edge answered an upstream 502 with, indented with tabs,
opened wide, its block of rows no narrower than its widest row's text, which runs wider than the scroll box), the
routing log's first decision row, opened before the log's shot (Time the card less the other columns with the row's
panel open), the routing log's decision card, which opens once the log's shot is taken (the strip and the candidate
table inside the card at 1280 by 800 and 1440 by 900 with nothing scrolling sideways, the table 958 then 1118 px, the
strip's drawing as wide as the strip at each width, every station badge in the kind badge's box, the station labels
clear of the leaf rows and the brackets, every station's and way out's dot, the gap to its words and its word taking the
pointer for its own button and the line on either side of the dot and a station's side of the line away from its label
not taking it (in the mock pass on the shed move's card too, and there at the strip's 640 px floor for the dots and the
line, and on the story's eight cards of the gate and the reservation, the spent move held for the reset ending in the
fourth way out, Held for the reset), the strip's height from its rows, a moved row's badge the Move station's color, the
shed move's seventeen stations, the chosen row's Prefix chips on the first rung placement the log lists, and a station's
popover inside the card and the card's part on screen less 8 px either side at both viewports and at 700 by 800 (the
Move list, the widest, among them), visible with no transition once placed, its width bound the least of 720 px, the
window less 16, the card less 16 and the card's part on screen less 16, its table's heads on at most two lines, its
figures on one line outside the Fit table, which wraps them to fit, and nothing in it scrolling sideways (the shed
move's Fit table among them, its nine columns the projection, the rewrite and the reserved share on the 5-hour and the
7-day window, then the runway, the window that binds and the verdict, a Fable pin adding its Fable columns and an ending
account its end), and the Fit and Pick tables' rows striped as the candidate table's are, every station's and way out's
word at the card's muted contrast or better on the open row's tint in both themes with an idle way out among them, the
strip's station count the record's steps and placeholders at both viewports, and in the mock pass the fresh placement's
Pick station opened by Enter and by Space with the focus inside its popover and closed by Escape with the focus back on
the station), the tie's pick line (in the mock pass, on a load of its own once the log's shot is taken: the story's
failover that tied primary and nightly on Pace before nightly's overage, its Pick station's rows reading "tied primary",
"tied nightly", "by runway primary" and "drawn no", each in the type, line height and left edge of the strategy row
above it), the subscription end editor (its checkbox label reading Canceled in the type of the Ends label beside it),
the hint a confirmed pairing draws in the Usage card while the first reading is in flight (in the mock pass, after the
view's shot, read whole with its closing period against the note of the story's API-key account annex in type, ink, line
height, margin and place), the catalog popup and its columns (Model and Name the fills), the Keys table's columns (Name,
Models and Status the fills) and the badges of its fill cells (each ending inside its cell and cutting its own label
with the ellipsis), the key dialog's Access selects, the Keys bar's pool chip against its status chip (26 px, on an
address that holds both), the lifetime cost card under the mock's unpriced switch (mock pass, on loads of its own: the
flagged key's bar a dash beside its billed dollars under the unpriced title with no fill on its track, last in the card,
its figure in the type and ink of a neighbor's that reads a number and its track as wide), the Sessions lead and its
disclosure at 1280 by 800 and 1440 by 900 (the head under the bar's bottom, its height closed and open, the button on
the sentence's last line or flush left on the next, the opened paragraph 6 px under the lead, Cost reference one line at
the head's top right), the stripes on every view with a table (the first four data rows plain, `--stripe`, plain,
`--stripe`, a `table--plain` table plain, `--stripe` a tone of its own apart from no fill, the surface, `--surface-2`
and `--accent-soft`, an expanded Sessions row's detail and the Routing log's open panel on their parent's tone, the open
row itself on `--surface-3`, a striped Requests row on `--surface-2` under the pointer and on `--accent-soft` once
selected on every cell, and the pool dialog's form tables plain), the scroll lock behind a modal dialog (in the mock
pass, on the Pools page in a browser that shows scrollbars: a wheel over the page moves it, a wheel over the open
dialog's backdrop leaves the page's scroll and its content box where they stood, and the close returns the page to the
scroll it had), the cost reference, the session table's fit (inside the card at 1280 by 800 and 1440 by 900, and at 375
by 800 scrolling inside its wrap with Session at its 74 px floor and the page as wide as the window) and its columns
(each fit column as wide as its content, the Session column the fill), the session id's width, the session row menu, the
agent lines, the fork badge's face (under the session id, in the row's badge face), the fork badges (inside their cell
and whole), the session row's chevron (14 px square at the icon's 1.75 stroke), the Sessions tiles in error (mock pass,
on loads of its own under the mock's sessions failure switch: the Sessions and Agents tiles a dash over "could not load"
titled with the read's message, in the box, the value's line and the note's line of the Retained tile beside them, which
keeps its figure, and the notice under the tiles naming the sessions), the released session's line and the Agents tile's
hover, their words against the detail's type (mock pass, on a load of its own under the finished filter once the session
list's shot is taken), the fork link, which is followed once the session list's shot is taken, the copy fallback (Copied
on the Settings snippet with the clipboard API removed, on the Settings view's own load, and on a Key created dialog's
secret in the mock pass, on a load of its own), the login screen's Stay logged in row (on both logins, the light theme's
and the dark one's after the Log out: the secret field's, the row's, its box's, its label's and its hint's edges inside
the card recorded, asserting nothing)). A check that opens an editor, a dialog or a row closes it again before the
view's shot, and one that turns a column on turns it off again. The recent requests' table is fitted in the router pass
and recorded in the mock pass, whose story's subscription rows read their 5-hour equivalent in the Cost cell, wider than
the card leaves at 1280 by 800, so the table scrolls inside its wrap there, and its width under Window shares, where the
cell keeps its two-line shares, is recorded beside it. The script exits non-zero with the list of failures when any of
that does not hold.

The screenshot directory has to lie outside the repository, which the script enforces against every worktree `git
worktree list` reports, and against the tree holding the nearest `Cargo.toml` when git cannot answer.

The page runs on Playwright's clock: it starts at `PHOEBIS_E2E_CLOCK` (an ISO instant), or at the instant the pass
launches when that is unset, and stands still except where the script moves it, in fixed steps, while a view loads
and again before its shot. Animations are off and the time zone is UTC, so two passes over one tree at one clock
origin draw the same pixels.

## Run it against a running router

```sh
npx playwright install chromium
PHOEBIS_E2E_BASE=http://127.0.0.1:8081 PHOEBIS_E2E_SECRET=... PHOEBIS_SCREENSHOT_DIR=/tmp/phoebis-shots npm run e2e
```

`PHOEBIS_E2E_BASE` is the admin listener's URL and `PHOEBIS_E2E_SECRET` the admin secret. The browser install puts about
150 MB of Chromium under `~/.cache/ms-playwright` and is needed once per host.

`cargo test -p phoebis-router --test end_to_end the_ui_passes` runs the same script against a router it starts and seeds
itself, when `PHOEBIS_E2E_BROWSER=1` is set. Without it the test prints a skip line, so a build host without Chromium
still runs the rest of that suite. The router seeds its registry at real time, so the clock origin there is the
instant the pass launches.

## Run it against the mock

```sh
npx playwright install chromium
PHOEBIS_SCREENSHOT_DIR=/tmp/phoebis-mock-shots npm run e2e:mock
```

When `PHOEBIS_SCREENSHOT_DIR` is unset, the shots go to `phoebis-screenshots` under the system's temporary directory,
the directory the router's pass under `cargo test` defaults to as well.

`e2e:mock` builds the console with the fixture-backed mock into `dist-mock/` (`npm run build:mock`), serves that bundle
from a static server on a loopback port, and runs `mock-pass.mjs`, which drives `views.mjs` against it with the mock's
admin secret and the clock pinned at `2026-09-16T14:30:00Z`, the weekday afternoon the smoke run pins. The mock's random
draws are seeded, so two runs write the same screenshots byte for byte, which `diff -r` over the two directories shows.
Beside the views' shots it writes the open catalog popup's as `<theme>/accounts-catalog.png`, the open cost reference's
three tabs as `<theme>/cost-reference-tiers.png`, `cost-reference-plan-rates.png` and `cost-reference-api-prices.png`,
and the Analytics page's Burn tab, its 5-hour card shading the peak band, as `<theme>/analytics-burn.png`. Its views
include the Analytics page's Cache warming tab as `<theme>/analytics-cache-warming.png` and the Quota page's Quota
windows section as `<theme>/quota-windows.png`. The router's pass writes none of these: one shot per view it runs, which
its test counts. The dashboard's remaining cards and its empty state are checked against the router's seeded registry
alone (`PHOEBIS_E2E_MOCK=1` skips them), since the mock's story has subscription accounts and no empty list on the
dashboard. The subscription end editor needs the story's OAuth account, so against the router's seed that part stands
down.

## The API reference page

```sh
npm run build
npm run e2e:reference
```

`e2e:reference` serves `dist/` from a static server on a loopback port with a three-operation stub document and a stub
`GET /admin/session`, opens `/api-reference/` in headless Chromium in both color schemes, and asserts that the page
loads with no console error and no policy violation, lists the stub's tags and operations, renders none of Scalar's
controls that lead to a hosted service (the agent chat, the developer tools, the hosted client, the MCP generator),
shows an empty Authorization input, sends a typed token as `Authorization: Bearer` and shows the answer, makes every
request to the server's origin and no other, keeps the token out of `localStorage` and `sessionStorage`, sets no cookie,
and follows the browser's color scheme. With `PHOEBIS_E2E_BASE` and `PHOEBIS_E2E_SECRET` set it runs the same assertions
over a running router's real document. `cargo test -p phoebis-router --test end_to_end the_reference_page` reads the
page and the document through the router it starts, and runs this script against it when `PHOEBIS_E2E_BROWSER=1` is set.

## On a host without Chromium

```sh
docker run --rm --network host -v "$PWD":/work -w /work \
  -e PHOEBIS_E2E_BASE -e PHOEBIS_E2E_SECRET -e PHOEBIS_SCREENSHOT_DIR=/shots -v /tmp/phoebis-shots:/shots \
  mcr.microsoft.com/playwright:v1.63.0-noble npm run e2e
```

The image tag has to match the pinned version: Playwright looks its browser up under a revision directory that only
the matching image ships, and a mismatched one fails the launch with "Executable doesn't exist". The test suite uses
the host's own Chromium instead, because Chromium runs headless here and the image would add 3.5 GB to the test path
for no gain.
