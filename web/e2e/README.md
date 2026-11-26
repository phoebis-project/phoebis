# Browser pass

`views.mjs` drives a headless Chromium over every view of the console in the light and the dark theme, and in the mock
pass under the default and the GitHub palette. It logs in through the login screen, then for each theme under each
palette (the default palette first) visits the dashboard, quota, accounts (which lands on the first account through a
client-side redirect, since `/accounts` carries no account id), pools, keys, guardrails, request logs, analytics (on its
Traffic tab), sessions, settings, and the routing log, and in the mock pass the Analytics page once more on its Cache
warming tab, the Quota page once more on its Quota windows section and the Requests view once more on its Feature calls
section. Before each theme and palette after the first it logs out through the top bar's Log out control and logs in
again on the screen drawn in that theme and palette, storing `phoebis.palette` beside `phoebis.theme`. Per view it waits
for one element, asserts the document title, records every console error and page error, and writes a full-page
screenshot to `$PHOEBIS_SCREENSHOT_DIR/<theme>/<view>.png` under the default palette and to `<theme>-github/<view>.png`
under the GitHub one, so the mock pass fills the four directories `light`, `dark`, `light-github` and `dark-github`.
Each measurement and failure line leads with its directory's name, the bare theme or `<theme>-github`. Further layout
assertions ride along, one function each in `views.mjs`, in the order the file defines them (the rail's two levels (the
indent, the gaps, the hairlines, a parent's color at rest and under the pointer), the create marks with and without the
refusal dots (a mark's size, weight and ink, 6 px after its label on the label's line and short of the dot, at the
pass's viewport and in the 720 px strip), the expert view's discovery line (below the group's last field, in the hint's
size and a rail parent's ink), the cell grid through an expert field's wrapper (a half cell beside the plain one, a lone
field the grid's width), the sidebar's brand row (the mark's box 36 px square with the logo drawn 26 px square in it,
the name 17 px at weight 600 on one line and inside the sidebar's content box), the dashboard's attention line (one 22
px row in every state, each badge and the "+N more" badge the pool card's strip badge on the row's one top edge, in the
content box and clear of the tiles, with the story's twenty-six items folded behind the dashed "+N more" badge in the
mock pass, the seeded exhaustion or the quiet line in the router pass), the list the "+N more" badge opens (every item,
one badge per row inside the box's left and right edges, 8 px under the badge and 8 px inside the window, every badge
inside the box's top and bottom where the list fits, and where the box is capped at the room under the badge and
scrolls, its overflow-y auto or scroll, the first badge inside its top at rest and the last inside its bottom once
scrolled to its end, the box's client and scroll heights and its overflow-y recorded, the unpriced model's info badge
against the first warning badge in height, left edge and type, its ink on its fill contrasting less than the warning
badge's and at 4.5 to 1 or more, read here since the line folds the item at 1280 px and shows none at the phone widths,
closed by Escape with the focus back on the badge), the attention line at the phone widths from 360 to 414 px (the first
row one 22 px row of the lead, the badges that fit and the fold badge, the Replay button on a second row where they need
more than the line, every part inside the line's box, the same draw on a second read), the dashboard's stat grid at the
pass's viewport, at 1024 by 768 and at 400 by 900 (all five tiles drawn, wrapping as the grid's columns let them and
none past its right edge, no tile's figure past its content box, the fifth tile across the row's width, the tiles, their
rows, the fifth tile's width and any figure past its tile recorded at each width), the Dashboard's tiles at 360 by 800,
900 by 800, 1280 by 800 and 1440 by 900 (one to a row at 360, two at 900 and four at 1280 and 1440, the tile closing the
first row its share of the row on the first tile's top edge, the tile after it opening the next row at the first tile's
left edge, and the fifth across the row's width, every width recorded), the Estimated cost, 24h tile with the widest
week figure, "0.00998% of an Ultra week", written into its subscription line (mock pass, at the same four widths and at
511 by 800 and 1260 by 800: the figure inside the tile's content box, breaking between its words where the tile is
narrower, from the left edge and in the type of the billed line's figure, with the rows its line takes recorded, at 511
by 800, 1260 by 800, 1280 by 800 and 1440 by 900 the common "12.5% of an Ultra week" written in on one line of its own,
the dash restored after, and at 1280 by 800 the widths of both figures on one line at 15 px, which the day grid's width
comments cite as the common week figure and the wider form a smaller share draws, recorded), the Dashboard's tile words
(the Estimated cost, 24h tile's subscription line a share of an Ultra week, `0%` on a day without subscription cost, or
the dash under the unpriced title on the story's day, and its note a figure per hour, and the Refused, 24h tile's hover
naming the router's own refusals beside the Requests, 24h tile's hover, the two labels in one type), the Dashboard's
Sessions card (its subtitle, the Engine caps and Cache probes, 24h headings, the three caps each with its label and
figure on one line in the type and ink of the grid's labels and figures above them, the Agent records cap's hover on a
released record's slot, the probes line and the expired and bus gaps line in their forms, and the probe bar from the
hero's left edge to the card's content edge), the top bar's controls (Log out and the theme control 32 px at one top),
the week control beside the unit switch (at 1280 by 800 and 1440 by 900 the Week select between the Unit select and the
legend's button in the bar's right group, its three long labels whole, in the Unit select's height, top edge, type and
ink, at 900 by 800 and 360 by 800 its three short labels whole on the Unit select's line with no legend button at 360,
the bar one 52 px line at 1280, 1440 and 900 and 90 px over two lines at 360, disabled with its title under All in
dollars and enabled with none under Native, and on the Settings page's Unit row at 1280, 400 and 360 px the two selects
and the legend's button inside the Appearance card's content box, the week select on the position select's line to its
right at 1280 and 400, and the controls under the row's label at 400 and 360), the sidebar's Expert view row above the
version line, the routing log's columns at 1280 by 800, 1440 by 900, 1920 by 1080 and 375 by 800 (the chevron 24 px,
Kind 166, Session 70, By 152, Fact 189 and Link 64 px at every width, Account 178 px under 1420 px and 146 px from it,
Model at 192 px from 1420 px alone with the model beneath the account under it, and Time the fill: on the three wide
viewports the table its card's width with nothing to scroll sideways and Time the card less the other columns, 147 px at
1280 and 1440, with no request link under a time cut at 1920, and at 375 the table at its 990 px minimum scrolling
inside its wrap with the page's width the window's and its table card's, the top bar's title on at most two lines inside
the bar and its last control inside its padding, each column as wide as its longest cell with no slack, the morning
dated time under the 12-hour setting among Time's, a line cut on purpose behind an ellipsis, and again under the 7-day
range on the state changes with each dated Time cell on one line), an account edit's Account cell under the 24-hour
range on the registry and state changes (the link the cell reads off the change's target, against the account link of a
state change in type, ink, height, left edge and top, and against the pool's and the key's links of the other registry
rows in all but their left edge, which their muted word moves), a cleared window's By chip under the 7-day range on the
cleared and spent windows (against a spent window's chip in type, ink, height, padding and top, the width its text needs
recorded, and the By column as wide as its longest cell), the Shed mark of the two pins the story stamps in the mock
pass alone (on the next request of the shed session and of the failed-over session, the origin chip opening on `shed
from` or `failed over from`, against the chip of the instant beside it in box, type, ink and top, its label against that
chip's label in type and ink, and its text whole), the routing log's event panels (mock pass, under the 24-hour range on
their kinds: the pool edit's `keepalive idle window seconds` row against the settings row above it, sunset's backoff
close reading Ended by the plan's end with no Resumed row against its Backed off row in the label and its Attempts row
in the value, the window reset's Read from row against its Before the reset row, each in the label's type and left edge
and the value's type, and the guardrail record's two chips on its Answer row against the `key` chip on their line in
height, top edge and font size, every text recorded), the shedding round panel's Rewrite cells (mock pass, under the
24-hour range on the two rounds that mark pins: studio's 14:05 round, which names its rewrite unit, reading each marked
pin's Pro-week rewrite in the week the top bar's Week select names, titled with the 7-day window the pick compared on,
and secondary's 11:00 round, an earlier build's, reading each stored figure under its 20x label with no title, each cell
against its row's Burn cell in type and top edge, the texts and titles recorded), the usage-based seat's Pace hover in
the decision card (mock pass: on the seat's record that carries no spend window, the spend sentence its pace's shape
names, equal to the hover on its record that carries the window, both recorded), the Diagnostics group (neither its
heading nor a link of it on the dashboard with the expert view off, and with the Routing log or the Meter view open the
heading over the two links in that order and the open view's link active), the Meter view's group tables at 1280 by 800,
1440 by 900, 1920 by 1080 and 375 by 800 (mock pass: each fit column as wide as its widest cell past the 30 px gutter,
Account the fill at its 132 px floor or wider, each table inside its wrap on the three wide viewports, and at 375 the
document as wide as the window with each table scrolling inside its wrap and the first Latest ratio cell in view, a
collapsed line 37.8 px at 1280 by 800 badged or plain, an open group's height recorded, that group on an even line with
its nested rows alternating two tones, and the nested windows table's fill column free of the group column's floor),
pool chips, the caching controls, the Load shedding switch against Migrate when spent, the Burn average field's hint in
the expert view (ending on the preset that unlocks the field, against the 5-hour ceiling field's hint in the cell above
it in type, ink, line height, margin and left edge, as wide as its field, and the Weekly ceiling field's hint, the other
half of its row, starting at the top of the 5-hour ceiling's hint), the Wind down and Retention hints of the pool dialog
(the Wind down hint naming the canceled subscription against the Return dwell hint beside it, and the Retention hint
naming the limit and the Settings page's About card against the capture switch's hint beside it, each in type, ink and
left edge), the load groups (the overage grace field against the 5-hour ceiling field, the utilization bar's 5-hour
ceiling handle and the shedding preview line), the pool dialog's two ceiling handles in the plain view (each on its own
track at its field's value, centered on the track and taller than it, the first track's lowest mark above the second
track's highest with a focused handle's ring counted on both, and an arrow key on the weekly handle moving the weekly
field by one point and leaving the 5-hour one as it was), the pool dialog's return rate range (mock pass: its select
against the Strategy select in height and font size, beside the "Return rate by idle age" label, their vertical centers
within 1 px, opening on "7 days", the hint's text recorded), the pool dialog's claude.ai features group (mock pass: on
the story's `all`, the Artifacts and Connectors selects against the Strategy select in height and font size, reading
`studio (work)` and `reserve (no pool)`, and the dialog's shot at rest), the Active pins tile's line under its figure
(in the mock pass on the work card alone, "1 on a former member" inside its tile below the figure, in the labels' ink
and the type of the serving table's sub line at a normal weight), the contract member's Tier cell (mock pass: `metered
P1` in the text, face, size, weight and ink of the all pool's API-key member's), the pool card's serving table and its
seven tiles, its attention rows ending with their age (the row's last cell, on the detail's line in the detail's ink and
size, uncut and inside the card, at the pass's viewport and at 400 px), each pool card's tiles and the Pools view's tile
row under Dollar equivalent at 1200 by 800 and 1280 by 800 (in each card and in the tile row the figures of one row of
tiles on one top within half a pixel, each tile's label, the rows its label takes and the tops of its box and its figure
recorded, and in the mock pass a tile labeled Dollar equivalent in each card), the pool card's head at 400 and 360 px
(the Edit and Delete buttons one group on the title's line, the group's vertical center within a pixel of the name's, or
on a line of their own with the group's right edge at the title's, Delete after Edit on one line, both inside the
title's box, and in the mock pass the named pools' group on the title's line at 400 px), the serving table's columns
(Account the fill), the chevron of the pool dialog's Identity select (12 px square at a 1.5 stroke), the pool dialog's
members table (Account the fill, each Priority and budget input at least its column's 96, 140 or 150 px less the cell's
padding, and in the plain view a Priority at 0 that holds the unparsable `1e` and reads no bad input once its member is
unticked) and keys table (Key the fill), each read while its group shows, and in the mock pass's expert view its model
rules table (Model the fill, each select holding its widest option's text inside its padding), which the router's seed
draws no override for, the Create key dialog's Key group (the Name input, the Pool select and the footer's buttons 32
px, the Notes field 72 px, each label above its control), the settings cards' order, the listener hint's gaps and size
and About's lines (their order, one type and one gap), About's three retention lines (the label's type and ink, the
value's ink and the row's height of the Storage path line above them, the label's and the value's type and ink of the
Admin secret line below them, and in the 720 px strip and at 400 px those types and inks again with the label column at
most half the list's width), About's Session expiry and Non-companion timeout lines (the label's type and ink, the
value's type, the row's height and the label's and the value's left edges of the Body capture line above them, each
line's text and hover recorded), the client card's two feature blocks (mock pass: each shown through the Setup switch,
its block, the listener hint, the feature hint, the fingerprint's line and the variable list in that order, 10 px apart
under the block and each hint and 14 px over the list, the fingerprint's line inside the card's content box at 1280 px
and in the phone window, the switch's top at one place below the card's top under each of the three tabs at both sizes,
every figure recorded, the switch ending on Base URL at rest), the Settings page's Unit row at 1280 and 400 px (the
legend's button on the select's line, and its popover inside the viewport's width, at or below its button's bottom,
uncovered, in the legend's 12 px type and closed by Escape), the Appearance card's Theme, Palette, Time format and Unit
rows at 400 px (each control under its label), the cooling-down block's columns (Reason the fill), the Delete account
button, the deletion's dialog of an account in use (mock pass: on the story's `reserve`, the dialog inside the window,
its three uses each on a row of its own inside the dialog's body and Delete enabled once both lists answered, every
figure recorded, closed by Cancel with nothing deleted), the window bars on the dashboard and the Accounts page (each
bar's line inside its bar, a compact bar's projection in its reset's place as two parts, its share and its horizon, a
whole bar's reset and projection as two parts, each part whole on its line, and a window the account does not carry
named muted in its slot's share place, on the line and in the ink of the neighboring bar's label (its own label where
the 400 px window stacks the slots), whole inside its slot with the window's words on its title, a whole slot drawing
them and a compact one its short form, at 1280 by 800 and, after the view's shot, in a 400 px window, mock pass: the
approved Ultra account's "no 5-hour window"), the usage-based seat's spend bar there at 1280 and 400 by 800 (mock pass:
its row as wide as a window triple's beside it, from its left edge where the two share a column, the amount spent of the
limit in its head, and its reset on one line inside the bar with the estimate's word muted and its sentence on hover),
the approved Ultra account's own Accounts page (mock pass, at 1280 by 800 after the view's shot: its Usage card's whole
slot reading "no 5-hour window" muted in the share's place, the words on its title, read as the triples' absent slots
are), the plan badges' words on the seat's row and the plan-ended account's row against the first other row's, the
Accounts bar's sort control (its button in the height, top edge, type size, weight and ink of the Add filter button
beside it, and once opened its popover inside the window under the button, its eight radio rows one height, a mark on
the checked key and direction alone, and Escape closing it with the focus back on the button), the Quota bar's sort
control at rest on Default order (its button on the top edge and in the height of the Refresh all pill beside it,
reading "Sort: Default order" titled "Sorted by Default order" with no arrow, and once opened its two direction rows
disabled and unchecked, both in the Add filter list's disabled row ink, and Escape closing the popover), the Pools bar's
sort control after the view's shot (its button reading "Sort: Name" titled "Sorted by Name, ascending" in the height,
top edge and weight of the Create pool button beside it, a primary button whose larger type and ink differ by design,
and once opened its popover inside the window under the button, and Escape closing it), the Keys bar's sort control
after the view's shot (its button reading "Sort: Name" titled "Sorted by Name, ascending" in the height, top edge, type
size, weight and ink of the Add filter button beside it, once opened its popover inside the window under the button and
Escape closing it, and under Requests, 7d the first row's week line under its name in the type size and ink of the row's
last-use line), the Guardrails bar's sort control after the view's shot (its button reading "Sort: Name" titled "Sorted
by Name, ascending" in the height, top edge and weight of the Create guardrail button beside it, a primary button whose
type size and ink differ by design, once opened its popover inside the window under the button and Escape closing it,
and under Created the first row's date under its name in the type size and ink of the row's blocked count), the
Analytics bar's sort control beside the bar's measure (on the Traffic tab its button reading "Sort: Name" with the up
arrow, titled "Sorted by Name, ascending", in the height, top edge, type size, weight and ink of the Add filter button
beside it, once opened its popover inside the window under the button and Escape closing it, and on the Burn tab, which
has no group table to order, the button disabled in its slot, titled "This tab's tables keep their own order"), the
Requests and Routing log bars' sort controls beside the bar's measure (on the bare log the button reading "Sort: Time"
with the down arrow, titled "Sorted by Time, descending", in the height, top edge, type size, weight and ink of the Add
filter button beside it, once opened its popover inside the window under the button, its radios the view's keys and the
two directions in one height, a mark on Time and Descending alone, and Escape closing it with the focus back on the
button), the Accounts detail's allowance grid at 1280 by 800, 1440 by 900 and 375 by 800 (mock pass: "Allowance left at
reset" and "Meter drift" stacked across the pane's content box at 1280 and 375 px, each as wide as the Usage and
Requests cards and in their title's type, and at 1440 px side by side on one top, as wide as each other and as the Usage
card and each under half the pane, the grid inside the pane with nothing in it scrolling sideways and every card's right
edge inside it), the plan-ended account's Identity list (mock pass: its Plan value `Free` alone, the plan resolved from
the document its profile read found, against its Plan ended value in type, left edge and row height, both texts
recorded), the plan-ended sentence (mock pass: on the plan-ended account's Dashboard card and Accounts list row the
compact `No window since the plan ended {ago}`, muted, from the left edge inside its card of the nearest neighbor's
window bars and in the type size and ink of their first label, both quota rings naming the usage-based seat with its
reason and the Fable ring the Pro seeds without a Fable week, and on its Accounts detail the Usage card's long sentence
in the ink of the Identity card's note, the two sizes recorded, each text recorded), the account lines, the Quota cards'
poll lines (short of the foot's leftmost control, Use reset where the card carries it and else the Refresh pill, and
inside the card, the backoff line in the serious ink, none wrapping, a failed poll's reason cut inside its line and
whole in its title where a listed card draws one, and in a 375 by 800 window each card's lines, their heights and the
control's left edge recorded, and beside Use reset a lone failed poll line's status box, words and reason recorded, the
reason's first line fragment wider than 0, inside the status box and the two drawn lines, and the line no taller than
two line heights, failing where it reads no line height in pixels (mock pass: the `workshop` card)), the Quota cards'
chips and reset cells (each chip and each reset cell without a projection on one line inside its column, a live label
with its projection split between its reset and its projection with each part on one line inside the cell, the window
grid inside its card, an expired cell's reset on its title where a listed card draws one) and each live window's pace
mark (inside its meter and taller than the track) and the chip row in a 375 by 800 window (mock pass: the story's
`secondary` card, whose chip row holds the Plan chip, the plan's end, the trial and a Resets chip, draws every chip of
that row inside the row's content box, its plan's end whole and no Resets chip) and the Resets chip's short form in that
window (mock pass: on the story's `nightly` and `workshop` cards the long form drawing no box and the short form drawing
one, `nightly`'s reading "1 ends in 4 days" and `workshop`'s "1 cooldown until 15:10"), the Quota cards' extra usage
flag (mock pass: its ink on its fill on nightly's card at 4.5 to 1 or more in each theme under each palette), the
timeline's chips on the Quota windows section (each inside its column, a chip with a share in the background and ink of
the card chip of the same tone, primary's used fill at the share the page computed, the now line at the chart's now
share of primary's track within a pixel, each live bar's body and used fill its lane's color at two strengths, and the
label's ink at 4.5:1 or better over both for every color a lane can take, and in the mock pass the contract lane's one
chip, its amount left of the limit with the estimate's sentence on hover, from primary's chips' left edge, of their
height and at least their column's width), the timeline at 340 and 400 by 900 (each lane's head above its track, the now
line at the chart's now share on every track and its pill in the axis row, the day grid crossing no lane's head and
starting at the tracks' left, the chips wrapping inside the lane with none cut, a usage-based seat's one chip on one
row, every day cell's short labels whole, no bar's label cut and nothing scrolling sideways), the timeline under the
12-hour setting in the 5-hour zoom at 340 and 400 by 900 and 1280 by 800 (mock pass, on loads of its own: as many cells
as the largest divisor of twelve whose cell holds "12 AM" in the sub's face, every cell's labels whole and a short
hour's full time on its title), the Quota cards' one height (the head's and the foot's rules at one offset on every
card, and in a 375 by 800 window the same figures recorded and nothing compared), the reset dialog's walk (mock pass: on
the story's `primary`, Back returns to the dialog's first page with the focus on the first row's Use reset, the close
through Cancel leaves the focus on the card's Use reset, and so does the close of a second opening whose click left the
focus on the card, and the walk sends no claim), the cards leaving out the plan-ended account the list shows (mock pass:
no card naming it, their count on the bar, the first card against its neighbor on its row, and a window the account does
not carry kept as a row, its words in the muted ink in the reset label's place, in the type size of a live row's label
and to its right edge, beside a chip naming the window alone with the words on its title), every Quota card's meters at
least 120 px wide, its window cells inside its padding, its chip column as wide as its widest chip and its reset column
as wide as its widest label within its 150 px bound, a split label read by its wider part, at 1280 by 800 with two cards
in a row in the mock pass and in a window that sets two cards on the card grid's floor (an expired line among its cells
where a listed card draws one, and one card in a row 1 px narrower), and at 375 by 800 and 390 by 844 with no sideways
scroll on the page and each card inside the window (the reset label on a row of its own there, so its column goes
unread), the Quota cards' window rows at seven widths from 340 to 1280 px (from 900 px up each reset label beside a
meter of 120 px or more, under 900 px on a row of its own under its meter, no label cut and nothing scrolling sideways),
the 7-day and Fable week remaining cards, the error state, the lines and the left-out lists under the quota rings (mock
pass: both lines and the Fable ring's list drawn, each across the ring's grid from its card title's left edge), the
recent requests' model cells and their table's fit, the recent requests' Cost cell under All in dollars and Billed
dollars (mock pass: a subscription row's equivalent line in the size, line height and ink of the Tokens cell's second
line, and its dash under the nothing billed title), the Estimated cost, 24h tile on the story's own unpriced rows (mock
pass, on a load of its own: the subscription line's dash under the unpriced title, in the size, weight and ink of the
billed figure beside it and in the weight and ink of the Tokens, 24h tile's lone figure), the recent requests' columns
(Time the fill, the table inside its card at 1280 by 800, at 1440 by 900 and at 1503 by 800), the cost-cap rows, the
wasted-probe rows, the Cache warming tab's saving tiles under each unit position (Probe cost, Saved by probes, Net
saving and Kept warm on one row at one top, the two saving tiles in the Probe cost tile's box and value type, and the
Saved by probes chart in the box and title type of the card beside it), the Cost tab on the story's own unpriced rows
(mock pass, on loads of its own: the Cost tile's dash under the unpriced title in the type, ink and top of the Cost per
request tile's first figure, and the Cost by card's note on a line of its own under the subtitle's text, at its left
edge, in its type and ink, its text recorded), the burn cards (Plan per hour, 5-hour per hour, Fable week per hour and
Spend per minute against Failovers, and in the mock pass the four cards two to a row on two top edges at 1440 by 900,
each half the row less half its gap, under the key grouping and under the Accounts filter naming the account whose
traffic prices nothing), the Errors by code card's subtitle (its text, in the type and ink of the Refusals by code
card's subtitle), the plan legend's model unlisted entry (and no estimated entry), the placement and budget tables (the
Burn section's cards closing on Member budgets after the charts), the Unused allowance by account card (mock pass: below
Accounts in range, its columns under the width rule with Account the fill at 1280 by 800, and its Account column at 375
by 800 at least 132 px with a 132 px min-width, as wide as Accounts in range's), a week priced at its own tier in the
Ultra week (mock pass: on the studio account's detail the allowance block's third-newest 7-day week, run under Pro, in
the type size, weight and right edge of the second-newest week's figure and from the head figure's right edge, its
figures and its total recorded, and on the Analytics view's Accounts tab the unused allowance card's studio row's 7-day
figure in range under 30 days, the block's total, in the type size, weight and right edge of the next row's), the
Analytics tables' columns, each read on the tab that draws it (the Traffic breakdown's group column the fill, Key on the
misses-by-reason breakdown under its wrapped headers, Cache by model's width recorded under native with every column
inside its wrap at 1280 by 800 and in the mock pass a cost cell's billed line drawn, Rung on the placements table, Pool
and Account on the member budgets, whose Spent column is as wide as its meter's 140 px floor, and the Accounts in range
card's account column at least 132 px at 375 by 800 with the table scrolling inside its wrap and the card as wide as the
Pace card), the request table's fit (in both passes the default columns inside the card at 1280 by 800 with the Cost
column whole, Time the one fill taking the card less the other columns, its floor the widest dated time the clock
setting draws with the cell's padding, every id line the whole id cut by the room alone with at least twelve characters
in view, and no Client text cut, every Time cell printing its date whatever the row's age, again under the 7-day range
read a day and an hour on, with every Time cell on one line inside its cell, and there under the 12-hour setting, the
table carrying its 12-hour class and, where the set runs past the card, scrolling inside its wrap with Time at its
floor), its sizes (at 1440 by 900 inside the card with every id whole in the mock pass, and at 375 by 800 at its
narrowest with Time at its floor, scrolling inside its wrap with the page as wide as the window), its columns (Time the
fill, the Status, Attempts and links cells wrapping), its text at 1920 by 1080 in the mock pass (no client class or
agent type cut, each read on its own span, and every id whole), the usage-based Cost cell (mock pass: `usage-based`
under its dollars in the tag, size, weight and ink of a billed row's funding word, its hover `Billed in dollars:
usage-based`), the Status cell's refusal chip against the code's badge (mock pass, on the refused answers' list: the
`cyber` chip in the badge's font size and height from its left edge and under it at the cell's gap, its text and the
upstream's hover recorded), the Agent filter's chip on both logs and Analytics (mock pass, on a story session's address
with `agent_id=main`: the `agent: main` chip against the `session:` chip in height, top edge and its values' font family
and size, its text recorded, and no `user agent:` chip), the table with Time turned off (`table--no-fill` at its content
width at 1280 by 800 and 1920 by 1080), the Cache warming grid (an odd last card spanning its row, its title in the type
of the card before it), the Analytics tiles rows and the Cache warming grid at 1280 by 800 and 1440 by 900 (no figure on
the Cost and Cache warming tiles rows, the Wasted probes card's among them, wider than its tile, each row holding a
figure in the Ultra week in the mock pass under the Accounts filter naming nightly, a Cache warming card alone on its
row spanning the grid and a last card that shares its row on its neighbor's top edge, each figure and card recorded),
the Traffic table grouped by model (its Error rate column, and the no model row's Error rate a dash in the muted ink and
in the type of the rate cell beside it, which reads a percent, the row required in the mock pass alone since the
router's seed holds no request without a model), the request pages' widths, walked on a load of their own in the part
behind the walks, the pagers of both logs (three buttons 32 px square on each, the request log's page-size select 32 px,
each footer one control tall with its padding and its border), the request log's range control (the group 32 px with its
six pills 28 px at one top, the group against the split Refresh button on height and the search field on height and top,
the Custom pill's width recorded), the speed chip, and the cache miss reason, effort and request class chips (each
against the turn origin chip at 26 px, on an address that holds them), the Cost split's rings under native (each center
figure inside its ring, the billed ring alone in the mock pass, whose range prices its plan rows nothing), the native
Cost by table (its width recorded, every column inside its wrap at 1280 by 800), the filter bar of every list view at
1280 by 800 and 1440 by 900, at rest, with the view's chips, on a ranged view with a span across midnight, on Requests
under "Sort: Output tokens" and on Sessions under "Sort: Cost" with a search that keeps one row (the row 32 px on one
line with every control 32 px at one top and a segmented's pills 28 px, the bar 32 px at rest and 66 px with a chip
line, 49 and 83 px as the Analytics sticky bar, the Requests row under the mock on two lines at 1280 by 800 in every
case, since a hidden sizer with the widest count of a freshly read page holds the count's slot at that width whatever
the count reads (the slots 1283 px in the 992 px row, so the edge wraps alone onto the second line, the row 72 px, the
bar 72 px at rest, across midnight and under the sort and 106 px with its chips), and at 1440 by 900 on one line at
rest, with its chips and under the sort (the slots 1152 px in the 1152 px row) and on two across midnight (the Custom
pill's span taking the slots to 1443 px in the 1152 px row, the row and the bar 72 px), the Sessions row under the mock
on two lines at 1280 by 800 in every case too, since a hidden sizer with three digits in each figure of its count holds
the count's slot at that width whatever the count and the Sort button read (the slots 1278 px in the 992 px row, so the
edge wraps alone onto the second line, the row and the bar 72 px at rest and under the sort), and at 1440 by 900 on one
line at rest and under the sort (the slots 1152 px in the 1152 px row), under another story on two lines where its slots
run past the row and on one line otherwise, the Sort button reading its pick under the sort and, in the mock pass, the
Sessions count reading `1 session · 2 agents` there, each chip 26 px with its word in `--ink-2` and its values in
`--ink`, the chip line on one line, the first card's head below the bar, on a card view the next element and the tiles
below it, on Analytics the Group by select against Add filter, and on the routing log and Analytics the width of the
row's slots and gaps at rest under the view's bound, and at 950 by 800 on Analytics and Requests the bar's scroll width
at most its client width with the count inside the window, the trailing slots wrapped, and at 400 by 800 on every view
with a bar, at rest, the bar and the row no wider than their boxes with the count where the view draws one and the
Refresh button inside the window), the Kinds list through Add filter on the routing log (Select all, the eight presets
and the 46 kinds, its popover 8 px inside the window at 1280 by 800), the split Refresh button on the request log (one
width at Off, at 5 s and paused under the drawer, so the count beside it never moves), a late row on the request log (in
the mock pass, after the view's shot, the story's late request landing below the first of the rows a page of 100 shows
on the first auto reload at 5 s, every cell of it in the fill a top row's cells take under the same tint, apart from the
fill of an untinted row of its own stripe and in the box of the untinted row above it, recorded as skipped in the router
pass), the fresh tint on both logs (in the mock pass, after the view's shot, the row the story lands a few seconds after
the load, the request log's late request on a page of 100 and the routing log's late event, drawn on the first auto
reload at 5 s with every cell in `--accent-soft` over the surface, apart from the row's resting fill and in its box, the
one row in the tint, recorded as skipped in the router pass), the traffic cards (Errors by code against Requests by
outcome, the Refusals by category card against Refusals by code, Failovers beside Migration rewrites and Effort levels
beside Requests by speed, and each Analytics check opens the tab it reads and returns to Traffic), the widest count axis
tick (8.88M in the place and type of a Requests by outcome tick on Traffic at 1280 by 800, at most 1 px wider than its
room from the left edge of the svg that clips it to the tick's right edge, with both recorded), the Refusals by category
card under a client pattern (mock pass, on loads of its own: under `claude-code/cli` the first row's count against the
next row's in font size, weight and right edge with its link carrying the pattern as `client`, and under
`claude-code/sdk*` a count below the bare page's, each read recorded beside the bare page's), the Client filter's typed
field and a picked value's chip (mock pass, on an address with the Account filter: the field under the list's rows in
their left edge and width with its top below the last row's bottom, and the picked `claude-code/cli` chip against the
account chip in height, top edge and its values' font family, its text recorded), the Analytics bars at 375 by 800 (mock
pass, on loads of its own: under a custom range of 14 days each drawn bar of Requests by outcome at least 2 px wide with
no outline, a figure that rests on the plot of the 375 px viewport, where a bucket's share draws 3 px and a gap in
pixels leaves the bar the floor's 1 px, under 24 hours each of its bars outlined, and on the Cache tab under 30 days
each drawn bar of Write cost vs read cost at least 1 px wide, each read's count of bars, narrowest and widest width and
strokes recorded beside the same read of Requests by outcome at 1280 by 800), the tab strip docked under the filter bar
at the end of the Latency tab (the bar 49 px at rest and 83 px with a chip line, read on an address of its own), the
unit legend's lines in the top bar's popover at 1280 by 800 (one line per position in the select's order and the closing
paragraph after them, Native's and Billed dollars' sentences naming the four billed fundings, each in the face, size and
ink of the line after it, its label at that label's weight), the Unit select and the legend's button in the top bar
(inside the bar, clear of the week control, Log out and the theme switch, inside the viewport at 1280 and 400 px with
nothing scrolling the bar sideways, every short label whole and no legend button at 400 px, the long labels whole at
1280 px, and the legend's popover over the filter bar uncovered at 1280 px), the top bar on every view at 1440, 1280,
900, 400 and 360 px (no sideways scroll, its last control inside its padding, the title whole on one line, and Log out a
text button at 1280 px and its icon alone at 400 px), every view at 360 by 800 (the document as wide as the window, the
widest element recorded), the reload banner under the mock's build switch `phoebis.mockBuildMismatch` (mock pass, on
loads of its own at 1280 by 800 and 400 by 800, scrolled 600 px down the Analytics view: the banner's top edge at the
window's top, where the bar's sits with the switch off, the bar's top edge on the banner's bottom, the banner's paint
opaque under its tint, the block's paint transparent with the switch off and opaque with it on, the sticky filter bar on
the block's bottom and the section bar on the filter bar's bottom, and at 1280 by 800 the Accounts list 20 px under the
block, each edge within 1 px), the Dropped thinking blocks card on the Cache tab (mock pass, on a load of its own, whose
story holds the account key and the prefix key at the pass's origin: the bar of
`thinking_dropped.end_user_binding_mismatch` in the fault ink and the bar of `thinking_dropped.prefix_binding_mismatch`
in the muted ink, each ink read off a probe, each bar's label, count and fill recorded), the anchored pool card and key
row under the reload banner (mock pass, on loads of its own under the mock's build switch, the Pools view at 1280 by 800
and the Keys view at 1280 by 400: the first card's or row's top edge 16 px under the sticky block's bottom once the view
moves to its hash, within 0.5 px), the fast lane, the request drawer, which opens once the request list's shot is taken
(its compact table with Time the fill as wide as its card at 1280 by 800 and 1440 by 900 with every id whole in the mock
pass, and at 375 by 800 under the list scrolling inside its card, its chips in the status badge's type, and the
Estimated cost row's figure chip and muted equivalent on one line each inside the cell and its hint's chips on a row of
their own under it, each on one line inside that row's one cell across the table, at 1280 by 800 and 1440 by 900, the
story's Fable request reading its figure in the Ultra week alone on the cost row, then its plan, and a 5-hour, a 7-day
and a Fable week share in that order, and scrolled up to 600 px at 1280 by 800 the drawer's top at or under the section
bar's bottom, the scroll recorded in both passes, where a page that does not scroll fails the mock pass alone and the
router pass's two requests can leave it too short to), the Accounts list at the page's end (scrolled to its end at 1280
by 800: the list's top at or under its dock, the dock, whether the list fills its greatest height, the list's height and
greatest height, the scroll and the top recorded in both passes, where a list short of its greatest height or a page
that does not scroll fails the mock pass alone), the Requests view's Feature calls section (mock pass, on its first page
at 1280 by 800: the table as wide as its card with Time the one fill, every Path cell on one line with its whole path in
its title, a cut path drawn at 48 characters with the ellipsis, the cut paths recorded, and its pager's three buttons 32
px square with the footer one control tall), a reasons panel link landing on a list whose first row's drawer reads the
reason (mock pass, on a load of its own in the part behind the walks), the attempt count's hover card at the request
log's bottom (mock pass, on the log read at 50 rows a page, where the story's last row of two or more attempts sits
below the window's fold, the bare log loaded again after: that row scrolled to the viewport's bottom edge and its count
hovered, the card inside the viewport and uncovered at its center with its last badge ending inside it, the card drawn
again on the focused count and gone after a 60 px scroll, each read recorded), the find field of a captured body (after
the wide dialog's Escape, the trimmed text of a line the copied body first holds past a third of the box typed into the
Request block's field, the find row at the 400 px drawer inside its width with the field at least 8rem wide and the
count and its buttons on one line, and Enter opening the wide dialog with that row the one drawn active inside the rows
in view, the box scrolled off its top, the mark's fill apart from the row's tint and the focus in the dialog's field, in
both passes and recorded as skipped where the body holds no such line), the wide body view's width floor (mock pass, on
a load of its own in the part behind the walks: the page an edge answered an upstream 502 with, indented with tabs,
opened wide, its block of rows no narrower than its widest row's text, which runs wider than the scroll box), the
routing log's newest renewal of the hour in the mock pass and its first decision row in the router pass, opened before
the log's shot (Time the card less the other columns with the row's panel open), the routing log's decision card, which
opens once the log's shot is taken (the strip and the candidate table inside the card at 1280 by 800 and 1440 by 900
with nothing scrolling sideways, the table 958 then 1118 px, the strip's drawing as wide as the strip at each width,
every station badge in the kind badge's box, the station labels clear of the leaf rows and the brackets, every station's
and way out's dot, the gap to its words and its word taking the pointer for its own button and the line on either side
of the dot and a station's side of the line away from its label not taking it (in the mock pass on the shed move's card
too, and there at the strip's 640 px floor for the dots and the line, and on the story's eight cards of the gate and the
reservation, the spent move held for the reset ending in the fourth way out, Held for the reset), the strip's height
from its rows, a moved row's badge the Move station's color, the shed move's seventeen stations, the chosen row's Prefix
chips on the first rung placement the log lists, and a station's popover inside the card and the card's part on screen
less 8 px either side at both viewports and at 700 by 800 (the Move list, the widest, among them), visible with no
transition once placed, its width bound the least of 720 px, the window less 16, the card less 16 and the card's part on
screen less 16, its table's heads on at most two lines, its figures on one line outside the Fit table, which wraps them
to fit, and nothing in it scrolling sideways (the shed move's Fit table among them, its nine columns the projection, the
rewrite and the reserved share on the 5-hour and the 7-day window, then the runway, the window that binds and the
verdict, a Fable pin adding its Fable columns and an ending account its end), and the Fit and Pick tables' rows striped
as the candidate table's are, every station's and way out's word at the card's muted contrast or better on the open
row's tint in both themes with an idle way out among them, the strip's station count the record's steps and placeholders
at both viewports, and in the mock pass the fresh placement's Pick station opened by Enter and by Space with the focus
inside its popover and closed by Escape with the focus back on the station), the tie's pick line (in the mock pass, on a
load of its own once the log's shot is taken: the story's failover that tied primary and nightly on Pace before
nightly's overage, its Pick station's rows reading "tied primary", "tied nightly", "by runway primary" and "drawn no",
each in the type, line height and left edge of the strategy row above it), the candidate table of a placement whose
candidate projects three windows (in the mock pass, on a load of its own once the log's shot is taken: the story's
subagent placement on primary two minutes before the pass's instant, a Projection cell of its card holding a line for
each of the three windows, the table inside its wrap at 1280 by 800 with nothing scrolling sideways, each column's width
recorded), the subscription end editor (its checkbox label reading Canceled in the type of the Ends label beside it),
the hint a confirmed pairing draws in the Usage card while the first reading is in flight (in the mock pass, after the
view's shot, read whole with its closing period against the note of the story's API-key account annex in type, ink, line
height, margin and place), the catalog popup and its columns (Model and Name the fills), the Keys table's columns (Name,
Models and Status the fills) and the badges of its fill cells (each ending inside its cell and cutting its own label
with the ellipsis), the key dialog's Access selects, the key dialog's claude.ai features group (mock pass: on the
story's `laptop`, the Artifacts and Connectors selects against the Expiry select in box and type, reading `contract
(contract)` and `Inherit (reserve)`), the Keys bar's pool chip against its status chip (26 px, on an address that holds
both), the lifetime cost card on the story (mock pass, on a load of its own: each key whose requests priced nothing
reads a dash beside its billed dollars under the unpriced title, the first one's figure in the type and ink of a
neighbor's that reads a number and its track as wide), the Sessions lead and its disclosure at 1280 by 800 and 1440 by
900 (the head under the bar's bottom, its height closed and open, the button on the sentence's last line or flush left
on the next, the opened paragraph 6 px under the lead and above the card's top, Cost reference one line at the head's
top right), the stripes on every view with a table (the first four data rows plain, `--stripe`, plain, `--stripe`, a
`table--plain` table plain, `--stripe` a tone of its own apart from no fill, the surface, `--surface-2` and
`--accent-soft`, an expanded Sessions row's detail and the Routing log's open panel on their parent's tone, the open row
itself on `--surface-3`, in the mock pass the written session's Agents table opened under a plain and under a striped
session row (its first four rows showing the table's own `--surface`, then `--stripe`, `--surface` and `--stripe` behind
every cell, its eleven heads closing on the menu's, and a type with a hyphen breaking after it in the table's narrowest
layout), a striped Requests row on `--surface-2` under the pointer and on `--accent-soft` once selected on every cell,
the grouped Sessions table under `?by=repository` (its header rows out of the count and plain, its first four session
rows plain, `--stripe`, plain and `--stripe`, its first row's detail plain once opened, each group's name at the left of
a session row's chevron, the card's 12 px edge, and every value the grouped view cuts, a group's name and a session
row's label among them, cut with the ellipsis, the whole grouped read recorded as skipped in the router pass where the
seed draws fewer than four session rows), and the pool dialog's form tables plain), the scroll lock behind a modal
dialog (in the mock pass, on the Pools page in a browser that shows scrollbars: a wheel over the page moves it, a wheel
over the open dialog's backdrop leaves the page's scroll and its content box where they stood, and the close returns the
page to the scroll it had), the cost reference, its headline words (mock pass, at 1280 by 800: the tabs reading Tiers
and peak band, Plan rates and API prices, the Windows by tier group rows reading the Interactive title, then the
Headless title, the first one's cell in the type size of the table's first head and heavier, both weights recorded, the
Peak band card's subtitle reading the shipped band sentence after "This week.", Plan rates' pressed Regime option
reading Interactive and its Tier chooser's first option reading Pro, pressed and in the Regime control's first option's
height, top edge and type, the caption recorded), the session table's fit (the nine default columns and the menu's
inside the card at 1280 by 800 and 1440 by 900 with a slowed session's speed chip inside its Account cell, at 1440 by
900 every column the Columns control lists turned on, the head reading each of them in the control's order with Context
among them and the menu's last and the page as wide as the window, the table's and the wrap's widths recorded, and after
a load that draws the defaults again, at 375 by 800 scrolling inside its wrap with Session at its 74 px floor and the
page as wide as the window) and its columns (each fit column as wide as its content, the Model and Cost cells' badges
wrapping under their figure and the Account cell's chips under the account's name, the Session column the fill), the
session id's width, the session row menu, the agent lines of the Cache warming details (each opened on every row Expand
all opens, at 1280 by 800 and 1440 by 900: the head's, main's and the widest agent label's cells on one left edge per
column within 1 px on the session whose grid holds that label, seven columns closing on State, every Agent cell reading
on one line the label the Agents table of its row gives that agent, with the agent's id on its title, the widest label
as wide as the Agent track at its shown width, a cut label counted at its bound, each state badge inside its grid's
scroll width at 1280 px and inside its visible box at 1440 px, where no grid scrolls, the widest grid's scroll and
client widths recorded, the expanded table inside its wrap at both widths, the wrap's scroll and client widths recorded,
and the first text line of the Model, Requests and Last used cells of the first row whose Status reads unreported on its
session id's line, its top and its height), the Agents tables (on every row Expand all opens at 1280 by 800, each
earlier agents line opened: each table inside its detail panel's client width with its eleven heads closing on the
menu's, its head cells in the type size, weight and ink of the session table's, the table whose narrowest layout is
widest recorded with that width, and every value the expanded view cuts, the session rows' labels and the Agent labels
of its tables and of its agent lines among them, cut with the ellipsis), the agent menu (mock pass, at 1280 by 800 on
the open row of a session whose Agents table lists main alone: the open list's last item takes the pointer at its
center, so the table's wrap clips none of the list), the open row's facts (the written session's row in the mock pass
and the first row in the router's: the Session and Routing grids on one row of tops at 1280 by 800, and one above the
other on one left edge at 599 by 800), the open row at a phone's 375 by 800 (ending on a load of the bare view: its
panel as wide as the session table's wrap and at the wrap's left edge before and after the wrap scrolls 200 px, the
Session facts above the Routing facts, and the Agents table scrolling in a wrap of its own under a sideways wheel that
leaves the session table's wrap where it stood), the Sessions bar's sort control and its order (on loads of its own that
end on the bare view: the Sort button in the height, top edge, type size and weight of the bar's Columns control,
reading "Sort: Created" on the bare address and "Sort: Last activity" under `?sort=activity`, the first five rows'
session ids recorded under each), the Sessions fold line (on a load of its own under `?model=claude-sonnet-5` that ends
on the bare view: every row Expand all opens listing the lines on that model alone, a row whose other agents the filter
hides reading the fold line's count, its toggle in the type size, weight and ink of the Cache warming line's, and the
model chip's `(any agent)` suffix in the type size and ink of the chip's word), the fork badge's face (under the session
id, in the face of the Status badges), the fork badges (inside their cell and whole), the row's badges (on the first row
whose Status and Cache cells both draw one and whose Status cell draws its presence line, else the first with both, the
two in one face and height, and the Agents, Requests, Cost and Cache cells' sub lines in the type of the Status cell's),
the badges' tones (on the bare view `busy` accent, `idle` and `warm` good, `parked`, `registered`, `cold`, `+$` and "+N"
neutral, the `silent` sub line in `--warning-ink`, the `unreported` and `no pin` words in the ink of the bar's muted
count, and the fork badge carrying `.srow__fork`), the session row's chevron (14 px square at the icon's 1.75 stroke),
the Sessions read error (mock pass, on loads of its own under the mock's sessions failure switch: the notice 12 px under
the bar naming the sessions with the read's message, and the card's error state, whose Try again the notice's matches in
height, type size and weight), a finished session's line, found by its Status, the first line of its open detail, its
words against the detail's type (mock pass, on a load of its own under the finished filter once the session list's shot
is taken), the fork link, which is followed once the session list's shot is taken, the copy fallback (Copied on the
Settings snippet with the clipboard API removed, on the Settings view's own load, and on a Key created dialog's secret
in the mock pass, on a load of its own), the login screen's palette and its Stay logged in row (on every login, the
first one's and each one's after a Log out: the root's `data-palette` against the stored palette, since the theme script
alone sets it on that screen, then the secret field's, the row's, its box's, its label's and its hint's edges inside the
card recorded, asserting nothing)). A check that opens an editor, a dialog or a row closes it again before the view's
shot, and one that turns a column on turns it off again. The script exits non-zero with the list of failures when any of
that does not hold.

The screenshot directory has to lie outside the repository, which the script enforces against every worktree `git
worktree list` reports, and against the tree holding the nearest `Cargo.toml` when git cannot answer.

The page runs on Playwright's clock: it starts at `PHOEBIS_E2E_CLOCK` (an ISO instant), or at the instant the pass
launches when that is unset, and stands still except where the script moves it, in fixed steps, while a view loads and
again before its shot. A step starts once the page has had two frames with no request in flight, so a view that loads a
module waits for the module's file however late it arrives. A request that a navigation cancels stops counting once the
main frame commits the new document, where it started before that document's own request, since Playwright reports
neither its end nor its failure. A request that has not landed after 30 s fails the pass with its address, and the later
steps wait for the frames alone. Animations are off and the time zone is UTC, so two passes over one tree at one clock
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
Beside the views' shots it writes the open catalog popup's as `<theme>/accounts-catalog.png`, the pool dialog open on
its claude.ai features group as `<theme>/pools-features.png`, the open cost reference's three tabs as
`<theme>/cost-reference-tiers.png`, `cost-reference-plan-rates.png` and `cost-reference-api-prices.png`, the Analytics
page's Burn tab, its plan card drawing its lines, as `<theme>/analytics-burn.png`, the Quota view's reset dialog on its
two pages as `<theme>/quota-reset-pick.png` and `<theme>/quota-reset-confirm.png`, and the written session's open row on
the Sessions view as `<theme>/sessions-open.png` and again at 375 by 800 as `<theme>/sessions-phone.png`. Its views
include the Analytics page's Cache warming tab as `<theme>/analytics-cache-warming.png`, the Quota page's Quota windows
section as `<theme>/quota-windows.png` and the Requests view's Feature calls section as
`<theme>/requests-feature-calls.png`. The mock pass writes each of these shots under both palettes, `<theme>/` for the
default and `<theme>-github/` for the GitHub one, and the site's screenshots come from the default palette's
directories. The router's pass writes none of these: one shot per view it runs, under the default palette alone, which
its test counts. The dashboard's remaining cards and its empty state are checked against the router's seeded registry
alone (`PHOEBIS_E2E_MOCK=1` skips them), since the mock's story has subscription accounts and no empty list on the
dashboard. The subscription end editor needs the story's OAuth account, so against the router's seed that part stands
down.

`mock-pass.mjs` runs the pass in parts, one `views.mjs` per part, side by side against the one server: each walk
(`light`, `dark`, `light-github` and `dark-github`) with its fixture path, and `rest`, the parts behind the walks (the
bare theme's measures and the browser that shows scrollbars). The mock's story lives in each page, so no part reads
another's writes. A walk's process logs in on the screen drawn in that walk's theme and palette, the `rest` process
logs in on a screen no measure reads, and every process writes its shots under the one `PHOEBIS_SCREENSHOT_DIR` and
closes with a line naming the parts it ran. The pass prints each process's output whole, one part after the other in
that order, and fails where one of them fails. `PHOEBIS_E2E_WALKS` narrows it to a comma-separated list of those words,
so a measure that every walk runs can be proved on one walk:

```sh
PHOEBIS_E2E_WALKS=light PHOEBIS_SCREENSHOT_DIR=/tmp/phoebis-mock-shots npm run e2e:mock
```

Unset or empty, the variable names every part, and a word that names no part stops the pass before a browser starts.
`views.mjs` reads the same variable and runs the parts it names in one process, every part when it is unset. The pass
against a running router keeps that one process, since its walks share the router's registry, where one walk's writes
would reach another's reads.

## Compare the computed styles

```sh
npm run e2e:styles -- --baseline /tmp/phoebis-styles/baseline.json --write
npm run e2e:styles -- --baseline /tmp/phoebis-styles/baseline.json
```

`e2e:styles` builds the mock bundle and runs `styles.mjs`, which reads the computed style of every element the console
draws in a set of states and compares it with a baseline the same script wrote. It checks a change that has to draw the
same pixels, such as a rule section moved from one stylesheet to another or a palette's token set that must match the
other's: the baseline is written before the change and compared after it. A comparison that finds nothing prints
`styles: no difference` and exits 0. Otherwise it prints each differing state with its count, then the first twenty
differences (the state, the element's path, the property, the baseline's value and this run's), writes the whole list
to `differences.txt` beside the baseline, names that file on its last line and exits 1.

The script serves the bundle, signs in with the mock's admin secret and sets the page up as the mock pass does: the
clock paused at `2026-09-16T14:30:00Z` and set back there before each state loads, the time zone UTC, animations and
transitions cut. It walks the states in the light and the dark theme, each under the default and the GitHub palette,
with `phoebis.palette` written beside `phoebis.theme`. The views of `VIEWS` are read at rest at 360, 375, 400, 720 and
1280 px wide. The routing log filtered on five kinds is read at 360 and 1280, since no view at rest draws a filter chip
and this one's values cut at the chip's width. The open surfaces are read at 1280: Add filter on the request log, the
Sort control on Accounts, a Sessions row's menu, a routing log decision's strip popover, a shedding round's open row
with its pins (and the rounds' list at rest), the pool chart's tooltip and the attempt trail's hover card. The Create
guardrail, Create key and Create pool dialogs are read at 720, where a grouped dialog's rail turns into two strips, and
at 1280. The pool dialog is read on its Members group, whose unchecked rows hold disabled fields. A state whose opener
the page lacks, or whose surface never appears, is skipped with a printed note. A write that skipped a state lists the
skipped states, stops with exit 2 and leaves the baseline untouched, and a comparison lists such a state as read in the
baseline alone. A page that fails mid-walk, such as a load or a click that times out, stops the run with exit 2 and
names the state. A server or a browser that does not start, a baseline line that is no state and a file the run cannot
read or finish writing stop it with exit 2 too, leaving no partial baseline or list behind. `views.mjs` runs its pass on
import, so `styles.mjs` keeps its own copy of the views' paths and waits, and a view added to `VIEWS` reaches no state
until its row joins that copy.

The baseline holds one JSON object per line, one line per state:
`{"key":"<theme>/<palette>/<state>@<width>","elements":{...}}`. Each element is keyed by its path from the document
element down, each step its tag, its classes and its `nth-of-type` index, and holds the properties whose computed value
differs from its parent element's, while the document element holds every property. A value an element shares with its
parent adds nothing to the record, and a change shows on the topmost element whose value changed, so the bound loses no
difference and keeps the file to about 270 MB. The script writes the file a state at a time and reads it a line at a
time. It compares the states, the elements and each property's value as sets, so a state or an element present on one
side alone is a difference, and a property an element's entry leaves out reads as its parent's value. A walk that
yields one path for two elements stops with exit 2, since the second element would overwrite the first in both runs and
hide a change on it. The baseline is taken from one tree on one host and only the runs around a change read it, so it
lives outside the repository and is never committed: the script refuses a `--baseline` whose directory lies inside the
repository with exit 2, as the pass refuses a screenshot directory there. Keep it beside the screenshot directory
rather than inside it, so clearing the shots between passes leaves it in place.

`--baseline <path>` is required and names the file to compare against or to write. `--write` writes the file, creating
its directory, and always covers both palettes, so it takes no `--palette`. `--palette default` or `--palette github`
limits a comparison to that palette's states, for a change that moves one palette and must leave the other alone. Any
other word exits 2, since a filter that matches no state would compare two empty sets and report no difference.

The script's exclusion list is empty, since two runs over one tree read every property alike. A property that two runs
read apart joins the list with its reason. Under the cut, every animation and transition reads a duration and a delay of
0s, so a change to either draws no difference. The read covers elements and leaves out every pseudo-element, such as
`::before`, `::after`, `::backdrop` and `::placeholder`, so a rule whose declarations reach a pseudo-element alone draws
no difference.

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
