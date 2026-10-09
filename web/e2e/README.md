# Browser pass

`views.mjs` drives a headless Chromium over every view of the console in the light and the dark theme. It logs in
through the login screen, then for each theme visits the dashboard, observe, quota, accounts (which lands on the first
account through a client-side redirect, since `/accounts` carries no account id), pools, keys, request logs, analytics
(on its Traffic tab), sessions, settings, and the routing log, and in the mock pass the Analytics page once more on its
Cache warming tab and the Quota page once more on its Quota windows section. Per view it waits for one element, asserts
the document title, records every console error and page error, and writes a full-page screenshot to
`$PHOEBIS_SCREENSHOT_DIR/<theme>/<view>.png`. Further layout assertions ride along, one function each in `views.mjs`, in
the order the file defines them (the rail's two levels (the indent, the gaps, the hairlines, a parent's color at rest
and under the pointer), the create marks with and without the refusal dots (a mark's size, weight and ink, 6 px after
its label on the label's line and short of the dot, at the pass's viewport and in the 720 px strip), the expert view's
discovery line (below the group's last field, in the hint's size and a rail parent's ink), the cell grid through an
expert field's wrapper (a half cell beside the plain one, a lone field the grid's width), the sidebar's brand row (the
mark's box 36 px square with the logo drawn 26 px square in it, the name 17 px at weight 600 on one line and inside the
sidebar's content box), the dashboard's attention line (one 22 px row per line of badges, each badge the pool card's
strip badge, in the content box and clear of the tiles; the story's nine items in the mock pass, the seeded exhaustion
or the quiet line in the router pass), the sidebar's Expert view row above the version line, the routing log's columns
at 1280 by 800 and 1440 by 900 (Fact 150 px at both, Account 246 then 256 px, Model at 1440 alone, the table its card's
width with nothing to scroll sideways), the Diagnostics group (neither its heading nor the Routing log link on the
dashboard with the expert view off, both with the log open and the link active), pool chips, the caching controls, the
select-below threshold, the load groups, the pool card's serving table and tiles, the serving table's columns (Account
the fill), the pool dialog's members table (Account the fill, each Priority and budget input at least its column's 96,
140 or 150 px less the cell's padding) and keys table (Key the fill), each read while its group shows, and in the mock
pass's expert view its model rules table (Model the fill, each select holding its widest option's text inside its
padding), which the router's seed draws no override for, the settings cards' order, the listener hint's gaps and size
and About's lines (their order, one type and one gap), the cooling-down block's columns (Reason the fill), the Delete
account button, the account lines, the Quota cards' poll lines (short of the Refresh pill and inside the card, the
backoff line in the serious ink, none wrapping, a failed poll's reason cut inside its line and whole in its title), the
Quota cards' chips and reset cells (each on one line inside its column, the window grid inside its card) and each live
window's pace mark (inside its meter and taller than the track), the timeline's chips on the Quota windows section (each
inside its column, a chip with a share in the background and ink of the card chip of the same tone, primary's used fill
at the share the page computed, and the lane label's ink at 4.5:1 or better over each tone's fill), the Quota cards' one
height (the head's and the foot's rules at one offset on every card), the remaining cards, the error state, the lines
under the quota rings, the recent requests' model cells and their table's fit, the recent requests' columns (Key the
fill), the cost-cap rows, the live feed, the wasted-probe rows, the burn cards, the plan legend's model unlisted entry,
the placement and budget tables, the Analytics tables' columns, each read on the tab that draws it (the Traffic
breakdown's group column the fill, Key on the misses-by-reason breakdown under its wrapped headers, Rung on the
placements table, Pool and Account on the member budgets, whose Spent column is as wide as its meter's 140 px floor),
the request table's fit, the request pages' widths, walked on a load of their own once every view's shot is taken, the
speed filter, the traffic cards (each Analytics check opens the tab it reads and returns to
Traffic), the tab strip docked under the filter bar at the end of the Latency tab, the fast lane, the request drawer,
which opens once the request list's shot is taken, the routing log's decision card, which opens once the log's shot is
taken (the walk and the candidate table, 958 then 1118 px, inside the card at 1280 by 800 and 1440 by 900, every verdict
badge in the kind badge's box, and the chosen row's Prefix chips on the first rung placement the log lists), the
subscription end editor, the catalog popup and its columns (Model and Name the fills), the Keys table's columns (Name,
Models and Status the fills) and the badges of its fill cells (each ending inside its cell and cutting its own label
with the ellipsis), the Models rules table's columns (Pattern the fill), the key dialog's Access selects, the Pool
select against the Status select, the Sessions lead and its disclosure at 1280 by 800 and 1440 by 900 (the head's height
closed and open, the button on the sentence's last line or flush left on the next, the opened paragraph 6 px under the
lead, Cost reference one line at the head's top right), the cost reference, the session table's fit and its columns
(each fit column as wide as its content, the Session column the fill), the session id's width, the session row menu, the
agent lines, the fork badges (inside their cell and whole),
the fork link, which is followed once the session list's shot is taken, the copy fallback (Copied
on the Settings snippet with the clipboard API removed, on the Settings view's own load, and on a Key created dialog's
secret in the mock pass, on a load of its own)). A check that opens an editor, a dialog or a row closes it again before
the view's shot, and one that turns a column on turns it off again. The script exits non-zero with the list of failures
when any of that does not hold.

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
itself, when `PHOEBIS_E2E_BROWSER=1` is set; without it the test prints a skip line, so a build host without Chromium
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
and the Analytics page's Burn tab, its plan chart shading the peak band, as `<theme>/analytics-burn.png`. Its views
include the Analytics page's Cache warming tab as `<theme>/analytics-cache-warming.png` and the Quota page's Quota
windows section as `<theme>/quota-windows.png`. The router's pass writes none of these: one shot per view it runs, which
its test counts. The dashboard's remaining cards and its empty state are checked against the router's seeded registry
alone (`PHOEBIS_E2E_MOCK=1` skips them), since the mock's story has subscription accounts and no empty list on the
dashboard. The subscription end editor needs the story's OAuth account, so against the router's seed that part stands
down.

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
