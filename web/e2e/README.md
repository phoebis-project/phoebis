# Browser pass

`views.mjs` drives a headless Chromium over every view of the console in the light and the dark theme. It logs in
through the login screen, then for each theme visits the dashboard, observe, quota, accounts (which lands on the first
account through a client-side redirect, since `/accounts` carries no account id), pools, keys, request logs,
analytics, sessions, alerts, and settings. Per view it waits for one element, asserts the document title, records every
console error and page error, and writes a full-page screenshot to `$PHOEBIS_SCREENSHOT_DIR/<theme>/<view>.png`. Further
layout assertions ride along, one function each in `views.mjs`, in the order the file defines them (the rail's two
levels (the indent, the gaps, the hairlines, a parent's color at rest and under the pointer), the create marks with and
without the refusal dots (a mark's size, weight and ink, 6 px after its label on the label's line and short of the dot,
at the pass's viewport and in the 720 px strip), the expert view's discovery line (below the group's last field, in the
hint's size and a rail parent's ink), the cell grid through an expert field's wrapper (a half cell beside the plain one,
a lone field the grid's width), the sidebar's Expert view row above the version line, pool chips, the caching controls,
the select-below threshold, the load groups, the pool card's serving table and tiles, the compatibility card, the
bootstrap cards, the Delete account button, the account lines, the alert rule dialog, the remaining cards, the error
state, the cost-cap rows, the live feed, the wasted-probe rows, the burn cards, the plan legend's model unlisted entry,
the placement and budget tables, the speed filter, the traffic cards, the fast lane, the request
drawer, which opens once the request list's shot is taken, the subscription end editor, the catalog popup, the key
dialog's cap field and selects, the Pool select against the Status select, the cost reference, the agent lines,
the fork link, which is followed once the session list's shot is taken). A check that opens an
editor, a dialog or a row closes it again before the view's shot.
The script exits non-zero with the list of failures when any of that does not hold.

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

`e2e:mock` builds the console with the fixture-backed mock into `dist-mock/` (`npm run build:mock`), serves that bundle
from a static server on a loopback port, and runs `mock-pass.mjs`, which drives `views.mjs` against it with the mock's
admin secret and the clock pinned at `2026-09-16T14:30:00Z`, the weekday afternoon the smoke run pins. The mock's random
draws are seeded, so two runs write the same screenshots byte for byte, which `diff -r` over the two directories shows.
Beside the views' shots it writes the open catalog popup's as `<theme>/accounts-catalog.png`; the router's pass writes
one shot per view, which its test counts. The dashboard's remaining cards and its empty state are checked against the
router's seeded registry alone (`PHOEBIS_E2E_MOCK=1` skips them), since the mock's story has subscription accounts and
no empty list on the dashboard. The subscription end editor needs the story's OAuth account, so against the router's
seed that part stands down.

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
