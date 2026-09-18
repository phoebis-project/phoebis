# Browser pass

`views.mjs` drives a headless Chromium over every view of the console in the light and the dark theme. It logs in
through the login screen, then for each theme visits the dashboard, observe, quota, upstreams (which lands on the
first upstream through a client-side redirect, since `/upstreams` carries no account id), pools, API keys, request
logs, analytics, sessions, alerts, and settings. Per view it waits for one element, asserts the document title,
records every console error and page error, and writes a full-page screenshot to
`$PHOEBIS_SCREENSHOT_DIR/<theme>/<view>.png`. Two extra assertions ride along: every chip on a pool card is the size of
the chip in the top bar, and each reason label in the wasted-probes card lies inside its own row. The script exits
non-zero with the list of failures when any of that does not hold.

The screenshot directory has to lie outside the repository, which the script enforces against every worktree `git
worktree list` reports, and against the tree holding the nearest `Cargo.toml` when git cannot answer.

## Run it against a running router

```sh
npx playwright install chromium
PHOEBIS_E2E_BASE=http://127.0.0.1:8081 PHOEBIS_E2E_SECRET=... PHOEBIS_SCREENSHOT_DIR=/tmp/phoebis-shots npm run e2e
```

`PHOEBIS_E2E_BASE` is the admin listener's URL and `PHOEBIS_E2E_SECRET` the admin secret. The browser install puts about
150 MB of Chromium under `~/.cache/ms-playwright` and is needed once per host.

`cargo test -p phoebis-router --test end_to_end the_ui_passes` runs the same script against a router it starts and seeds
itself, when `PHOEBIS_E2E_BROWSER=1` is set; without it the test prints a skip line, so a build host without Chromium
still runs the rest of that suite.

## On a host without Chromium

```sh
docker run --rm --network host -v "$PWD":/work -w /work \
  -e PHOEBIS_E2E_BASE -e PHOEBIS_E2E_SECRET -e PHOEBIS_SCREENSHOT_DIR=/shots -v /tmp/phoebis-shots:/shots \
  mcr.microsoft.com/playwright:v1.49.0-noble npm run e2e
```

The image's bundled Playwright is older than the lockfile's, so `npm ci` inside the container installs the pinned one
over it: the image is a Chromium host and nothing more. The test suite uses the host's own Chromium instead, because
Chromium runs headless here and the image would add 3.5 GB to the test path for no gain.
